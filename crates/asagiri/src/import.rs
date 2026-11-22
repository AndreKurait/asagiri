//! Moving to Asagiri from OrbStack or Docker Desktop: images, volumes and
//! containers, copied through the docker CLI so each engine speaks its own
//! API. The source is only read: `docker save` for images, `inspect` for
//! containers, and for a volume a short-lived helper container that mounts
//! it read-only and is removed after (that keeps file owners, which a copy
//! through the Finder would lose). Containers arrive stopped.

use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::client;

/// The engines Asagiri can import from: docker context name, label. Docker
/// Desktop's context has the same name on Windows; OrbStack is Mac-only.
pub const SOURCES: &[(&str, &str)] = if cfg!(windows) { &[("desktop-linux", "Docker Desktop")] } else { &[("orbstack", "OrbStack"), ("desktop-linux", "Docker Desktop")] };

// asagiri-win builds this file too, where docker_cli() gives a String.
#[allow(clippy::useless_conversion)]
fn docker() -> io::Result<PathBuf> {
    client::docker_cli().map(PathBuf::from).ok_or_else(|| io::Error::other("no docker CLI found (Asagiri bundles one)"))
}

/// The docker CLI, run without a console window of its own (Windows opens
/// one for each console program a windowed one starts).
fn cli() -> io::Result<Command> {
    #[allow(unused_mut)]
    let mut cmd = Command::new(docker()?);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    Ok(cmd)
}

fn known(source: &str) -> io::Result<&'static str> {
    SOURCES.iter().find(|(c, _)| *c == source).map(|(c, _)| *c).ok_or_else(|| io::Error::other(format!("unknown source {source}")))
}

/// `docker --context ctx <args>`, its output as JSON lines.
fn json_lines(ctx: &str, args: &[&str]) -> io::Result<Vec<Value>> {
    let out = cli()?.arg("--context").arg(ctx).args(args).stderr(Stdio::piped()).output()?;
    if !out.status.success() {
        return Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_owned()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
}

/// Which sources are installed and answering.
pub fn sources() -> Vec<Value> {
    SOURCES
        .iter()
        .map(|(ctx, label)| {
            json!({"id": ctx, "label": label, "running": answers(ctx)})
        })
        .collect()
}

/// A 64-hex name is an anonymous volume: not worth moving by default.
fn anonymous(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// What the source has, and which of it Asagiri already has.
pub fn inventory(source: &str) -> io::Result<Value> {
    let ctx = known(source)?;
    let mine = |args: &[&str], key: &str| -> Vec<String> {
        json_lines(crate::flavor::CONTEXT.get(), args).unwrap_or_default().iter().filter_map(|v| v[key].as_str().map(str::to_owned)).collect()
    };
    let asagiri_images: Vec<String> = json_lines(crate::flavor::CONTEXT.get(), &["image", "ls", "--format", "{{json .}}"])
        .unwrap_or_default()
        .iter()
        .map(|v| format!("{}:{}", v["Repository"].as_str().unwrap_or(""), v["Tag"].as_str().unwrap_or("")))
        .collect();
    let asagiri_volumes = mine(&["volume", "ls", "--format", "{{json .}}"], "Name");
    let asagiri_containers = mine(&["ps", "-a", "--format", "{{json .}}"], "Names");
    let images: Vec<Value> = json_lines(ctx, &["image", "ls", "-a", "--digests", "--format", "{{json .}}"])?
        .into_iter()
        .filter_map(|v| {
            let repository = v["Repository"].as_str()?;
            if repository == "<none>" { return None; }
            let name = match v["Tag"].as_str()? {
                "<none>" => {
                    let digest = v["Digest"].as_str()?.strip_prefix("sha256:")?;
                    if !anonymous(digest) { return None; }
                    format!("{repository}@sha256:{digest}")
                }
                tag => format!("{repository}:{tag}"),
            };
            Some(json!({"name": name, "size": v["Size"], "have": asagiri_images.contains(&name)}))
        })
        .collect();
    let volumes: Vec<Value> = json_lines(ctx, &["volume", "ls", "--format", "{{json .}}"])?
        .into_iter()
        .filter_map(|v| v["Name"].as_str().map(str::to_owned))
        .map(|name| json!({"name": name, "anonymous": anonymous(&name), "have": asagiri_volumes.contains(&name)}))
        .collect();
    let containers: Vec<Value> = json_lines(ctx, &["ps", "-a", "--format", "{{json .}}"])?
        .into_iter()
        .map(|v| {
            let name = v["Names"].as_str().unwrap_or("").to_owned();
            json!({"name": name, "image": v["Image"], "state": v["State"], "have": asagiri_containers.contains(&name)})
        })
        .collect();
    Ok(json!({"source": ctx, "images": images, "volumes": volumes, "containers": containers}))
}

/// `docker --context src save IMAGE | docker --context asagiri load`.
pub fn image(source: &str, name: &str) -> io::Result<()> {
    image_with_cli(known(source)?, name, &cli).map(|_| ())
}

fn image_with_cli(ctx: &str, name: &str, make_cli: &impl Fn() -> io::Result<Command>) -> io::Result<Option<String>> {
    let mut save = make_cli()?.args(["--context", ctx, "save", name]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let out = save.stdout.take().ok_or_else(|| io::Error::other("no output from docker save"))?;
    let load = make_cli()?.args(["--context", crate::flavor::CONTEXT.get(), "load", "-q"]).stdin(out).stdout(Stdio::piped()).stderr(Stdio::piped()).output()?;
    let saved = save.wait_with_output()?;
    if !saved.status.success() {
        return Err(io::Error::other(format!("{ctx}: {}", String::from_utf8_lossy(&saved.stderr).trim())));
    }
    if !load.status.success() {
        return Err(io::Error::other(format!("asagiri: {}", String::from_utf8_lossy(&load.stderr).trim())));
    }
    let output = String::from_utf8_lossy(&load.stdout);
    let mut lines = output.lines().filter(|line| line.starts_with("Loaded image ID: ") || line.starts_with("Loaded image: "));
    let loaded_id = lines.next().and_then(|line| line.strip_prefix("Loaded image ID: "));
    Ok(loaded_id.filter(|id| lines.next().is_none() && id.strip_prefix("sha256:").is_some_and(anonymous)).map(str::to_owned))
}

/// An image with `tar` that both engines have, for the volume helpers.
fn helper_image(ctx: &str, make_cli: &impl Fn() -> io::Result<Command>) -> io::Result<&'static str> {
    let has = |c: &str, img: &str| make_cli().ok()?.args(["--context", c, "image", "inspect", img]).stdout(Stdio::null()).stderr(Stdio::null()).status().ok().map(|s| s.success());
    for img in ["alpine:latest", "busybox:latest", "alpine:3.22", "debian:trixie", "ubuntu:24.04"] {
        if has(ctx, img) == Some(true) {
            if has(crate::flavor::CONTEXT.get(), img) != Some(true) {
                image_with_cli(ctx, img, make_cli)?;
            }
            return Ok(img);
        }
    }
    Err(io::Error::other(format!("{ctx} has no alpine or busybox image to read volumes with; pull alpine there, or import the volume by hand")))
}

/// A volume's files, owners kept: tar in a read-only helper on the source,
/// untar in a helper on Asagiri. Both helpers remove themselves. The source's
/// users must stay stopped throughout: these checks are not an atomic snapshot.
pub fn volume(source: &str, name: &str) -> io::Result<()> {
    volume_with_cli(known(source)?, name, &cli)
}

fn inspect_idle_volume(ctx: &str, name: &str, make_cli: &impl Fn() -> io::Result<Command>) -> io::Result<Value> {
    let inspect = |args: &[&str]| -> io::Result<Vec<u8>> {
        let output = make_cli()?.args(["--context", ctx]).args(args).output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!("{ctx}: could not check volume {name}: {}", String::from_utf8_lossy(&output.stderr).trim())));
        }
        Ok(output.stdout)
    };
    // A missing source must not be created implicitly by the read helper.
    let volume: Value = serde_json::from_slice(&inspect(&["volume", "inspect", "--format", "{{json .}}", "--", name])?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{ctx}: invalid volume inspection: {e}")))?;
    if volume["Name"].as_str() != Some(name) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{ctx}: volume inspection did not identify {name}")));
    }
    let filter = format!("volume={name}");
    let bytes = inspect(&["ps", "--all", "--filter", &filter, "--filter", "status=running",
        "--filter", "status=paused", "--filter", "status=restarting", "--format", "{{json .}}"])?;
    let text = std::str::from_utf8(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut users = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let row: Value = serde_json::from_str(line)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{ctx}: invalid volume users: {e}")))?;
        let user = row["Names"].as_str().filter(|name| !name.is_empty() && !name.chars().any(char::is_control));
        if !matches!(row["State"].as_str(), Some("running" | "paused" | "restarting")) || user.is_none() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{ctx}: could not identify an active user of volume {name}")));
        }
        users.push(user.unwrap().to_owned());
    }
    if !users.is_empty() {
        users.sort_unstable();
        users.dedup();
        return Err(io::Error::other(format!("{ctx}: volume {name} is used by active containers {}; stop them before importing and keep them stopped until the import finishes", users.join(", "))));
    }
    Ok(volume)
}

fn volume_with_cli(ctx: &str, name: &str, make_cli: &impl Fn() -> io::Result<Command>) -> io::Result<()> {
    let source = inspect_idle_volume(ctx, name, make_cli)?;
    let labels: Option<std::collections::BTreeMap<String, String>> = serde_json::from_value(source["Labels"].clone())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{ctx}: invalid volume labels: {e}")))?;
    let img = helper_image(ctx, make_cli)?;
    let mut create = make_cli()?;
    create.args(["--context", crate::flavor::CONTEXT.get(), "volume", "create"]);
    for (key, value) in labels.into_iter().flatten() {
        create.arg("--label").arg(format!("{key}={value}"));
    }
    let created = create.args(["--", name]).output()?;
    if !created.status.success() {
        return Err(io::Error::other(format!("asagiri: could not create volume {name}: {}", String::from_utf8_lossy(&created.stderr).trim())));
    }
    let src_mount = format!("{name}:/from:ro");
    let dst_mount = format!("{name}:/to");
    let mut read = make_cli()?
        .args(["--context", ctx, "run", "--rm", "--network", "none", "--label", "dev.asagiri.import-helper=1", "-v", &src_mount, img, "tar", "-C", "/from", "-cf", "-", "."])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let out = read.stdout.take().ok_or_else(|| io::Error::other("no output from the source helper"))?;
    let write = make_cli()?
        .args(["--context", crate::flavor::CONTEXT.get(), "run", "--rm", "-i", "--network", "none", "-v", &dst_mount, img, "tar", "-C", "/to", "-xpf", "-"])
        .stdin(out)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    let r = read.wait_with_output()?;
    if !r.status.success() {
        return Err(io::Error::other(format!("{ctx}: {}", String::from_utf8_lossy(&r.stderr).trim())));
    }
    if !write.status.success() {
        return Err(io::Error::other(format!("asagiri: {}", String::from_utf8_lossy(&write.stderr).trim())));
    }
    inspect_idle_volume(ctx, name, make_cli).map(drop).map_err(|e| io::Error::other(format!(
        "volume {name} was copied, but the source is no longer confirmed idle: {e}; stop the source users and inspect the destination before retrying")))
}

/// The container, made again in Asagiri from the source's `inspect`, stopped:
/// command, environment, labels, ports, mounts, restart policy and its
/// networks (made in Asagiri first if missing). Its image must be in Asagiri.
pub fn container(source: &str, name: &str) -> io::Result<()> {
    let ctx = known(source)?;
    let info = json_lines(ctx, &["inspect", "--type", "container", "--format", "{{json .}}", name])?.into_iter().next().ok_or_else(|| io::Error::other(format!("{name}: not found in {ctx}")))?;
    let cfg = &info["Config"];
    let mut image = cfg["Image"].as_str().unwrap_or("").to_owned();
    if json_lines(crate::flavor::CONTEXT.get(), &["image", "inspect", "--format", "{{json .Id}}", &image]).map_or(true, |v| v.is_empty()) {
        let loaded_id = image_with_cli(ctx, &image, &cli)?;
        // An untagged archive can lose the registry digest during load. Use the
        // content ID returned for this transfer, including on containerd sources
        // whose inspect ID identifies a manifest list instead of its config.
        if image.contains("@sha256:") {
            if let Some(id) = loaded_id {
                image = id;
            }
        }
    }
    let mut networks = serde_json::Map::new();
    let mode = info["HostConfig"]["NetworkMode"].as_str().unwrap_or("default").to_owned();
    for (net, ep) in info["NetworkSettings"]["Networks"].as_object().into_iter().flatten() {
        if !["bridge", "host", "none", "default"].contains(&net.as_str()) {
            let _ = cli()?.args(["--context", crate::flavor::CONTEXT.get(), "network", "create", net]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        // The source's own short ID is no name in Asagiri.
        let short = info["Id"].as_str().unwrap_or("").get(..12).unwrap_or("").to_owned();
        let aliases: Vec<&Value> = ep["Aliases"].as_array().into_iter().flatten().filter(|a| a.as_str() != Some(short.as_str())).collect();
        networks.insert(net.clone(), json!({"Aliases": aliases}));
    }
    let hc = &info["HostConfig"];
    let mut host = json!({
        "Binds": hc["Binds"], "PortBindings": hc["PortBindings"], "RestartPolicy": hc["RestartPolicy"], "Mounts": hc["Mounts"],
        "NetworkMode": mode, "Privileged": hc["Privileged"], "CapAdd": hc["CapAdd"], "CapDrop": hc["CapDrop"], "ExtraHosts": hc["ExtraHosts"],
        "Tmpfs": hc["Tmpfs"], "ReadonlyRootfs": hc["ReadonlyRootfs"], "SecurityOpt": hc["SecurityOpt"], "Memory": hc["Memory"],
        "NanoCpus": hc["NanoCpus"], "ShmSize": hc["ShmSize"], "PublishAllPorts": hc["PublishAllPorts"],
    });
    if let Some(o) = host.as_object_mut() {
        o.retain(|_, v| !v.is_null());
    }
    let body = json!({
        "Image": image, "Cmd": cfg["Cmd"], "Entrypoint": cfg["Entrypoint"], "Env": cfg["Env"], "Labels": cfg["Labels"],
        "WorkingDir": cfg["WorkingDir"], "User": cfg["User"], "ExposedPorts": cfg["ExposedPorts"], "Tty": cfg["Tty"],
        "OpenStdin": cfg["OpenStdin"], "StopSignal": cfg["StopSignal"], "Hostname": cfg["Hostname"],
        "HostConfig": host, "NetworkingConfig": {"EndpointsConfig": networks},
    });
    let r = client::request("POST", &format!("/containers/create?name={name}"), Some(&body))?;
    if !r.ok() {
        return Err(io::Error::other(r.error()));
    }
    Ok(())
}

/// Whether `ctx`'s engine answers.
fn answers(ctx: &str) -> bool {
    cli().is_ok_and(|mut c| c.args(["--context", ctx, "version", "--format", "{{.Server.Version}}"]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success()))
}

/// The engine to import from: the one named, or else the first that
/// answers. Only the engines that need it are asked (`answers` waits on
/// each), and one that isn't there gets a plain message instead of docker's
/// "context not found" (#266).
fn pick(given: Option<&str>, answers: impl Fn(&str) -> bool) -> io::Result<&'static str> {
    let label = |id: &'static str| SOURCES.iter().find(|(c, _)| *c == id).map_or(id, |(_, l)| *l);
    match given {
        Some(id) => {
            let id = known(id)?;
            if answers(id) {
                Ok(id)
            } else {
                Err(io::Error::other(format!("{} isn't installed or running here (no docker context \"{id}\" answering)", label(id))))
            }
        }
        None => SOURCES.iter().map(|(c, _)| *c).find(|c| answers(c)).ok_or_else(|| {
            let names: Vec<&str> = SOURCES.iter().map(|(_, l)| *l).collect();
            io::Error::other(format!("nothing to import from: {} isn't installed or running here", names.join(" or ")))
        }),
    }
}

const KINDS: [&str; 3] = ["image", "volume", "container"];
const USAGE: &str = "usage: asagiri import [orbstack|desktop-linux] [--list | image|volume|container NAME]";

/// What `asagiri import` was asked to do.
#[derive(Debug, PartialEq)]
enum Ask<'a> {
    Everything,
    List,
    One(&'a str, &'a str),
}

/// The source named (if any) and what to do. A kind without exactly one
/// name, `--list` with an item, or a flag Asagiri doesn't know is a usage
/// error, never "import everything".
fn parse<'a>(args: &[&'a str]) -> io::Result<(Option<&'a str>, Ask<'a>)> {
    let usage = || io::Error::other(USAGE);
    let mut list = false;
    let mut words = Vec::new();
    for a in args {
        match *a {
            "--list" => list = true,
            f if f.starts_with('-') => return Err(usage()),
            w => words.push(w),
        }
    }
    let (given, rest) = match words.split_first() {
        Some((first, rest)) if !KINDS.contains(first) => (Some(*first), rest),
        _ => (None, &words[..]),
    };
    let ask = match (rest, list) {
        ([], false) => Ask::Everything,
        ([], true) => Ask::List,
        ([kind, name], false) if KINDS.contains(kind) => Ask::One(kind, name),
        _ => return Err(usage()),
    };
    Ok((given, ask))
}

/// `asagiri import [orbstack|desktop-linux] [--list | image|volume|container NAME]`:
/// one item, or every image, named volume and container Asagiri lacks.
pub fn run(args: &[String]) -> io::Result<()> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (given, ask) = parse(&args)?;
    let source = pick(given, answers)?;
    if given.is_none() {
        let label = SOURCES.iter().find(|(c, _)| *c == source).map_or(source, |(_, l)| *l);
        println!("from {label} ({source})");
    }
    if let Ask::One(kind, name) = ask {
        let r = match kind {
            "image" => image(source, name),
            "volume" => volume(source, name),
            _ => container(source, name),
        };
        return r.map(|()| println!("{kind} {name}: done"));
    }
    let inv = inventory(source)?;
    let list = |k: &str| inv[k].as_array().cloned().unwrap_or_default();
    if ask == Ask::List {
        for k in ["images", "volumes", "containers"] {
            println!("{k}:");
            for v in list(k) {
                println!("  {}{}", v["name"].as_str().unwrap_or(""), if v["have"] == true { "  (already in Asagiri)" } else { "" });
            }
        }
        return Ok(());
    }
    let mut failed = 0;
    let mut step = |what: &str, name: &str, r: io::Result<()>| match r {
        Ok(()) => println!("{what} {name}: done"),
        Err(e) => {
            failed += 1;
            println!("{what} {name}: {e}");
        }
    };
    for v in list("images").iter().filter(|v| v["have"] != true) {
        let n = v["name"].as_str().unwrap_or("");
        step("image", n, image(source, n));
    }
    for v in list("volumes").iter().filter(|v| v["have"] != true && v["anonymous"] != true) {
        let n = v["name"].as_str().unwrap_or("");
        step("volume", n, volume(source, n));
    }
    for v in list("containers").iter().filter(|v| v["have"] != true) {
        let n = v["name"].as_str().unwrap_or("");
        step("container", n, container(source, n));
    }
    if failed > 0 { Err(io::Error::other(format!("{failed} items could not be imported"))) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    struct VolumeFixture(std::path::PathBuf);

    #[cfg(unix)]
    impl VolumeFixture {
        fn new(case: &str) -> Self {
            use std::os::unix::fs::PermissionsExt;
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!("asagiri-import-{}-{}",
                std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
            std::fs::create_dir(&root).unwrap();
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::write(root.join("case"), case).unwrap();
            std::fs::write(root.join("docker.sh"), include_str!("../../../tests/fixtures/import/docker.sh")).unwrap();
            Self(root)
        }

        fn run(&self) -> std::io::Result<()> {
            super::volume_with_cli("orbstack", "fixture-volume", &|| {
                let mut command = std::process::Command::new("/bin/sh");
                command.arg(self.0.join("docker.sh")).env("ASAGIRI_IMPORT_FIXTURE", &self.0)
                    .env("ASAGIRI_IMPORT_DESTINATION", crate::flavor::CONTEXT.get());
                Ok(command)
            })
        }

        fn trace(&self) -> String {
            std::fs::read_to_string(self.0.join("trace")).unwrap()
        }
    }

    #[cfg(unix)]
    impl Drop for VolumeFixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn volume_import_preserves_labels_on_creation() {
        VolumeFixture::new("labels").run().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn volume_import_rejects_active_sources_before_any_helpers() {
        for state in ["running", "paused", "restarting"] {
            let fixture = VolumeFixture::new(state);
            let error = fixture.run().expect_err("an active source volume was copied").to_string();
            assert!(error.contains("fixture-writer") && error.contains("stop"), "{state}: {error}");
            let trace = fixture.trace();
            assert!(!trace.contains(" image ") && !trace.contains(" volume create ") && !trace.contains(" run "), "{trace}");
            assert!(!fixture.0.join("copied").exists());
            eprintln!("verified active {state}: no helper, destination or copy");
        }
    }

    #[cfg(unix)]
    #[test]
    fn volume_import_preflight_failures_never_start_a_copy() {
        for case in ["inspect-error", "wrong-volume", "list-error", "invalid-json", "unknown-state"] {
            let fixture = VolumeFixture::new(case);
            assert!(fixture.run().is_err(), "{case} was treated as an idle source");
            let trace = fixture.trace();
            assert!(!trace.contains(" image ") && !trace.contains(" volume create ") && !trace.contains(" run "), "{trace}");
            assert!(!fixture.0.join("copied").exists());
            eprintln!("verified {case}: no destination or copy");
        }
    }

    #[cfg(unix)]
    #[test]
    fn volume_import_checks_creation_and_rechecks_the_source() {
        let idle = VolumeFixture::new("idle");
        idle.run().unwrap();
        assert_eq!(std::fs::read(idle.0.join("copied")).unwrap(), b"\0volume fixture\xff\n");
        assert_eq!(idle.trace().matches(" ps --all ").count(), 2);
        assert!(idle.trace().contains("fixture-volume:/from:ro"));
        for case in ["create-error", "source-error", "write-error", "late-writer", "late-inspect-error"] {
            let fixture = VolumeFixture::new(case);
            assert!(fixture.run().is_err(), "{case} was reported as a completed import");
            if case == "create-error" {
                assert!(!fixture.trace().contains(" run "));
                assert!(!fixture.0.join("copied").exists());
            }
            if case.starts_with("late-") {
                assert_eq!(std::fs::read(fixture.0.join("copied")).unwrap(), b"\0volume fixture\xff\n");
            }
            eprintln!("verified {case}: import did not report success");
        }
    }

    #[test]
    fn anonymous_volumes() {
        assert!(super::anonymous(&"a1".repeat(32)));
        assert!(!super::anonymous("pgdata"));
    }

    // #266: a fresh machine with neither engine gets a plain answer, not
    // docker's "context not found"; with no source named, the first that
    // answers is used, and only as many engines are asked as needed.
    #[test]
    fn picks_a_running_source() {
        use super::pick;
        let none = |_: &str| false;
        let only_desktop = |c: &str| c == "desktop-linux";
        if cfg!(windows) {
            assert_eq!(pick(None, none).unwrap_err().to_string(), "nothing to import from: Docker Desktop isn't installed or running here");
        } else {
            assert_eq!(pick(None, none).unwrap_err().to_string(), "nothing to import from: OrbStack or Docker Desktop isn't installed or running here");
            assert!(pick(Some("orbstack"), only_desktop).is_err());
            // Named: only that engine is asked.
            let asked = std::cell::RefCell::new(Vec::new());
            assert_eq!(pick(Some("desktop-linux"), |c: &str| { asked.borrow_mut().push(c.to_owned()); true }).unwrap(), "desktop-linux");
            assert_eq!(*asked.borrow(), ["desktop-linux"]);
            // Not named: stops at the first that answers.
            asked.borrow_mut().clear();
            assert_eq!(pick(None, |c: &str| { asked.borrow_mut().push(c.to_owned()); true }).unwrap(), "orbstack");
            assert_eq!(*asked.borrow(), ["orbstack"]);
        }
        assert_eq!(pick(None, only_desktop).unwrap(), "desktop-linux");
        assert!(pick(Some("desktop-linux"), none).unwrap_err().to_string().starts_with("Docker Desktop isn't installed or running here"));
        // An unknown source is refused before any engine is asked.
        assert_eq!(pick(Some("podman"), |_: &str| panic!("asked")).unwrap_err().to_string(), "unknown source podman");
    }

    // A kind without exactly one name is a usage error, never "import
    // everything"; the source may be left out before a kind.
    #[test]
    fn parses_what_to_import() {
        use super::{Ask, parse};
        assert_eq!(parse(&[]).unwrap(), (None, Ask::Everything));
        assert_eq!(parse(&["orbstack"]).unwrap(), (Some("orbstack"), Ask::Everything));
        assert_eq!(parse(&["--list"]).unwrap(), (None, Ask::List));
        assert_eq!(parse(&["orbstack", "--list"]).unwrap(), (Some("orbstack"), Ask::List));
        assert_eq!(parse(&["image", "nginx"]).unwrap(), (None, Ask::One("image", "nginx")));
        assert_eq!(parse(&["desktop-linux", "volume", "pg"]).unwrap(), (Some("desktop-linux"), Ask::One("volume", "pg")));
        for bad in [
            &["orbstack", "image"][..],
            &["image"],
            &["orbstack", "image", "a", "b"],
            &["orbstack", "photo", "a"],
            // --list with an item would import it.
            &["--list", "image", "nginx"],
            &["image", "nginx", "--list"],
            // An unknown flag must not start a full import.
            &["--dry-run"],
            &["orbstack", "-l"],
        ] {
            assert!(parse(bad).unwrap_err().to_string().starts_with("usage:"), "{bad:?}");
        }
        assert_eq!(parse(&["podman"]).unwrap(), (Some("podman"), Ask::Everything));
    }
}
