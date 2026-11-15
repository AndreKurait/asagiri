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
    let images: Vec<Value> = json_lines(ctx, &["image", "ls", "--format", "{{json .}}"])?
        .into_iter()
        .filter(|v| v["Repository"] != "<none>" && v["Tag"] != "<none>")
        .map(|v| {
            let name = format!("{}:{}", v["Repository"].as_str().unwrap_or(""), v["Tag"].as_str().unwrap_or(""));
            json!({"name": name, "size": v["Size"], "have": asagiri_images.contains(&name)})
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
    let ctx = known(source)?;
    let mut save = cli()?.args(["--context", ctx, "save", name]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let out = save.stdout.take().ok_or_else(|| io::Error::other("no output from docker save"))?;
    let load = cli()?.args(["--context", crate::flavor::CONTEXT.get(), "load", "-q"]).stdin(out).stdout(Stdio::null()).stderr(Stdio::piped()).output()?;
    let saved = save.wait_with_output()?;
    if !saved.status.success() {
        return Err(io::Error::other(format!("{ctx}: {}", String::from_utf8_lossy(&saved.stderr).trim())));
    }
    if !load.status.success() {
        return Err(io::Error::other(format!("asagiri: {}", String::from_utf8_lossy(&load.stderr).trim())));
    }
    Ok(())
}

/// An image with `tar` that both engines have, for the volume helpers.
fn helper_image(ctx: &str) -> io::Result<&'static str> {
    let has = |c: &str, img: &str| cli().ok()?.args(["--context", c, "image", "inspect", img]).stdout(Stdio::null()).stderr(Stdio::null()).status().ok().map(|s| s.success());
    for img in ["alpine:latest", "busybox:latest", "alpine:3.22", "debian:trixie", "ubuntu:24.04"] {
        if has(ctx, img) == Some(true) {
            if has(crate::flavor::CONTEXT.get(), img) != Some(true) {
                image(ctx, img)?;
            }
            return Ok(img);
        }
    }
    Err(io::Error::other(format!("{ctx} has no alpine or busybox image to read volumes with; pull alpine there, or import the volume by hand")))
}

/// A volume's files, owners kept: tar in a read-only helper on the source,
/// untar in a helper on Asagiri. Both helpers remove themselves.
pub fn volume(source: &str, name: &str) -> io::Result<()> {
    let ctx = known(source)?;
    let img = helper_image(ctx)?;
    let _ = cli()?.args(["--context", crate::flavor::CONTEXT.get(), "volume", "create", name]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    let src_mount = format!("{name}:/from:ro");
    let dst_mount = format!("{name}:/to");
    let mut read = cli()?
        .args(["--context", ctx, "run", "--rm", "--network", "none", "--label", "dev.asagiri.import-helper=1", "-v", &src_mount, img, "tar", "-C", "/from", "-cf", "-", "."])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let out = read.stdout.take().ok_or_else(|| io::Error::other("no output from the source helper"))?;
    let write = cli()?
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
    Ok(())
}

/// The container, made again in Asagiri from the source's `inspect`, stopped:
/// command, environment, labels, ports, mounts, restart policy and its
/// networks (made in Asagiri first if missing). Its image must be in Asagiri.
pub fn container(source: &str, name: &str) -> io::Result<()> {
    let ctx = known(source)?;
    let info = json_lines(ctx, &["inspect", "--type", "container", "--format", "{{json .}}", name])?.into_iter().next().ok_or_else(|| io::Error::other(format!("{name}: not found in {ctx}")))?;
    let cfg = &info["Config"];
    let image = cfg["Image"].as_str().unwrap_or("").to_owned();
    if json_lines(crate::flavor::CONTEXT.get(), &["image", "inspect", "--format", "{{json .Id}}", &image]).map_or(true, |v| v.is_empty()) {
        self::image(source, &image)?;
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

/// The source named (if any) and what to import: everything (`None`) or one
/// item. A kind without exactly one name is a usage error, never everything.
fn parse<'a>(words: &[&'a str]) -> io::Result<(Option<&'a str>, Option<(&'a str, &'a str)>)> {
    let (given, rest) = match words.split_first() {
        Some((first, rest)) if !KINDS.contains(first) => (Some(*first), rest),
        _ => (None, words),
    };
    match rest {
        [] => Ok((given, None)),
        [kind, name] if KINDS.contains(kind) => Ok((given, Some((*kind, *name)))),
        _ => Err(io::Error::other(USAGE)),
    }
}

/// `asagiri import [orbstack|desktop-linux] [--list | image|volume|container NAME]`:
/// one item, or every image, named volume and container Asagiri lacks.
pub fn run(args: &[String]) -> io::Result<()> {
    let words: Vec<&str> = args.iter().filter(|a| !a.starts_with('-')).map(String::as_str).collect();
    let (given, item) = parse(&words)?;
    let source = pick(given, answers)?;
    if given.is_none() {
        let label = SOURCES.iter().find(|(c, _)| *c == source).map_or(source, |(_, l)| *l);
        println!("from {label} ({source})");
    }
    if let Some((kind, name)) = item {
        let r = match kind {
            "image" => image(source, name),
            "volume" => volume(source, name),
            _ => container(source, name),
        };
        return r.map(|()| println!("{kind} {name}: done"));
    }
    let inv = inventory(source)?;
    let list = |k: &str| inv[k].as_array().cloned().unwrap_or_default();
    if args.iter().any(|a| a == "--list") {
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
        use super::parse;
        assert_eq!(parse(&[]).unwrap(), (None, None));
        assert_eq!(parse(&["orbstack"]).unwrap(), (Some("orbstack"), None));
        assert_eq!(parse(&["image", "nginx"]).unwrap(), (None, Some(("image", "nginx"))));
        assert_eq!(parse(&["desktop-linux", "volume", "pg"]).unwrap(), (Some("desktop-linux"), Some(("volume", "pg"))));
        for bad in [&["orbstack", "image"][..], &["image"], &["orbstack", "image", "a", "b"], &["orbstack", "photo", "a"]] {
            assert!(parse(bad).unwrap_err().to_string().starts_with("usage:"), "{bad:?}");
        }
        assert_eq!(parse(&["podman"]).unwrap(), (Some("podman"), None));
    }
}
