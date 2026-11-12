//! Custom seccomp profiles (`--security-opt seccomp=<profile.json>`), read
//! the way Docker and runc read them. The agent compiles the profile before
//! the clone, so a bad one fails the start with its reason; the child only
//! installs the finished program.
//!
//! The program is classic BPF for this machine's own syscalls. No other ABI
//! gets rules: the host boundary underneath ([`super::seccomp`]) refuses
//! them to every container, so a profile's entries for other architectures
//! have nothing to act on, as on a kernel that cannot run them.

use std::collections::BTreeMap;
use std::io;

use serde_json::Value;

use super::syscall_table;

const LD_W_ABS: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
const AND_K: u16 = (libc::BPF_ALU | libc::BPF_AND | libc::BPF_K) as u16;
const JA: u16 = (libc::BPF_JMP | libc::BPF_JA) as u16;
const JEQ_K: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
const JGT_K: u16 = (libc::BPF_JMP | libc::BPF_JGT | libc::BPF_K) as u16;
const JGE_K: u16 = (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16;
const RET_K: u16 = (libc::BPF_RET | libc::BPF_K) as u16;

const RET_KILL: u32 = libc::SECCOMP_RET_KILL_THREAD;
const RET_ERRNO: u32 = libc::SECCOMP_RET_ERRNO;
const RET_TRACE: u32 = libc::SECCOMP_RET_TRACE;
const RET_LOG: u32 = libc::SECCOMP_RET_LOG;
const RET_ALLOW: u32 = libc::SECCOMP_RET_ALLOW;

/// seccomp_data offsets: the syscall number, its architecture, and the
/// first argument. Each argument is 64 bits wide.
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_ARGS: u32 = 16;

/// x86-64's x32 ABI shares its architecture value and sets this bit in the
/// syscall number instead.
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

const MAX_INSNS: usize = libc::BPF_MAXINSNS as usize;

/// From here on the kernel has no errno to return.
const MAX_ERRNO: u64 = 4095;

#[cfg(target_arch = "aarch64")]
const NATIVE: (&str, &str, u32) = ("SCMP_ARCH_AARCH64", "arm64", 0xC000_00B7);
#[cfg(target_arch = "x86_64")]
const NATIVE: (&str, &str, u32) = ("SCMP_ARCH_X86_64", "amd64", 0xC000_003E);

/// The architectures Docker takes by name, and whether they are big-endian.
/// One filter cannot hold both byte orders, so naming a big-endian one
/// fails the start under Docker too.
const ARCHES: &[(&str, bool)] = &[
    ("X86", false), ("X86_64", false), ("X32", false), ("ARM", false), ("AARCH64", false), ("MIPS", true), ("MIPS64", true),
    ("MIPS64N32", true), ("MIPSEL", false), ("MIPSEL64", false), ("MIPSEL64N32", false), ("PPC", true), ("PPC64", true),
    ("PPC64LE", false), ("RISCV64", false), ("S390", true), ("S390X", true),
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Ne,
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
    MaskedEq,
}

/// A condition on one syscall argument. For `MaskedEq`, `value` is the mask
/// and `two` what the masked argument has to equal.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Cond {
    index: u32,
    op: Op,
    value: u64,
    two: u64,
}

/// What a syscall gets when all of `conds` hold.
struct Rule {
    conds: Vec<Cond>,
    action: u32,
}

/// A compiled profile, ready for the child to install.
pub struct Filter {
    insns: Vec<libc::sock_filter>,
    flags: libc::c_ulong,
}

fn bad(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("seccomp profile: {}", message.to_string()))
}

static ABSENT: Value = Value::Null;

/// A key as Docker compares it with a field's name: without regard to case,
/// which to its JSON decoder also makes U+017F an s and U+212A a k.
fn folded(key: &str) -> impl Iterator<Item = char> + '_ {
    key.chars().map(|c| match c {
        '\u{17f}' => 's',
        '\u{212a}' => 'k',
        _ => c.to_ascii_lowercase(),
    })
}

/// The field `name` of a JSON object, null when it has none. Docker takes a
/// key in any case, so `Syscalls` is the rules here as well and not a field
/// to ignore. Of two spellings in one object Docker takes the later one,
/// which the parsed object no longer tells: that is refused.
fn field<'a>(object: &'a Value, name: &str) -> io::Result<&'a Value> {
    let Some(map) = object.as_object() else { return Ok(&ABSENT) };
    let mut found = map.iter().filter(|(key, _)| folded(key).eq(folded(name)));
    match (found.next(), found.next()) {
        (Some(_), Some(_)) => Err(bad(format!("{name} is given more than once, in different spellings"))),
        (first, _) => Ok(first.map_or(&ABSENT, |(_, value)| value)),
    }
}

fn array<'a>(value: &'a Value, name: &str) -> io::Result<&'a [Value]> {
    match value {
        Value::Null => Ok(&[]),
        Value::Array(items) => Ok(items),
        _ => Err(bad(format!("{name} must be an array"))),
    }
}

fn strings<'a>(value: &'a Value, name: &str) -> io::Result<Vec<&'a str>> {
    array(value, name)?.iter().map(|v| v.as_str().ok_or_else(|| bad(format!("{name} must hold strings")))).collect()
}

/// A string field; absent reads as empty, as it does to Docker.
fn string<'a>(value: &'a Value, name: &str) -> io::Result<&'a str> {
    match value {
        Value::Null => Ok(""),
        Value::String(s) => Ok(s),
        _ => Err(bad(format!("{name} must be a string"))),
    }
}

fn number(value: &Value, name: &str) -> io::Result<Option<u64>> {
    if value.is_null() {
        return Ok(None);
    }
    value.as_u64().map(Some).ok_or_else(|| bad(format!("{name} must be an unsigned integer")))
}

/// `<kernel>.<major>`, the only form Docker takes for `minKernel`.
fn kernel_version(value: &str) -> io::Result<(u32, u32)> {
    let parts = value.split_once('.').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)));
    parts.ok_or_else(|| bad(format!("invalid kernel version: {value:?}, expected \"<kernel>.<major>\"")))
}

/// The running kernel's version from an osrelease string like `6.12.5-asagiri`.
fn running_kernel(release: &str) -> Option<(u32, u32)> {
    let mut parts = release.trim().split(|c: char| !c.is_ascii_digit());
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// The kernel's value for an action. An errno defaults to EPERM, as in runc.
fn action(name: &str, errno: Option<u64>) -> io::Result<u32> {
    let data = errno.unwrap_or(libc::EPERM as u64);
    if data > 0xffff {
        return Err(bad(format!("errnoRet {data} does not fit a seccomp return value")));
    }
    // The kernel has no errno above 4095, and Docker takes none from there on.
    if name == "SCMP_ACT_ERRNO" && data >= MAX_ERRNO {
        return Err(bad(format!("errnoRet {data} is not an errno; the highest is {}", MAX_ERRNO - 1)));
    }
    Ok(match name {
        "SCMP_ACT_KILL" | "SCMP_ACT_KILL_THREAD" => RET_KILL,
        "SCMP_ACT_KILL_PROCESS" => libc::SECCOMP_RET_KILL_PROCESS,
        "SCMP_ACT_TRAP" => libc::SECCOMP_RET_TRAP,
        "SCMP_ACT_ERRNO" => RET_ERRNO | data as u32,
        "SCMP_ACT_TRACE" => RET_TRACE | data as u32,
        "SCMP_ACT_LOG" => RET_LOG,
        "SCMP_ACT_ALLOW" => RET_ALLOW,
        // It hands the syscall to a listener process, which the agent has
        // no way to connect: refused rather than left to hang or to pass.
        "SCMP_ACT_NOTIFY" => return Err(bad("SCMP_ACT_NOTIFY needs a notification listener, which is not supported")),
        _ => return Err(bad(format!("string {name} is not a valid action for seccomp"))),
    })
}

/// Checks an architecture a profile names. Only the names are checked: the
/// rules are compiled for this machine's architecture, named or not.
fn arch(name: &str) -> io::Result<()> {
    let known = name.strip_prefix("SCMP_ARCH_").and_then(|short| ARCHES.iter().find(|(known, _)| *known == short));
    match known {
        Some((_, big_endian)) if *big_endian != cfg!(target_endian = "big") => {
            Err(bad(format!("error adding architecture to seccomp filter: {name} has another byte order than this machine")))
        }
        Some(_) => Ok(()),
        None => Err(bad(format!("string {name} is not a valid arch for seccomp"))),
    }
}

/// Capability names by number, as profiles spell them.
const CAPS: &[&str] = &[
    "CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_DAC_READ_SEARCH", "CAP_FOWNER", "CAP_FSETID", "CAP_KILL", "CAP_SETGID", "CAP_SETUID",
    "CAP_SETPCAP", "CAP_LINUX_IMMUTABLE", "CAP_NET_BIND_SERVICE", "CAP_NET_BROADCAST", "CAP_NET_ADMIN", "CAP_NET_RAW", "CAP_IPC_LOCK",
    "CAP_IPC_OWNER", "CAP_SYS_MODULE", "CAP_SYS_RAWIO", "CAP_SYS_CHROOT", "CAP_SYS_PTRACE", "CAP_SYS_PACCT", "CAP_SYS_ADMIN",
    "CAP_SYS_BOOT", "CAP_SYS_NICE", "CAP_SYS_RESOURCE", "CAP_SYS_TIME", "CAP_SYS_TTY_CONFIG", "CAP_MKNOD", "CAP_LEASE",
    "CAP_AUDIT_WRITE", "CAP_AUDIT_CONTROL", "CAP_SETFCAP", "CAP_MAC_OVERRIDE", "CAP_MAC_ADMIN", "CAP_SYSLOG", "CAP_WAKE_ALARM",
    "CAP_BLOCK_SUSPEND", "CAP_AUDIT_READ", "CAP_PERFMON", "CAP_BPF", "CAP_CHECKPOINT_RESTORE",
];

/// Whether a rule applies to this container: Docker's `includes` and
/// `excludes`, by architecture, held capabilities and kernel version. A
/// capability name Docker does not know is one the container does not hold.
fn selected(rule: &Value, caps: u64, kernel: Option<(u32, u32)>) -> io::Result<bool> {
    let holds = |name: &str| CAPS.iter().position(|n| *n == name).is_some_and(|bit| caps & (1u64 << bit) != 0);
    let mut keep = true;
    for (which, exclude) in [("excludes", true), ("includes", false)] {
        let condition = field(rule, which)?;
        if condition.is_null() {
            continue;
        }
        if !condition.is_object() {
            return Err(bad(format!("{which} must be an object")));
        }
        let arches = strings(field(condition, "arches")?, "arches")?;
        let names = strings(field(condition, "caps")?, "caps")?;
        let matched = arches.contains(&NATIVE.1);
        if exclude {
            keep &= !matched && !names.iter().any(|c| holds(c));
        } else {
            keep &= (arches.is_empty() || matched) && names.iter().all(|c| holds(c));
        }
        let min_kernel = field(condition, "minKernel")?;
        if !min_kernel.is_null() {
            let want = kernel_version(string(min_kernel, "minKernel")?)?;
            let have = kernel.ok_or_else(|| bad("cannot tell the running kernel's version for minKernel"))?;
            keep &= (have >= want) != exclude;
        }
    }
    Ok(keep)
}

/// One syscall rule's argument conditions.
fn conditions(rule: &Value) -> io::Result<Vec<Cond>> {
    let mut conds = Vec::new();
    for a in array(field(rule, "args")?, "args")? {
        if !a.is_object() {
            return Err(bad("a syscall argument must be an object"));
        }
        let index = number(field(a, "index")?, "index")?.unwrap_or(0);
        if index > 5 {
            return Err(bad(format!("syscalls only have up to 6 arguments ({index} given)")));
        }
        let op = match string(field(a, "op")?, "op")? {
            "SCMP_CMP_NE" => Op::Ne,
            "SCMP_CMP_LT" => Op::Lt,
            "SCMP_CMP_LE" => Op::Le,
            "SCMP_CMP_EQ" => Op::Eq,
            "SCMP_CMP_GE" => Op::Ge,
            "SCMP_CMP_GT" => Op::Gt,
            "SCMP_CMP_MASKED_EQ" => Op::MaskedEq,
            other => return Err(bad(format!("string {other} is not a valid operator for seccomp"))),
        };
        let value = number(field(a, "value")?, "value")?.unwrap_or(0);
        let two = number(field(a, "valueTwo")?, "valueTwo")?.unwrap_or(0);
        // Only the masked comparison has a second value, and under Docker
        // the mask applies to it as well.
        conds.push(Cond { index: index as u32, op, value, two: if op == Op::MaskedEq { two & value } else { 0 } });
    }
    Ok(conds)
}

/// The flags for the seccomp call. With none given runc turns off the
/// speculation mitigation, and so does the default profile here.
fn filter_flags(profile: &Value) -> io::Result<libc::c_ulong> {
    let names = strings(field(profile, "flags")?, "flags")?;
    if names.is_empty() {
        return Ok(libc::SECCOMP_FILTER_FLAG_SPEC_ALLOW);
    }
    let mut flags = 0;
    for name in names {
        flags |= match name {
            "SECCOMP_FILTER_FLAG_TSYNC" => libc::SECCOMP_FILTER_FLAG_TSYNC,
            "SECCOMP_FILTER_FLAG_LOG" => libc::SECCOMP_FILTER_FLAG_LOG,
            "SECCOMP_FILTER_FLAG_SPEC_ALLOW" => libc::SECCOMP_FILTER_FLAG_SPEC_ALLOW,
            // Only means something next to a notification listener.
            "SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV" => return Err(bad(format!("flag {name} needs a notification listener, which is not supported"))),
            _ => return Err(bad(format!("seccomp flag {name} is not known"))),
        };
    }
    Ok(flags)
}

const fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt: 0, jf: 0, k }
}

const fn ret(action: u32) -> libc::sock_filter {
    stmt(RET_K, action)
}

/// `then` if the comparison of the loaded word with `k` holds, `otherwise`
/// if not. Both end in a return on every path, so nothing has to jump back
/// out of them.
fn branch(code: u16, k: u32, mut then: Vec<libc::sock_filter>, otherwise: Vec<libc::sock_filter>) -> Vec<libc::sock_filter> {
    let mut out = Vec::with_capacity(then.len() + otherwise.len() + 2);
    match u8::try_from(then.len()) {
        Ok(skip) => out.push(libc::sock_filter { code, jt: 0, jf: skip, k }),
        // A conditional jump reaches 255 instructions; an unconditional one
        // goes the rest of the way.
        Err(_) => out.extend([libc::sock_filter { code, jt: 1, jf: 0, k }, stmt(JA, then.len() as u32)]),
    }
    out.append(&mut then);
    out.extend(otherwise);
    out
}

/// Where a comparison leads: to the next instruction, past its condition
/// (which then holds), or past its rule (which then does not apply).
#[derive(Clone, Copy)]
enum To {
    Next,
    Pass,
    Fail,
}

/// The instructions that test one condition. BPF compares 32 bits at a
/// time, so the argument is read as two halves, the high one first.
fn condition(c: &Cond) -> Vec<(u16, u32, To, To)> {
    use To::{Fail, Next, Pass};
    let low_first = cfg!(target_endian = "little");
    let at = OFF_ARGS + 8 * c.index;
    let (hi_at, lo_at) = if low_first { (at + 4, at) } else { (at, at + 4) };
    let (hi, lo) = ((c.value >> 32) as u32, c.value as u32);
    let load = |at| (LD_W_ABS, at, Next, Next);
    let equal = |pass, fail| vec![load(hi_at), (JEQ_K, hi, Next, fail), load(lo_at), (JEQ_K, lo, pass, fail)];
    // A greater high half settles it, a smaller one too; on a tie the low
    // halves decide, with `last` saying whether equal counts.
    let greater = |last, pass, fail| vec![load(hi_at), (JGT_K, hi, pass, Next), (JEQ_K, hi, Next, fail), load(lo_at), (last, lo, pass, fail)];
    match c.op {
        Op::Eq => equal(Pass, Fail),
        Op::Ne => equal(Fail, Pass),
        Op::Gt => greater(JGT_K, Pass, Fail),
        Op::Ge => greater(JGE_K, Pass, Fail),
        Op::Le => greater(JGT_K, Fail, Pass),
        Op::Lt => greater(JGE_K, Fail, Pass),
        Op::MaskedEq => vec![
            load(hi_at),
            (AND_K, hi, Next, Next),
            (JEQ_K, (c.two >> 32) as u32, Next, Fail),
            load(lo_at),
            (AND_K, lo, Next, Next),
            (JEQ_K, c.two as u32, Pass, Fail),
        ],
    }
}

/// One rule: its action if every condition holds; otherwise control leaves
/// by the end, where the syscall's next rule starts.
fn checks(rule: &Rule) -> Vec<libc::sock_filter> {
    let parts: Vec<_> = rule.conds.iter().map(condition).collect();
    // At most six conditions of six instructions: every jump is a short one.
    let end = parts.iter().map(Vec::len).sum::<usize>() + 1;
    let mut out = Vec::with_capacity(end);
    for part in &parts {
        let passed = out.len() + part.len();
        for &(code, k, jt, jf) in part {
            let from = out.len() + 1;
            let offset = |to| match to {
                To::Next => 0,
                To::Pass => (passed - from) as u8,
                To::Fail => (end - from) as u8,
            };
            out.push(libc::sock_filter { code, jt: offset(jt), jf: offset(jf), k });
        }
    }
    out.push(ret(rule.action));
    out
}

/// Adjacent syscall numbers that end the same way.
struct Span {
    first: u32,
    last: u32,
    /// The action all of them get, when it takes no look at the arguments.
    always: Option<u32>,
    body: Vec<libc::sock_filter>,
}

/// Finds the loaded syscall number among `spans`, which are in ascending
/// order, by halving: a long profile costs a syscall a dozen comparisons
/// instead of hundreds.
fn dispatch(spans: &[Span], default: u32) -> Vec<libc::sock_filter> {
    match spans {
        [] => vec![ret(default)],
        [span] if span.first == span.last => branch(JEQ_K, span.first, span.body.clone(), vec![ret(default)]),
        [span] => branch(JGE_K, span.first, branch(JGT_K, span.last, vec![ret(default)], span.body.clone()), vec![ret(default)]),
        _ => {
            let (low, high) = spans.split_at(spans.len() / 2);
            branch(JGE_K, high[0].first, dispatch(high, default), dispatch(low, default))
        }
    }
}

/// The whole filter. `arch` and `x32` tell this machine's syscalls from
/// another ABI's; `newest` is the highest syscall number the profile names,
/// when syscalls above it are to fail with ENOSYS.
fn program(arch: u32, x32: bool, default: u32, newest: Option<u32>, table: &BTreeMap<u32, Vec<Rule>>) -> Vec<libc::sock_filter> {
    let mut spans: Vec<Span> = Vec::new();
    for (&nr, rules) in table {
        let always = match rules.as_slice() {
            [only] if only.conds.is_empty() => Some(only.action),
            _ => None,
        };
        match spans.last_mut() {
            Some(span) if always.is_some() && span.always == always && span.last + 1 == nr => span.last = nr,
            _ => {
                let body = match always {
                    Some(action) => vec![ret(action)],
                    None => rules.iter().flat_map(checks).chain([ret(default)]).collect(),
                };
                spans.push(Span { first: nr, last: nr, always, body });
            }
        }
    }
    let mut rules = dispatch(&spans, default);
    if let Some(newest) = newest {
        // What runc puts in front of a profile that does not allow by
        // default: a libc trying a syscall the profile's author never saw
        // (clone3, faccessat2) then falls back to the older one.
        rules = branch(JGT_K, newest, vec![ret(RET_ERRNO | libc::ENOSYS as u32)], rules);
    }
    // The architecture comes first: a number means another syscall in every
    // ABI. One the filter has no rules for is killed, as under Docker.
    let mut out = vec![stmt(LD_W_ABS, OFF_ARCH), libc::sock_filter { code: JEQ_K, jt: 1, jf: 0, k: arch }, ret(RET_KILL), stmt(LD_W_ABS, OFF_NR)];
    if x32 {
        out.extend([libc::sock_filter { code: JGE_K, jt: 0, jf: 1, k: X32_SYSCALL_BIT }, ret(RET_KILL)]);
    }
    out.extend(rules);
    out
}

/// Adds a rule to those of its syscall, settling overlaps as Docker does: a
/// rule without conditions decides its syscall alone, the first such in the
/// profile, and the same conditions cannot lead to two actions. The rest
/// apply in the profile's order. False for such a clash.
fn add(rules: &mut Vec<Rule>, new: Rule) -> bool {
    if rules.first().is_some_and(|first| first.conds.is_empty()) {
        return true;
    }
    if new.conds.is_empty() {
        rules.clear();
    }
    match rules.iter().find(|rule| rule.conds == new.conds) {
        Some(same) => same.action == new.action,
        None => {
            rules.push(new);
            true
        }
    }
}

/// Compiles a profile for a container that holds `caps`. `None` is a profile
/// with no default action and no rules, which to Docker means no filter.
pub fn compile(json: &str, caps: u64) -> io::Result<Option<Filter>> {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    compile_for(json, caps, running_kernel(&release))
}

fn compile_for(json: &str, caps: u64, kernel: Option<(u32, u32)>) -> io::Result<Option<Filter>> {
    let profile: Value = serde_json::from_str(json).map_err(bad)?;
    if !profile.is_object() {
        return Err(bad("must be a JSON object"));
    }
    let rules = array(field(&profile, "syscalls")?, "syscalls")?;
    let default_name = string(field(&profile, "defaultAction")?, "defaultAction")?;
    if default_name.is_empty() && rules.is_empty() {
        return Ok(None);
    }
    let default = action(default_name, number(field(&profile, "defaultErrnoRet")?, "defaultErrnoRet")?)?;
    let flags = filter_flags(&profile)?;

    let mut arches = strings(field(&profile, "architectures")?, "architectures")?;
    let arch_map = array(field(&profile, "archMap")?, "archMap")?;
    if !arches.is_empty() && !arch_map.is_empty() {
        return Err(bad("both 'architectures' and 'archMap' are specified in the seccomp profile, use either 'architectures' or 'archMap'"));
    }
    for mapping in arch_map {
        if !mapping.is_object() {
            return Err(bad("an archMap entry must be an object"));
        }
        // Only this machine's entry counts: itself and its sub-architectures.
        if string(field(mapping, "architecture")?, "architecture")? == NATIVE.0 {
            arches.push(NATIVE.0);
            arches.extend(strings(field(mapping, "subArchitectures")?, "subArchitectures")?);
        }
    }
    for name in arches {
        arch(name)?;
    }

    let mut table: BTreeMap<u32, Vec<Rule>> = BTreeMap::new();
    let mut newest: Option<u32> = None;
    let mut count = 0;
    for rule in rules {
        if !rule.is_object() {
            return Err(bad("a syscall rule must be an object"));
        }
        let act = action(string(field(rule, "action")?, "action")?, number(field(rule, "errnoRet")?, "errnoRet")?)?;
        let mut conds = conditions(rule)?;
        let mut names = strings(field(rule, "names")?, "names")?;
        let name = string(field(rule, "name")?, "name")?;
        if !name.is_empty() {
            if !names.is_empty() {
                return Err(bad("both 'name' and 'names' are specified in the seccomp profile, use either 'name' or 'names'"));
            }
            names.push(name);
        }
        if !selected(rule, caps, kernel)? {
            continue;
        }
        // Conditions on different arguments must all hold. As soon as two
        // are on one argument, runc makes each a rule of its own, so that
        // any of them may match.
        let repeated = (0..6).any(|i| conds.iter().filter(|c| c.index == i).count() > 1);
        conds.sort_by_key(|c| c.index);
        let groups: Vec<Vec<Cond>> = if repeated { conds.iter().map(|c| vec![*c]).collect() } else { vec![conds] };
        for name in names {
            if name.is_empty() {
                return Err(bad("empty string is not a valid syscall"));
            }
            // Profiles name syscalls of every architecture and kernel: one
            // unknown here is skipped, as runc does.
            let Some(nr) = syscall_table::number(name) else { continue };
            newest = newest.max(Some(nr));
            // Same as the default: nothing to add.
            if act == default {
                continue;
            }
            for conds in &groups {
                let listed = table.entry(nr).or_default();
                count -= listed.len();
                if !add(listed, Rule { conds: conds.clone(), action: act }) {
                    return Err(bad(format!("error adding seccomp rule for syscall {name}: the same conditions already have another action")));
                }
                count += listed.len();
                // Every rule is at least one instruction, and all but one
                // per syscall are five or more. Past this the program cannot
                // fit, so it is not worth building to find out by how much.
                if count > MAX_INSNS {
                    return Err(bad(format!("compiles to more than {MAX_INSNS} instructions; the kernel takes up to {MAX_INSNS}")));
                }
            }
        }
    }

    let permissive = matches!(default & libc::SECCOMP_RET_ACTION_FULL, RET_ALLOW | RET_LOG | RET_TRACE) || default == RET_ERRNO | libc::ENOSYS as u32;
    let insns = program(NATIVE.2, cfg!(target_arch = "x86_64"), default, newest.filter(|_| !permissive), &table);
    if insns.len() > MAX_INSNS {
        return Err(bad(format!("compiles to {} instructions; the kernel takes up to {MAX_INSNS}", insns.len())));
    }
    Ok(Some(Filter { insns, flags }))
}

impl Filter {
    /// Installs the filter on this process; what it execs inherits it. Needs
    /// CAP_SYS_ADMIN or no_new_privs, like the default filter.
    pub fn install(&self) -> io::Result<()> {
        let prog = libc::sock_fprog { len: self.insns.len() as u16, filter: self.insns.as_ptr() as *mut _ };
        // SAFETY: a valid sock_fprog whose instructions outlive the call.
        let rc = unsafe { libc::syscall(libc::SYS_seccomp, 1 /* SECCOMP_SET_MODE_FILTER */, self.flags, &prog as *const libc::sock_fprog) };
        match rc {
            0 => Ok(()),
            // With TSYNC, the thread that could not be synchronized.
            n if n > 0 => Err(io::Error::other("cannot synchronize the filter across threads")),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const KERNEL: Option<(u32, u32)> = Some((6, 12));

    fn compiled(profile: Value) -> io::Result<Option<Filter>> {
        compile_for(&profile.to_string(), 0, KERNEL)
    }

    fn rule(field: &str, value: Value) -> Value {
        let mut rule = json!({"names": ["getppid"], "action": "SCMP_ACT_ERRNO"});
        rule[field] = value;
        json!({"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [rule]})
    }

    /// Runs `probe` in a forked child under the host boundary and `filter`;
    /// true when it returned true. The test runner keeps neither filter.
    fn enforced(filter: &Filter, probe: fn() -> bool) -> bool {
        // SAFETY: the child only installs filters, makes the probe's
        // syscalls and exits without returning into the test framework.
        unsafe {
            let pid = libc::fork();
            assert!(pid >= 0);
            if pid == 0 {
                libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
                let installed = super::super::seccomp::install(None).is_ok() && filter.install().is_ok();
                libc::_exit(if !installed { 2 } else if probe() { 0 } else { 1 });
            }
            let mut status = 0;
            assert_eq!(libc::waitpid(pid, &mut status, 0), pid);
            assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) < 2, "child status: {status}");
            libc::WEXITSTATUS(status) == 0
        }
    }

    fn errno_of(rc: libc::c_long) -> i32 {
        if rc == -1 { io::Error::last_os_error().raw_os_error().unwrap_or(0) } else { 0 }
    }

    const ERRNO: u32 = RET_ERRNO | libc::EPERM as u32;

    /// What a program returns for one syscall, stepping through it as the
    /// kernel does. A jump out of the program, or an instruction seccomp
    /// would not take, fails the test.
    fn verdict(program: &[libc::sock_filter], arch: u32, nr: u32, args: [u64; 6]) -> u32 {
        let mut data = Vec::with_capacity(64);
        data.extend(nr.to_ne_bytes());
        data.extend(arch.to_ne_bytes());
        data.extend(0u64.to_ne_bytes());
        args.iter().for_each(|arg| data.extend(arg.to_ne_bytes()));
        let (mut loaded, mut at) = (0u32, 0);
        loop {
            let insn = program[at];
            at += 1;
            match insn.code {
                LD_W_ABS => loaded = u32::from_ne_bytes(data[insn.k as usize..insn.k as usize + 4].try_into().unwrap()),
                AND_K => loaded &= insn.k,
                JA => at += insn.k as usize,
                JEQ_K | JGT_K | JGE_K => {
                    let holds = match insn.code {
                        JEQ_K => loaded == insn.k,
                        JGT_K => loaded > insn.k,
                        _ => loaded >= insn.k,
                    };
                    at += usize::from(if holds { insn.jt } else { insn.jf });
                }
                RET_K => return insn.k,
                other => panic!("instruction {other:#x} at {}", at - 1),
            }
        }
    }

    /// The program for `rules` (syscall number, conditions, action) over a
    /// default of ALLOW, on an ABI of this test's own.
    fn built(rules: Vec<(u32, Vec<Cond>, u32)>, newest: Option<u32>, x32: bool) -> Vec<libc::sock_filter> {
        let mut table: BTreeMap<u32, Vec<Rule>> = BTreeMap::new();
        for (nr, conds, action) in rules {
            assert!(add(table.entry(nr).or_default(), Rule { conds, action }));
        }
        program(7, x32, RET_ALLOW, newest, &table)
    }

    fn cond(index: u32, op: Op, value: u64, two: u64) -> Cond {
        Cond { index, op, value, two }
    }

    #[test]
    fn an_empty_profile_is_no_filter() {
        assert!(compiled(json!({})).unwrap().is_none());
        assert!(compiled(json!({"syscalls": [], "comment": "none"})).unwrap().is_none());
        // Rules without a default action are a mistake, not an empty profile.
        assert!(compiled(json!({"syscalls": [{"names": ["getppid"], "action": "SCMP_ACT_ERRNO"}]})).is_err());
        assert!(compiled(json!({"defaultAction": "SCMP_ACT_ALLOW"})).unwrap().is_some());
    }

    #[test]
    fn invalid_profiles_are_refused() {
        for profile in [
            json!([]),
            json!({"defaultAction": "SCMP_ACT_BOGUS"}),
            json!({"defaultAction": 1}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "defaultErrnoRet": -1}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "syscalls": {}}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [false]}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "architectures": false}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "architectures": ["SCMP_ARCH_BOGUS"]}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "architectures": ["aarch64"]}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "architectures": ["SCMP_ARCH_NATIVE"]}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "architectures": ["SCMP_ARCH_PARISC"]}),
            // Big-endian: Docker cannot add them to a filter of this machine either.
            json!({"defaultAction": "SCMP_ACT_ALLOW", "architectures": [NATIVE.0, "SCMP_ARCH_S390X"]}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "archMap": [{"architecture": NATIVE.0, "subArchitectures": ["SCMP_ARCH_PPC64"]}]}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "archMap": [false]}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "architectures": [NATIVE.0], "archMap": [{"architecture": NATIVE.0}]}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "archMap": [{"architecture": NATIVE.0, "subArchitectures": ["SCMP_ARCH_BOGUS"]}]}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "flags": "SECCOMP_FILTER_FLAG_LOG"}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "flags": ["SECCOMP_FILTER_FLAG_BOGUS"]}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "flags": ["SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV"]}),
            json!({"defaultAction": "SCMP_ACT_NOTIFY"}),
            rule("action", json!("SCMP_ACT_NOTIFY")),
            rule("action", json!("SCMP_ACT_BOGUS")),
            rule("action", Value::Null),
            rule("errnoRet", json!(-1)),
            rule("errnoRet", json!(70000)),
            rule("errnoRet", json!(4095)),
            json!({"defaultAction": "SCMP_ACT_ERRNO", "defaultErrnoRet": 4095}),
            // Two spellings of one field: which one counts is not in the parsed profile.
            json!({"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [], "Syscalls": [{"names": ["getppid"], "action": "SCMP_ACT_ERRNO"}]}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [{"names": ["getppid"], "action": "SCMP_ACT_ERRNO", "errnoRet": 13, "ErrnoRet": 14}]}),
            rule("names", json!([1])),
            rule("names", json!([""])),
            rule("name", json!("getpid")),
            rule("args", json!({})),
            rule("args", json!([{"index": 6, "op": "SCMP_CMP_EQ", "value": 0}])),
            rule("args", json!([{"index": 0, "op": "SCMP_CMP_BOGUS", "value": 0}])),
            rule("args", json!([{"index": 0, "value": 0}])),
            rule("args", json!([{"index": 0, "op": "SCMP_CMP_EQ", "value": -1}])),
            rule("args", json!([{"index": 0, "op": "SCMP_CMP_MASKED_EQ", "value": 255, "valueTwo": false}])),
            rule("includes", json!(false)),
            rule("includes", json!({"caps": "CAP_SYS_ADMIN"})),
            rule("excludes", json!({"arches": [1]})),
            rule("includes", json!({"minKernel": "invalid"})),
            rule("excludes", json!({"minKernel": "4.8.1"})),
            // The same condition with two outcomes.
            json!({"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [
                {"names": ["getppid"], "action": "SCMP_ACT_ERRNO", "args": [{"index": 0, "op": "SCMP_CMP_EQ", "value": 1}]},
                {"names": ["getppid"], "action": "SCMP_ACT_KILL", "args": [{"index": 0, "op": "SCMP_CMP_EQ", "value": 1, "valueTwo": 9}]},
            ]}),
        ] {
            let error = compiled(profile.clone()).err().unwrap_or_else(|| panic!("accepted {profile}"));
            assert!(error.to_string().starts_with("seccomp profile: "), "{error}");
        }
        assert!(compile_for("{", 0, KERNEL).is_err());
    }

    #[test]
    fn what_docker_ignores_is_accepted() {
        for profile in [
            json!({"defaultAction": "SCMP_ACT_ALLOW", "listenerPath": "/run/listener.sock", "listenerMetadata": "x", "unknown": 1}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "flags": [], "architectures": [NATIVE.0]}),
            // Other architectures of this byte order: named, nothing to compile.
            json!({"defaultAction": "SCMP_ACT_ALLOW", "architectures": ["SCMP_ARCH_X86", "SCMP_ARCH_X32", "SCMP_ARCH_ARM", "SCMP_ARCH_MIPSEL64N32", "SCMP_ARCH_RISCV64"]}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "archMap": [{"architecture": NATIVE.0, "subArchitectures": ["SCMP_ARCH_PPC64LE"]}]}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "flags": ["SECCOMP_FILTER_FLAG_LOG", "SECCOMP_FILTER_FLAG_SPEC_ALLOW"]}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "archMap": [{"architecture": "SCMP_ARCH_S390X", "subArchitectures": ["SCMP_ARCH_BOGUS"]}]}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [{"action": "SCMP_ACT_ERRNO"}]}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [{"name": "getppid", "action": "SCMP_ACT_ERRNO", "comment": "old form"}]}),
            rule("names", json!(["no_such_syscall", "getppid"])),
            rule("action", json!("SCMP_ACT_ALLOW")),
            rule("includes", json!({"caps": ["CAP_NOT_A_CAPABILITY"]})),
        ] {
            assert!(compiled(profile.clone()).unwrap_or_else(|e| panic!("{profile}: {e}")).is_some());
        }
        assert_eq!(filter_flags(&json!({})).unwrap(), libc::SECCOMP_FILTER_FLAG_SPEC_ALLOW);
        assert_eq!(filter_flags(&json!({"flags": ["SECCOMP_FILTER_FLAG_LOG"]})).unwrap(), libc::SECCOMP_FILTER_FLAG_LOG);
    }

    #[test]
    fn keys_are_read_in_any_case() {
        let nr = syscall_table::number("getppid").unwrap();
        let answer = |profile: Value, arg: u64| verdict(&compiled(profile).unwrap().expect("a filter").insns, NATIVE.2, nr, [arg, 0, 0, 0, 0, 0]);
        // Capitalised, upper case, and the one letter only case folding turns into an s.
        for profile in [
            json!({"DefaultAction": "SCMP_ACT_ALLOW", "Syscalls": [{"Names": ["getppid"], "Action": "SCMP_ACT_ERRNO", "ErrnoRet": 13}]}),
            json!({"DEFAULTACTION": "SCMP_ACT_ALLOW", "SYSCALLS": [{"NAME": "getppid", "ACTION": "SCMP_ACT_ERRNO", "ERRNORET": 13}]}),
            json!({"defaultaction": "SCMP_ACT_ALLOW", "\u{17f}y\u{17f}call\u{17f}": [{"name\u{17f}": ["getppid"], "action": "SCMP_ACT_ERRNO", "errnoret": 13}]}),
        ] {
            assert_eq!(answer(profile.clone(), 0), RET_ERRNO | 13, "{profile}");
        }
        // A default that denies everything is not an empty profile for its capital letter.
        assert_eq!(answer(json!({"DefaultAction": "SCMP_ACT_ERRNO", "DefaultErrnoRet": 77, "Syscalls": []}), 0), RET_ERRNO | 77);
        let masked = json!({"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [
            {"names": ["getppid"], "action": "SCMP_ACT_ERRNO", "Args": [{"Index": 0, "Op": "SCMP_CMP_MASKED_EQ", "Value": 3, "ValueTwo": 1}]},
        ]});
        assert_eq!([0, 1, 5].map(|arg| answer(masked.clone(), arg)), [RET_ALLOW, ERRNO, ERRNO]);
        let for_admin = json!({"defaultAction": "SCMP_ACT_ERRNO", "syscalls": [{"names": ["getppid"], "action": "SCMP_ACT_ALLOW", "Includes": {"Caps": ["CAP_SYS_ADMIN"]}}]});
        assert_eq!(answer(for_admin, 0), ERRNO);
        assert!(!selected(&json!({"Excludes": {"Arches": [NATIVE.1]}}), 0, KERNEL).unwrap());
        assert!(!selected(&json!({"includes": {"min\u{212a}ernel": "6.13"}}), 0, KERNEL).unwrap());
        for profile in [
            json!({"defaultAction": "SCMP_ACT_ALLOW", "Architectures": ["SCMP_ARCH_BOGUS"]}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "Flags": ["SECCOMP_FILTER_FLAG_BOGUS"]}),
            json!({"defaultAction": "SCMP_ACT_ALLOW", "ArchMap": [{"Architecture": NATIVE.0, "SubArchitectures": ["SCMP_ARCH_BOGUS"]}]}),
        ] {
            assert!(compiled(profile.clone()).is_err(), "accepted {profile}");
        }
    }

    #[test]
    fn conditional_rules_use_capabilities_architecture_and_kernel() {
        let admin = 1 << 21;
        assert!(selected(&json!({}), 0, None).unwrap());
        assert!(selected(&json!({"includes": {"arches": [NATIVE.1], "caps": ["CAP_SYS_ADMIN"], "minKernel": "4.8"}}), admin, KERNEL).unwrap());
        assert!(!selected(&json!({"includes": {"caps": ["CAP_SYS_ADMIN"]}}), 0, KERNEL).unwrap());
        assert!(!selected(&json!({"includes": {"caps": ["CAP_SYS_ADMIN", "CAP_CHOWN"]}}), admin, KERNEL).unwrap());
        assert!(!selected(&json!({"excludes": {"caps": ["CAP_CHOWN", "CAP_SYS_ADMIN"]}}), admin, KERNEL).unwrap());
        // Docker compares the names as written: no prefix, no match.
        assert!(!selected(&json!({"includes": {"caps": ["SYS_ADMIN"]}}), admin, KERNEL).unwrap());
        assert!(selected(&json!({"excludes": {"caps": ["CAP_NOT_A_CAPABILITY"]}}), u64::MAX, KERNEL).unwrap());
        assert!(!selected(&json!({"includes": {"arches": ["s390x"]}}), 0, KERNEL).unwrap());
        assert!(!selected(&json!({"excludes": {"arches": ["s390x", NATIVE.1]}}), 0, KERNEL).unwrap());
        assert!(selected(&json!({"includes": {"minKernel": "6.12"}}), 0, KERNEL).unwrap());
        assert!(!selected(&json!({"includes": {"minKernel": "6.13"}}), 0, KERNEL).unwrap());
        assert!(!selected(&json!({"excludes": {"minKernel": "5.0"}}), 0, KERNEL).unwrap());
        assert!(selected(&json!({"includes": {"minKernel": "6.13"}}), 0, None).is_err());
        assert_eq!(running_kernel("6.12.5-asagiri\n"), KERNEL);
        assert_eq!(running_kernel("6.12-rc1"), KERNEL);
        assert_eq!(running_kernel("unknown"), None);
    }

    #[test]
    fn a_rule_is_enforced_with_its_errno_next_to_the_host_boundary() {
        let filter = compiled(json!({"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [{"names": ["getppid"], "action": "SCMP_ACT_ERRNO", "errnoRet": 13}]})).unwrap().unwrap();
        assert!(enforced(&filter, || {
            // SAFETY: plain syscalls.
            let denied = errno_of(unsafe { libc::syscall(libc::SYS_getppid) }) == libc::EACCES;
            let vsock = errno_of(unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) } as libc::c_long) == libc::EAFNOSUPPORT;
            let io_uring = errno_of(unsafe { libc::syscall(libc::SYS_io_uring_setup, 0, 0) }) == libc::EPERM;
            denied && vsock && io_uring && unsafe { libc::getpid() } > 0
        }));
        // A rule its conditions exclude leaves the syscall alone.
        let filter = compiled(rule("includes", json!({"caps": ["CAP_SYS_ADMIN"]}))).unwrap().unwrap();
        assert!(enforced(&filter, || errno_of(unsafe { libc::syscall(libc::SYS_getppid) }) == 0));
    }

    #[test]
    fn argument_conditions_are_enforced() {
        // personality(0xffffffff) only reads; two conditions on one argument
        // are alternatives.
        let filter = compiled(json!({"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [
            {"names": ["personality"], "action": "SCMP_ACT_ERRNO", "errnoRet": 22, "args": [
                {"index": 0, "op": "SCMP_CMP_EQ", "value": 8},
                {"index": 0, "op": "SCMP_CMP_MASKED_EQ", "value": 0xff00, "valueTwo": 0x4200},
            ]},
            {"names": ["getpriority"], "action": "SCMP_ACT_ERRNO", "errnoRet": 34, "args": [
                {"index": 0, "op": "SCMP_CMP_EQ", "value": 1},
                {"index": 1, "op": "SCMP_CMP_GT", "value": 4000000000u32},
            ]},
            // The kernel reads 32 bits of umask's argument; the filter sees all 64.
            {"names": ["umask"], "action": "SCMP_ACT_ERRNO", "errnoRet": 75, "args": [{"index": 0, "op": "SCMP_CMP_GE", "value": 0x2_0000_0012u64}]},
            {"names": ["umask"], "action": "SCMP_ACT_ERRNO", "errnoRet": 34, "args": [{"index": 0, "op": "SCMP_CMP_LT", "value": 0x1_0000_0012u64}]},
            {"names": ["umask"], "action": "SCMP_ACT_ERRNO", "errnoRet": 22, "args": [
                {"index": 0, "op": "SCMP_CMP_MASKED_EQ", "value": 0xf_0000_000fu64, "valueTwo": 0x1_0000_0003u64},
            ]},
        ]})).unwrap().unwrap();
        assert!(enforced(&filter, || {
            // SAFETY: plain syscalls that change nothing.
            let personality = |arg: libc::c_ulong| errno_of(unsafe { libc::syscall(libc::SYS_personality, arg) });
            let priority = |which: libc::c_long, who: libc::c_long| errno_of(unsafe { libc::syscall(libc::SYS_getpriority, which, who) });
            personality(0xffff_ffff) == 0 && personality(8) == libc::EINVAL && personality(0x4233) == libc::EINVAL
                // Conditions on different arguments must all hold.
                && priority(1, 4_000_000_001) == libc::ERANGE && priority(1, 0) != libc::ERANGE && priority(0, 4_000_000_001) != libc::ERANGE
        }));
        assert!(enforced(&filter, || {
            // SAFETY: umask only swaps the file mode mask of this child.
            let umask = |arg: u64| errno_of(unsafe { libc::syscall(libc::SYS_umask, arg) });
            // The high half decides first, whatever the low one would say.
            umask(0x2_0000_0012) == libc::EOVERFLOW && umask(0x3_0000_0000) == libc::EOVERFLOW && umask(0x1_ffff_ffff) == 0
                && umask(0x1_0000_0011) == libc::ERANGE && umask(0xffff_ffff) == libc::ERANGE && umask(0x1_0000_0012) == 0
                && umask(0x1_0000_0023) == libc::EINVAL && umask(0x1_0000_0024) == 0
        }));
    }

    #[test]
    fn newer_syscalls_than_the_profile_names_fail_with_enosys() {
        // An allow list that stops well before the newest syscalls.
        let profile = |default: Value| json!({"defaultAction": default, "syscalls": [
            {"names": ["exit_group", "exit", "getpid", "write", "rt_sigreturn"], "action": "SCMP_ACT_ALLOW"},
        ]});
        let probe = || {
            // SAFETY: plain syscalls that fail before doing anything.
            let newer = errno_of(unsafe { libc::syscall(libc::SYS_close_range, 1000, 1000, 0) });
            let older = errno_of(unsafe { libc::syscall(libc::SYS_umask, 0o22) });
            newer == libc::ENOSYS && older == libc::EPERM && unsafe { libc::getpid() } > 0
        };
        assert!(enforced(&compiled(profile(json!("SCMP_ACT_ERRNO"))).unwrap().unwrap(), probe));
        // Not when everything unnamed is allowed anyway.
        let filter = compiled(json!({"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [{"names": ["getppid"], "action": "SCMP_ACT_ERRNO"}]})).unwrap().unwrap();
        assert!(enforced(&filter, || errno_of(unsafe { libc::syscall(libc::SYS_close_range, 1000, 1000, 0) }) != libc::ENOSYS));
    }

    #[test]
    fn the_program_checks_the_architecture_first() {
        for x32 in [false, true] {
            let p = built(vec![(5, vec![], ERRNO)], Some(40), x32);
            assert_eq!((p[0].code, p[0].k), (LD_W_ABS, OFF_ARCH));
            assert_eq!((p[1].code, p[1].k, p[1].jt, p[1].jf), (JEQ_K, 7, 1, 0));
            assert_eq!((p[2].code, p[2].k), (RET_K, RET_KILL));
            assert_eq!((p[3].code, p[3].k), (LD_W_ABS, OFF_NR));
            // Another architecture's syscalls never reach the rules.
            for nr in [0, 5, 41, 500] {
                assert_eq!(verdict(&p, 8, nr, [0; 6]), RET_KILL);
            }
            assert_eq!(verdict(&p, 7, 5, [0; 6]), ERRNO);
            assert_eq!(verdict(&p, 7, 6, [0; 6]), RET_ALLOW);
            // Newer than the profile knows; x32 numbers are not that.
            assert_eq!(verdict(&p, 7, 41, [0; 6]), RET_ERRNO | libc::ENOSYS as u32);
            let foreign = if x32 { RET_KILL } else { RET_ERRNO | libc::ENOSYS as u32 };
            assert_eq!(verdict(&p, 7, X32_SYSCALL_BIT | 5, [0; 6]), foreign);
            assert_eq!(verdict(&p, 7, u32::MAX, [0; 6]), foreign);
        }
        // Without the stub the default answers for what no rule names.
        assert_eq!(verdict(&built(vec![(5, vec![], ERRNO)], None, false), 7, 41, [0; 6]), RET_ALLOW);
        assert_eq!(built(vec![], None, false).len(), 5);
    }

    #[test]
    fn comparisons_cover_all_64_bits() {
        let values = [0, 1, 5, 0x7fff_ffff, 0x8000_0000, 0xffff_ffff, 0x1_0000_0000, 0x1_0000_0005, 0x5_0000_0001, 0x7fff_ffff_ffff_ffff, 0x8000_0000_0000_0000, 0xffff_ffff_0000_0000, u64::MAX];
        type Holds = fn(u64, u64, u64) -> bool;
        let ops: [(Op, Holds); 7] = [
            (Op::Ne, |arg, value, _| arg != value),
            (Op::Lt, |arg, value, _| arg < value),
            (Op::Le, |arg, value, _| arg <= value),
            (Op::Eq, |arg, value, _| arg == value),
            (Op::Ge, |arg, value, _| arg >= value),
            (Op::Gt, |arg, value, _| arg > value),
            (Op::MaskedEq, |arg, mask, two| arg & mask == two),
        ];
        for (op, holds) in ops {
            for value in values {
                for two in [0, value, 0x1_0000_0005 & value] {
                    for index in [0, 3, 5] {
                        let p = built(vec![(9, vec![cond(index, op, value, two)], ERRNO)], None, false);
                        for arg in values {
                            let mut args = [0x1234_5678_9abc_def0; 6];
                            args[index as usize] = arg;
                            let expected = if holds(arg, value, two) { ERRNO } else { RET_ALLOW };
                            assert_eq!(verdict(&p, 7, 9, args), expected, "{op:?} {value:#x} {two:#x} on {arg:#x}");
                            assert_eq!(verdict(&p, 7, 8, args), RET_ALLOW);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn conditions_on_several_arguments_must_all_hold() {
        let p = built(vec![
            (9, vec![cond(0, Op::Eq, 1, 0), cond(2, Op::Gt, 0x1_0000_0000, 0), cond(5, Op::MaskedEq, 0xf0, 0x20)], ERRNO),
            (9, vec![cond(1, Op::Ne, 0, 0)], RET_KILL),
        ], None, false);
        assert_eq!(verdict(&p, 7, 9, [1, 0, 0x1_0000_0001, 0, 0, 0x2f]), ERRNO);
        for miss in [[2, 0, 0x1_0000_0001, 0, 0, 0x2f], [1, 0, 0x1_0000_0000, 0, 0, 0x2f], [1, 0, 0x1_0000_0001, 0, 0, 0x3f]] {
            assert_eq!(verdict(&p, 7, 9, miss), RET_ALLOW);
        }
        // A rule that does not apply leaves the syscall to the next one.
        assert_eq!(verdict(&p, 7, 9, [2, 7, 0, 0, 0, 0]), RET_KILL);
        assert_eq!(verdict(&p, 7, 9, [1, 7, 0x1_0000_0001, 0, 0, 0x2f]), ERRNO);
    }

    #[test]
    fn every_syscall_number_gets_its_own_answer() {
        // Runs of neighbours with one action, lone numbers, gaps, and two
        // syscalls with conditions, in a profile as long as Docker's.
        let action = |nr: u32| match nr {
            0..=120 => Some(ERRNO),
            121..=180 if nr % 2 == 0 => Some(RET_ERRNO | nr),
            200..=330 if nr % 7 != 0 => Some(RET_ERRNO | (nr % 3)),
            424..=470 => Some(RET_KILL),
            _ => None,
        };
        let mut rules: Vec<_> = (0..500).filter_map(|nr| Some((nr, vec![], action(nr)?))).collect();
        rules.push((185, vec![cond(0, Op::Eq, 3, 0)], ERRNO));
        rules.push((480, vec![cond(1, Op::Ge, 3, 0)], ERRNO));
        let p = built(rules, None, true);
        for nr in 0..600 {
            let expected = match nr {
                185 | 480 => RET_ALLOW,
                _ => action(nr).unwrap_or(RET_ALLOW),
            };
            assert_eq!(verdict(&p, 7, nr, [0; 6]), expected, "syscall {nr}");
        }
        assert_eq!(verdict(&p, 7, 185, [3, 0, 0, 0, 0, 0]), ERRNO);
        assert_eq!(verdict(&p, 7, 480, [0, 3, 0, 0, 0, 0]), ERRNO);
        // Jumps only go forward and stay inside, the far ones unconditionally.
        assert!(p.iter().any(|insn| insn.code == JA), "{} instructions and no long jump", p.len());
        for (at, insn) in p.iter().enumerate() {
            let reach = match insn.code {
                JA => insn.k as usize,
                JEQ_K | JGT_K | JGE_K => insn.jt.max(insn.jf) as usize,
                _ => 0,
            };
            assert!(at + 1 + reach < p.len() || insn.code == RET_K, "instruction {at} leaves the program");
        }
        assert_eq!(p.last().map(|insn| insn.code), Some(RET_K));
        // A handful of comparisons find any of them.
        let longest = (0..600).map(|nr| steps(&p, nr)).max().unwrap();
        assert!(longest < 40, "{longest} instructions for one syscall");
    }

    /// How many instructions a syscall without arguments of note runs through.
    fn steps(program: &[libc::sock_filter], nr: u32) -> usize {
        let (mut at, mut count) = (3, 3);
        loop {
            let insn = program[at];
            count += 1;
            let jump = |holds: bool| usize::from(if holds { insn.jt } else { insn.jf });
            at += 1 + match insn.code {
                JA => insn.k as usize,
                JEQ_K => jump(nr == insn.k),
                JGT_K => jump(nr > insn.k),
                JGE_K => jump(nr >= insn.k),
                RET_K => return count,
                _ => 0,
            };
        }
    }

    #[test]
    fn many_rules_for_one_syscall_take_long_jumps() {
        // 100 rules of five instructions: past what a conditional jump reaches.
        let rules: Vec<_> = (0..100).map(|i| (9, vec![cond(0, Op::Eq, 1000 + i, 0)], RET_ERRNO | i as u32)).collect();
        let p = built(rules.into_iter().chain([(8, vec![], RET_KILL), (10, vec![], RET_KILL)]).collect(), None, false);
        assert!(p.len() > 500);
        for i in 0..100 {
            assert_eq!(verdict(&p, 7, 9, [1000 + i, 0, 0, 0, 0, 0]), RET_ERRNO | i as u32);
        }
        assert_eq!(verdict(&p, 7, 9, [999, 0, 0, 0, 0, 0]), RET_ALLOW);
        for (nr, expected) in [(7, RET_ALLOW), (8, RET_KILL), (10, RET_KILL), (11, RET_ALLOW)] {
            assert_eq!(verdict(&p, 7, nr, [1000, 0, 0, 0, 0, 0]), expected);
        }
    }

    #[test]
    fn a_program_too_long_for_the_kernel_is_refused() {
        let rules = |count: u64| -> Vec<Value> {
            let rule = |i: u64| json!({"names": ["getppid"], "action": "SCMP_ACT_ERRNO", "args": [{"index": 0, "op": "SCMP_CMP_EQ", "value": i}]});
            (0..count).map(rule).collect()
        };
        let fits = compiled(json!({"defaultAction": "SCMP_ACT_ALLOW", "syscalls": rules(800)})).unwrap().unwrap();
        assert!(fits.insns.len() > 4000 && fits.insns.len() <= MAX_INSNS, "{}", fits.insns.len());
        assert!(enforced(&fits, || unsafe { libc::getpid() } > 0), "the kernel takes a program of {} instructions", fits.insns.len());
        let error = compiled(json!({"defaultAction": "SCMP_ACT_ALLOW", "syscalls": rules(900)})).err().expect("too long");
        assert!(error.to_string().contains("instructions; the kernel takes up to 4096"), "{error}");
        // Far more rules than can fit are refused before the program is built.
        let conds: Vec<Value> = (0..5000).map(|i| json!({"index": 0, "op": "SCMP_CMP_EQ", "value": i})).collect();
        let error = compiled(json!({"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [{"names": ["getppid", "getpid"], "action": "SCMP_ACT_ERRNO", "args": conds}]})).err().expect("too many");
        assert!(error.to_string().contains("more than 4096 instructions"), "{error}");
    }

    #[test]
    fn overlapping_rules_are_settled_as_docker_settles_them() {
        let errno = |n: u32| RET_ERRNO | n;
        let verdicts = |profile: Value| {
            let filter = compiled(profile).unwrap().unwrap();
            let nr = syscall_table::number("getppid").unwrap();
            [0, 1].map(|arg| verdict(&filter.insns, NATIVE.2, nr, [arg, 0, 0, 0, 0, 0]))
        };
        let plain = |errno: u32| json!({"names": ["getppid"], "action": "SCMP_ACT_ERRNO", "errnoRet": errno});
        let on_one = |errno: u32| json!({"names": ["getppid"], "action": "SCMP_ACT_ERRNO", "errnoRet": errno, "args": [{"index": 0, "op": "SCMP_CMP_EQ", "value": 1}]});
        let profile = |rules: Value| json!({"defaultAction": "SCMP_ACT_ALLOW", "syscalls": rules});
        // The first rule without conditions, wherever the others stand.
        assert_eq!(verdicts(profile(json!([plain(13), plain(22)]))), [errno(13); 2]);
        assert_eq!(verdicts(profile(json!([plain(13), on_one(22)]))), [errno(13); 2]);
        assert_eq!(verdicts(profile(json!([on_one(22), plain(13)]))), [errno(13); 2]);
        // A repeated rule is one rule; one like the default is none.
        assert_eq!(verdicts(profile(json!([on_one(22), on_one(22)]))), [RET_ALLOW, errno(22)]);
        assert_eq!(verdicts(profile(json!([{"names": ["getppid"], "action": "SCMP_ACT_ALLOW"}, on_one(22)]))), [RET_ALLOW, errno(22)]);
        // The mask applies to what a masked argument is compared with.
        let masked = json!({"names": ["getppid"], "action": "SCMP_ACT_ERRNO", "args": [{"index": 0, "op": "SCMP_CMP_MASKED_EQ", "value": 1, "valueTwo": 0xff}]});
        assert_eq!(verdicts(profile(json!([masked]))), [RET_ALLOW, ERRNO]);
        // Two conditions on one argument are alternatives, and so are all the rule's others.
        let either = json!({"names": ["getppid"], "action": "SCMP_ACT_ERRNO", "args": [
            {"index": 0, "op": "SCMP_CMP_EQ", "value": 0}, {"index": 0, "op": "SCMP_CMP_EQ", "value": 1}, {"index": 1, "op": "SCMP_CMP_EQ", "value": 9},
        ]});
        assert_eq!(verdicts(profile(json!([either]))), [ERRNO; 2]);
    }
}
