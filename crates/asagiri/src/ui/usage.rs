//! What Asagiri's window costs this computer, for the status bar beside its
//! version: memory and CPU of the window's process and the processes that
//! draw its page (WebKit's on the Mac and Linux, WebView2's on Windows), not the
//! engine's VM, which the status bar already shows. The Mac's window and
//! Windows' (crates/asagiri-win, by #[path]) answer `usage` with it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use serde_json::json;

/// One process: what to call it, its memory in bytes (the footprint the
/// Activity Monitor or Task Manager shows), and its CPU time so far.
struct Proc {
    pid: u32,
    started: u64,
    part: &'static str,
    mem: u64,
    cpu_ns: u64,
}

/// The last look: CPU time by PID and creation time. A reused PID must not
/// inherit the CPU baseline of the process that previously owned it.
static LAST: Mutex<Option<(Instant, HashMap<(u32, u64), u64>)>> = Mutex::new(None);

fn cpu_percent(p: &Proc, seconds: Option<f64>, previous: Option<&HashMap<(u32, u64), u64>>) -> f64 {
    match (seconds, previous.and_then(|m| m.get(&(p.pid, p.started)))) {
        (Some(s), Some(before)) if s > 0.0 => p.cpu_ns.saturating_sub(*before) as f64 / 1e9 / s * 100.0,
        _ => 0.0,
    }
}

/// `{type: "usage", mem, cpu, cores, parts: [{name, mem, cpu, n}]}`: bytes, and
/// CPU as a percentage of one core since the last look (0 the first time).
pub fn event() -> String {
    let procs = procs();
    let now = Instant::now();
    let mut last = LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let secs = last.as_ref().map(|(at, _)| now.duration_since(*at).as_secs_f64()).filter(|s| *s > 0.2);
    let mut parts: Vec<(&'static str, u64, f64, u32)> = Vec::new();
    for p in &procs {
        let c = cpu_percent(p, secs, last.as_ref().map(|(_, m)| m));
        match parts.iter_mut().find(|x| x.0 == p.part) {
            Some(x) => {
                x.1 += p.mem;
                x.2 += c;
                x.3 += 1;
            }
            None => parts.push((p.part, p.mem, c, 1)),
        }
    }
    *last = Some((now, procs.iter().map(|p| ((p.pid, p.started), p.cpu_ns)).collect()));
    let mem: u64 = parts.iter().map(|p| p.1).sum();
    let cpu: f64 = parts.iter().map(|p| p.2).sum();
    let round = |c: f64| (c * 1000.0).round() / 1000.0;
    json!({
        // CPU is a percentage of one core, as Activity Monitor and top count it;
        // the page divides by the cores to say how much of the computer it is.
        "type": "usage", "mem": mem, "cpu": round(cpu), "cores": std::thread::available_parallelism().map_or(1, |n| n.get()),
        "parts": parts.iter().map(|(name, mem, cpu, n)| json!({"name": name, "mem": mem, "cpu": round(*cpu), "n": n})).collect::<Vec<_>>(),
    })
    .to_string()
}

#[cfg(target_os = "macos")]
fn procs() -> Vec<Proc> {
    // WebKit's processes are launchd's children, not ours: macOS says which
    // app they work for through this (private, long-stable) call.
    type Responsible = unsafe extern "C" fn(libc::pid_t) -> libc::pid_t;
    // SAFETY: dlsym with a NUL-terminated name; the symbol, when there, has this signature.
    let responsible: Option<Responsible> = unsafe {
        let f = libc::dlsym(libc::RTLD_DEFAULT, c"responsibility_get_pid_responsible_for_pid".as_ptr());
        (!f.is_null()).then(|| std::mem::transmute::<*mut libc::c_void, Responsible>(f))
    };
    let me = std::process::id() as libc::pid_t;
    let mut out = Vec::new();
    if let Some(p) = sample(me, "Window") {
        out.push(p);
    }
    let Some(responsible) = responsible else { return out };
    let mut pids = vec![0 as libc::pid_t; 4096];
    // SAFETY: the buffer is pids.len() pid_t long, passed as its size in bytes.
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * size_of::<libc::pid_t>()) as libc::c_int) };
    for &pid in pids.iter().take(n.max(0) as usize) {
        // SAFETY: plain query of another process by pid.
        if pid == me || pid <= 0 || unsafe { responsible(pid) } != me {
            continue;
        }
        let mut name = [0u8; 256];
        // SAFETY: the buffer is 256 bytes, passed as its size.
        let len = unsafe { libc::proc_name(pid, name.as_mut_ptr().cast(), name.len() as u32) };
        let name = String::from_utf8_lossy(&name[..len.max(0) as usize]).into_owned();
        let part = if name.contains("WebContent") {
            "Page"
        } else if name.contains("GPU") {
            "Graphics"
        } else if name.contains("Networking") {
            "Network"
        } else {
            "Helpers"
        };
        out.extend(sample(pid, part));
    }
    out
}

#[cfg(target_os = "macos")]
fn sample(pid: libc::pid_t, part: &'static str) -> Option<Proc> {
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    // SAFETY: a rusage_info_v2 for the RUSAGE_INFO_V2 flavor.
    let r = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, (&raw mut info).cast()) };
    if r != 0 {
        return None;
    }
    // CPU times are in Mach ticks.
    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
    }
    let mut tb = Timebase { numer: 0, denom: 0 };
    // SAFETY: fills the struct.
    unsafe { mach_timebase_info(&raw mut tb) };
    let ticks = info.ri_user_time + info.ri_system_time;
    let ns = if tb.denom == 0 { ticks } else { ticks * u64::from(tb.numer) / u64::from(tb.denom) };
    Some(Proc { pid: pid as u32, started: info.ri_proc_start_abstime, part, mem: info.ri_phys_footprint, cpu_ns: ns })
}

/// This process and its children: WebKitGTK's page and network processes.
#[cfg(target_os = "linux")]
fn procs() -> Vec<Proc> {
    let me = std::process::id();
    // pid, parent, name: field 2 of /proc/<pid>/stat is the name in brackets, then state and parent.
    let all: Vec<(u32, u32, String)> = std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let (name, rest) = stat.split_once(" (")?.1.rsplit_once(") ")?;
            Some((pid, rest.split(' ').nth(1)?.parse().ok()?, name.to_owned()))
        })
        .collect();
    let mut tree = vec![me];
    let mut i = 0;
    while i < tree.len() {
        let parent = tree[i];
        tree.extend(all.iter().filter(|p| p.1 == parent && !tree.contains(&p.0)).map(|p| p.0).collect::<Vec<_>>());
        i += 1;
    }
    // SAFETY: sysconf has no preconditions.
    let tick_ns = 1_000_000_000 / u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).unwrap_or(100).max(1);
    tree.into_iter()
        .filter_map(|pid| {
            let name = &all.iter().find(|p| p.0 == pid)?.2;
            let part = if pid == me {
                "Window"
            } else if name.starts_with("WebKitWebProces") {
                "Page"
            } else if name.starts_with("WebKitNetworkPr") {
                "Network"
            } else if name.starts_with("WebKitGPUProces") {
                "Graphics"
            } else {
                "Helpers"
            };
            // utime and stime: fields 14 and 15, so 11 and 12 after the name.
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let f: Vec<u64> = stat.rsplit_once(") ")?.1.split(' ').map(|x| x.parse().unwrap_or(0)).collect();
            let cpu_ns = (f.get(11)? + f.get(12)?) * tick_ns;
            // Pss: shared pages split between the processes that share them, as a footprint.
            let rollup = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).unwrap_or_default();
            let kib = rollup.lines().find_map(|l| l.strip_prefix("Pss:")).and_then(|v| v.trim().trim_end_matches(" kB").trim().parse::<u64>().ok()).unwrap_or(0);
            Some(Proc { pid, started: *f.get(19)?, part, mem: kib << 10, cpu_ns })
        })
        .collect()
}

/// Windows retains the original parent PID after its parent exits. Require
/// creation-time ordering before following an edge, or a recycled PID can
/// make an unrelated application look like one of our helpers (#190).
#[cfg(any(windows, test))]
fn descendants(me: u32, parents: &[(u32, u32)], mut started: impl FnMut(u32) -> Option<u64>) -> Vec<u32> {
    let Some(born) = started(me) else { return Vec::new() };
    let mut tree = vec![(me, born)];
    let mut index = 0;
    while index < tree.len() {
        let (parent, parent_born) = tree[index];
        for &(pid, ppid) in parents {
            if ppid != parent || tree.iter().any(|(known, _)| *known == pid) {
                continue;
            }
            if let Some(born) = started(pid).filter(|born| *born >= parent_born) {
                tree.push((pid, born));
            }
        }
        index += 1;
    }
    tree.into_iter().map(|(pid, _)| pid).collect()
}

#[cfg(windows)]
fn procs() -> Vec<Proc> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};

    let mut all: Vec<(u32, u32, String)> = Vec::new();
    // SAFETY: a snapshot handle walked with a sized PROCESSENTRY32W and closed.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return Vec::new();
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e) != 0;
        while ok {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            all.push((e.th32ProcessID, e.th32ParentProcessID, String::from_utf16_lossy(&e.szExeFile[..len])));
            ok = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    let me = std::process::id();
    let parents: Vec<_> = all.iter().map(|p| (p.0, p.1)).collect();
    let mut samples = HashMap::new();
    let tree = descendants(me, &parents, |pid| {
        // Sample once: the creation time, memory and CPU belong to the same
        // process handle even if its PID is recycled after this query.
        let sample = windows_sample(pid)?;
        let born = sample.0;
        samples.insert(pid, sample);
        Some(born)
    });
    tree.into_iter().filter_map(|pid| {
        let name = all.iter().find(|p| p.0 == pid)?.2.to_ascii_lowercase();
        let part = if pid == me { "Window" } else if name.starts_with("msedgewebview2") { "WebView2" } else { "Helpers" };
        let &(started, mem, cpu_ns) = samples.get(&pid)?;
        Some(Proc { pid, started, part, mem, cpu_ns })
    }).collect()
}

#[cfg(windows)]
fn windows_sample(pid: u32) -> Option<(u64, u64, u64)> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let ticks = |f: FILETIME| u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime);
    // SAFETY: a query-only handle, read and closed on every path.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h == 0 { return None; }
        let (mut c, mut x, mut k, mut u) = (std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed());
        if GetProcessTimes(h, &mut c, &mut x, &mut k, &mut u) == 0 {
            CloseHandle(h);
            return None;
        }
        let mut m: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        m.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let mem = if GetProcessMemoryInfo(h, (&raw mut m).cast(), m.cb) != 0 { m.PrivateUsage as u64 } else { 0 };
        CloseHandle(h);
        Some((ticks(c), mem, (ticks(k) + ticks(u)) * 100))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reused_pid_does_not_inherit_the_previous_process_cpu() {
        let previous = HashMap::from([((42, 100), 1_000_000_000)]);
        let current = Proc { pid: 42, started: 200, part: "Helpers", mem: 0, cpu_ns: 9_000_000_000 };
        assert_eq!(cpu_percent(&current, Some(1.0), Some(&previous)), 0.0);
    }

    #[test]
    fn same_process_reports_cpu_delta_and_new_processes_start_at_zero() {
        let previous = HashMap::from([((42, 100), 1_000_000_000)]);
        let current = Proc { pid: 42, started: 100, part: "Helpers", mem: 0, cpu_ns: 1_500_000_000 };
        assert_eq!(cpu_percent(&current, Some(2.0), Some(&previous)), 25.0);
        assert_eq!(cpu_percent(&current, Some(2.0), None), 0.0);
        assert_eq!(cpu_percent(&current, None, Some(&previous)), 0.0);
        assert_eq!(cpu_percent(&current, Some(0.0), Some(&previous)), 0.0);
        let reset = Proc { cpu_ns: 0, ..current };
        assert_eq!(cpu_percent(&reset, Some(2.0), Some(&previous)), 0.0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn current_linux_process_has_a_creation_time_and_can_be_sampled() {
        let samples = procs();
        let me = samples.iter().find(|p| p.pid == std::process::id()).unwrap();
        assert!(me.started > 0);
        assert_eq!(me.part, "Window");
        let event: serde_json::Value = serde_json::from_str(&event()).unwrap();
        assert_eq!(event["type"], "usage");
        assert!(event["cpu"].as_f64().unwrap().is_finite());
    }

    #[test]
    fn recycled_parent_pid_excludes_the_unrelated_subtree() {
        let parents = [(2, 1), (3, 2), (4, 3), (5, 2)];
        let times = [(1, 10), (2, 20), (3, 5), (4, 30), (5, 25)];
        assert_eq!(descendants(1, &parents, |pid| times.iter().find(|p| p.0 == pid).map(|p| p.1)), [1, 2, 5]);
    }

    #[test]
    fn child_order_and_identical_creation_ticks_do_not_drop_valid_children() {
        let parents = [(3, 2), (2, 1)];
        assert_eq!(descendants(1, &parents, |_| Some(10)), [1, 2, 3]);
    }

    #[test]
    fn unreadable_processes_are_not_assumed_to_belong_to_the_window() {
        assert_eq!(descendants(1, &[(2, 1), (3, 2)], |pid| (pid != 2).then_some(10)), [1]);
        assert!(descendants(1, &[], |_| None).is_empty());
    }

    #[test]
    fn parent_cycles_never_duplicate_processes() {
        assert_eq!(descendants(1, &[(1, 1), (2, 1), (1, 2)], |_| Some(10)), [1, 2]);
    }

    #[cfg(windows)]
    #[test]
    fn current_window_process_can_be_sampled() {
        assert!(windows_sample(std::process::id()).unwrap().0 > 0);
        assert!(procs().iter().any(|p| p.pid == std::process::id() && p.part == "Window"));
    }
}
