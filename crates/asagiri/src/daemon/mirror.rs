//! Asagiri's images as a registry, for its BuildKit: `FROM myapp:dev` (an
//! image built or loaded here, never pushed) works as it does with Docker.
//!
//! A read-only registry API on 127.0.0.1, which containers reach at
//! host.docker.internal. buildkitd.toml names it as a mirror of every
//! registry Asagiri has images from; a mirror that answers 404 is skipped, so
//! anything not here comes from the registry itself. A tag a registry
//! published is not answered here even when Asagiri holds it: the registry,
//! not this machine, knows which platforms it has (see `lookup`).
//! Paths carry a secret
//! (`/v2/<token>/<registry>/<repo>/…`), so other local users can't read
//! images through it. Manifests are made from the store's records (layers,
//! config), the same bytes for the same image every time.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use serde_json::{Value, json};

use asagiri_image::{Image, Reference};

use super::engine::Engine;

/// Tried first, so a builder started by buildx after a daemon restart still
/// finds the mirror its config names.
const PORT: u16 = 47_391;
const DEV_PORT: u16 = 47_392;

static LISTENING: OnceLock<Option<u16>> = OnceLock::new();

/// The mirror's secret path segment, kept in the state directory so it
/// survives restarts (the builder's config keeps naming it).
fn token() -> String {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN
        .get_or_init(|| {
            let path = super::state_dir().ok().map(|d| d.join("build-mirror-token"));
            if let Some(t) = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| t.trim().to_owned()).filter(|t| t.len() == 32) {
                return t;
            }
            let t = super::engine::random_hex(16);
            if let Some(p) = path {
                let _ = save_token(&p, &t);
            }
            t
        })
        .clone()
}

/// Writes the token readable by its owner only from the moment the file
/// exists: written and then chmod-ed, it was the umask's 0644 in between,
/// and other local users could read it there.
fn save_token(path: &std::path::Path, token: &str) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    // One already there (not a valid token) is replaced, mode and all.
    let _ = std::fs::remove_file(path);
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?.write_all(token.as_bytes())
}

/// Starts the server (at the daemon's start: a listening socket costs
/// nothing while no build asks).
pub fn start(engine: &Arc<Engine>) {
    LISTENING.get_or_init(|| {
        let wanted = if crate::flavor::dev() { DEV_PORT } else { PORT };
        let listener = TcpListener::bind(("127.0.0.1", wanted)).or_else(|_| TcpListener::bind(("127.0.0.1", 0))).ok()?;
        let port = listener.local_addr().ok()?.port();
        let me = Arc::downgrade(engine);
        // On a Linux host the builder reaches the mirror at the default
        // bridge's gateway (mirror_host): containers have no way to the
        // host's loopback there, so it listens on that address too.
        #[cfg(target_os = "linux")]
        if let Some(gateway) = bridge_gateway(&engine.lock_net()) {
            match bind_freely(gateway, port) {
                Ok(l) => {
                    let me = me.clone();
                    let _ = std::thread::Builder::new().name("asa-build-mirro".into()).spawn(move || serve(l, me));
                }
                Err(e) => crate::diag::warn!("image mirror not on the bridge; builds pull from registries", error = e),
            }
        }
        std::thread::Builder::new().name("asa-build-mirro".into()).spawn(move || serve(listener, me)).ok()?;
        crate::diag::info!("image mirror for builds", port = port);
        Some(port)
    });
}

/// buildkitd.toml lines: this mirror for every registry Asagiri has images
/// from (Docker Hub always), over plain HTTP.
#[cfg(target_os = "linux")]
fn mirror_host(networks: &super::network::Networks) -> String {
    bridge_gateway(networks).map_or_else(|| super::ports::HOST_ADDR.into(), super::network::ip_str)
}

/// The default bridge's gateway, which containers reach the host at.
#[cfg(target_os = "linux")]
fn bridge_gateway(networks: &super::network::Networks) -> Option<[u8; 4]> {
    networks.find("bridge").map(|n| n.gateway).filter(|g| *g != [0; 4])
}

/// A listener on `ip:port` before the address exists: the bridge (and
/// its gateway address) is made with the first container (IP_FREEBIND).
#[cfg(target_os = "linux")]
fn bind_freely(ip: [u8; 4], port: u16) -> io::Result<TcpListener> {
    use std::os::fd::{FromRawFd, OwnedFd};
    // SAFETY: plain socket creation; owned at once.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is fresh and ours.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    let one: libc::c_int = 1;
    for opt in [(libc::SOL_SOCKET, libc::SO_REUSEADDR), (libc::IPPROTO_IP, libc::IP_FREEBIND)] {
        // SAFETY: setsockopt with an int of the stated size on a socket we own.
        if unsafe { libc::setsockopt(fd, opt.0, opt.1, (&one as *const libc::c_int).cast(), std::mem::size_of::<libc::c_int>() as libc::socklen_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: sockaddr_in is plain data; all-zero is valid, then filled in.
    let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sin.sin_family = libc::AF_INET as libc::sa_family_t;
    sin.sin_port = port.to_be();
    sin.sin_addr.s_addr = u32::from_ne_bytes(ip);
    // SAFETY: bind and listen on our socket with a sockaddr_in of its size.
    unsafe {
        if libc::bind(fd, (&sin as *const libc::sockaddr_in).cast(), std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t) != 0 || libc::listen(fd, 128) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(TcpListener::from(sock))
}

#[cfg(not(target_os = "linux"))]
fn mirror_host(_networks: &super::network::Networks) -> String {
    super::ports::HOST_ADDR.into()
}

/// Where a build reaches this computer: the address `--add-host
/// x:host-gateway` gets, the one the mirror is reached at. On a Linux host
/// (and Windows' engine in WSL2) that's the default bridge's gateway, as
/// for `docker run`; 198.19.248.254 only leads anywhere in the Mac's VM.
pub fn host_addr(engine: &Engine) -> String {
    mirror_host(&engine.lock_net())
}

/// Adds `mirror` to `registry`'s list, in order, once.
fn add_mirror(registries: &mut Vec<(String, Vec<String>)>, registry: &str, mirror: String) {
    match registries.iter_mut().find(|(r, _)| r == registry) {
        Some((_, mirrors)) => {
            if !mirrors.contains(&mirror) {
                mirrors.push(mirror);
            }
        }
        None => registries.push((registry.to_owned(), vec![mirror])),
    }
}

pub fn config(engine: &Engine, configured: &super::registry_config::Config) -> String {
    // One table per registry: BuildKit refuses a buildkitd.toml that names
    // one twice, and Asagiri's own mirror and Settings' may both cover Hub.
    let mut registries: Vec<(String, Vec<String>)> = Vec::new();
    let mut http: Vec<String> = Vec::new();
    if let Some(port) = LISTENING.get().copied().flatten() {
        let host = format!("{}:{port}", mirror_host(&engine.lock_net()));
        let token = token();
        let mut names = vec!["docker.io".to_owned()];
        for i in engine.store.images().unwrap_or_default() {
            if let Ok(r) = Reference::parse(&i.name)
                && !names.contains(&r.registry)
                && !asagiri_image::reference::is_local(&r.registry)
            {
                names.push(r.registry);
            }
        }
        for name in names {
            add_mirror(&mut registries, &name, format!("{host}/{token}/{name}"));
        }
        http.push(host);
    }
    // Settings → Registry mirrors: Docker Hub only, as Docker's are. They
    // come after Asagiri's own mirror, which serves images built or loaded
    // here and answers 404 for everything else.
    for mirror in &configured.mirrors {
        add_mirror(&mut registries, "docker.io", mirror.buildkit());
        if mirror.http && !http.contains(&mirror.host) {
            http.push(mirror.host.clone());
        }
    }
    render_config(registries, http, &configured.settings)
}

fn render_config(registries: Vec<(String, Vec<String>)>, http: Vec<String>, settings: &asagiri_image::RegistrySettings) -> String {
    let mut entries = std::collections::BTreeMap::<String, (Vec<String>, bool, bool)>::new();
    for (registry, mirrors) in registries {
        for mirror in &mirrors {
            let host = mirror.split('/').next().unwrap_or(mirror);
            if settings.allows_insecure(host) { entries.entry(host.into()).or_default().2 = true; }
        }
        let entry = entries.entry(registry.clone()).or_default();
        entry.0 = mirrors;
        entry.2 = settings.allows_insecure(&registry);
    }
    for host in http { entries.entry(host).or_default().1 = true; }
    for host in settings.authorities() { entries.entry(host.into()).or_default().2 = true; }
    for host in settings.ca_certificates().keys() {
        entries.entry(host.clone()).or_default();
        // Docker references may spell the default HTTPS port explicitly.
        let parsed: ureq::http::uri::Authority = host.parse().expect("validated CA authority");
        if parsed.port().is_none() { entries.entry(format!("{host}:443")).or_default(); }
    }
    let mut out = String::new();
    for (registry, (mirrors, http, insecure)) in entries {
        out.push_str(&format!("[registry.{}]\n", serde_json::to_string(&registry).unwrap()));
        if !mirrors.is_empty() {
            out.push_str(&format!("  mirrors = {}\n", serde_json::to_string(&mirrors).unwrap()));
        }
        // BuildKit's two flags together select HTTPS with a self-signed
        // certificate allowed, then HTTP fallback, for this authority only.
        if http || insecure { out.push_str("  http = true\n"); }
        if insecure { out.push_str("  insecure = true\n"); }
        if let Some(pem) = settings.ca_certificates().get(registry.strip_suffix(":443").unwrap_or(&registry)) {
            out.push_str(&format!("  ca = [{}]\n", serde_json::to_string(&ca_path(pem)).unwrap()));
        }
    }
    out
}

/// A content-addressed filename; no user-controlled authority enters a path.
pub fn ca_path(pem: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest: String = Sha256::digest(pem.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect();
    format!("/etc/buildkit/registry-ca-{digest}.pem")
}

fn serve(listener: TcpListener, engine: Weak<Engine>) {
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let Some(engine) = engine.upgrade() else { return };
        let _ = std::thread::Builder::new().name("asa-build-mirro".into()).spawn(move || {
            let _ = conn.set_read_timeout(Some(Duration::from_secs(30)));
            let _ = answer(conn, &engine);
        });
    }
}

/// One request, then the connection closes.
fn answer(conn: TcpStream, engine: &Engine) -> io::Result<()> {
    let mut r = BufReader::new((&conn).take(16 << 10));
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or_default().to_owned(), parts.next().unwrap_or_default().to_owned());
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
    }
    let mut w = &conn;
    let head = |w: &mut &TcpStream, status: &str, extra: &str, len: u64| write!(w, "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {len}\r\n{extra}\r\n");
    if method != "GET" && method != "HEAD" {
        return head(&mut w, "405 Method Not Allowed", "", 0);
    }
    match lookup(&engine.store, target.split('?').next().unwrap_or_default(), &token()) {
        Some(Found::Manifest(bytes, media_type, digest)) => {
            head(&mut w, "200 OK", &format!("Content-Type: {media_type}\r\nDocker-Content-Digest: {digest}\r\n"), bytes.len() as u64)?;
            if method == "GET" {
                w.write_all(&bytes)?;
            }
        }
        Some(Found::Blob(path, digest)) => {
            let mut f = std::fs::File::open(&path)?;
            let len = f.metadata()?.len();
            head(&mut w, "200 OK", &format!("Content-Type: application/octet-stream\r\nDocker-Content-Digest: {digest}\r\n"), len)?;
            if method == "GET" {
                io::copy(&mut f, &mut w)?;
            }
        }
        None => head(&mut w, "404 Not Found", "", 0)?,
    }
    w.flush()
}

const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const MAX_MANIFEST: u64 = 4 << 20;

/// Content-Type for raw manifest or index JSON `bytes`.
fn manifest_media_type(bytes: &[u8]) -> String {
    if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
        if let Some(mt) = v["mediaType"].as_str().map(str::trim).filter(|m| !m.is_empty() && !m.contains(['\r', '\n'])) {
            return mt.to_owned();
        }
        if v.get("manifests").is_some() {
            return OCI_INDEX.to_owned();
        }
    }
    OCI_MANIFEST.to_owned()
}

/// Reads a raw upstream manifest or multi-arch index blob saved during `pull`,
/// verifying that its bytes hash to `digest`.
fn stored_manifest(store: &asagiri_image::Store, digest: &str) -> Option<(Vec<u8>, String)> {
    let path = store.blob_path(digest).ok()?;
    let len = std::fs::metadata(&path).ok()?.len();
    if len > MAX_MANIFEST {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    (asagiri_image::store::sha256_digest(&bytes) == digest).then(|| {
        let media_type = manifest_media_type(&bytes);
        (bytes, media_type)
    })
}

#[derive(Debug)]
enum Found {
    Manifest(Vec<u8>, String, String),
    Blob(std::path::PathBuf, String),
}

/// An image's manifest as Asagiri serves it, and its digest.
fn manifest(store: &asagiri_image::Store, i: &Image) -> Option<(Vec<u8>, String)> {
    let config_size = std::fs::metadata(store.blob_path(&i.config_digest).ok()?).ok()?.len();
    let m = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": i.config_digest, "size": config_size},
        "layers": i.layers.iter().map(|l| json!({"mediaType": l.media_type, "digest": l.digest, "size": l.size})).collect::<Vec<Value>>(),
    });
    let bytes = serde_json::to_vec(&m).ok()?;
    let digest = asagiri_image::store::sha256_digest(&bytes);
    Some((bytes, digest))
}

/// `/v2/<token>/<registry>/<repo…>/manifests/<tag|digest>` or `…/blobs/<digest>`.
fn parse(path: &str, token: &str) -> Option<(String, String, bool, String)> {
    let rest = path.strip_prefix("/v2/")?.strip_prefix(token)?.strip_prefix('/')?;
    let (registry, rest) = rest.split_once('/')?;
    let (repo, kind, reference) = if let Some((repo, r)) = rest.rsplit_once("/manifests/") {
        (repo, true, r)
    } else {
        let (repo, r) = rest.rsplit_once("/blobs/")?;
        (repo, false, r)
    };
    (!repo.is_empty() && !reference.is_empty() && !reference.contains('/')).then(|| (registry.to_owned(), repo.to_owned(), kind, reference.to_owned()))
}

/// Local names stay available even when another tag of the same image has
/// a registry digest. Tags pulled or pushed under this exact name go to
/// the registry, which knows all their platforms and their current value.
fn serve_tag(i: &Image) -> bool {
    i.local_tag || i.repo_digest().is_none()
}

fn lookup(store: &asagiri_image::Store, path: &str, token: &str) -> Option<Found> {
    let (registry, repo, is_manifest, reference) = parse(path, token)?;
    let images = store.images().ok()?;
    let in_repo = |i: &&Image| Reference::parse(&i.name).is_ok_and(|n| n.registry == registry && n.repository == repo);
    if is_manifest {
        if reference.starts_with("sha256:") {
            if images.iter().filter(in_repo).any(|i| (i.digest != i.config_digest && i.digest == reference) || i.index_digest.as_deref() == Some(reference.as_str()))
                && let Some((bytes, media_type)) = stored_manifest(store, &reference)
            {
                return Some(Found::Manifest(bytes, media_type, reference));
            }
            return images.iter().filter(in_repo).find_map(|i| manifest(store, i).filter(|(_, d)| *d == reference)).map(|(b, d)| Found::Manifest(b, OCI_MANIFEST.to_owned(), d));
        }
        let wanted = Reference::parse(&format!("{registry}/{repo}:{reference}")).ok()?.short();
        // A tag a registry published is the registry's to answer: Asagiri holds
        // the one platform it pulled, and serving that says the tag has no
        // other, so a build for another platform takes these binaries and
        // fails on them (issue #57). 404 here makes the builder skip this
        // mirror and pull the platform it asked for.
        let i = images.iter().find(|i| i.name == wanted && serve_tag(i))?;
        let (b, d) = manifest(store, i)?;
        return Some(Found::Manifest(b, OCI_MANIFEST.to_owned(), d));
    }
    // Only blobs of that repository's images.
    let owned = images.iter().filter(in_repo).any(|i| i.config_digest == reference || i.layers.iter().any(|l| l.digest == reference));
    let path = store.blob_path(&reference).ok()?;
    (owned && path.is_file()).then_some(Found::Blob(path, reference))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_ca_tables_keep_https_verification_and_bind_certificate_bytes() {
        let ca = include_str!("../../../asagiri-image/tests/fixtures/registry-ca/ca.pem");
        let settings = asagiri_image::RegistrySettings::default().with_ca_certificates(&[("registry.example:443".into(), ca.into())].into()).unwrap();
        let config = render_config(vec![], vec![], &settings);
        assert!(config.contains("[registry.\"registry.example\"]"));
        assert!(config.contains("[registry.\"registry.example:443\"]"));
        assert_eq!(config.matches(&ca_path(ca)).count(), 2);
        assert!(!config.contains("insecure"));
        assert!(!config.contains("http ="));
        assert_ne!(ca_path(ca), ca_path(include_str!("../../../asagiri-image/tests/fixtures/registry-ca/other-ca.pem")));
    }

    #[test]
    fn digest_lookup_serves_exact_registry_bytes_only_for_the_owning_repository() {
        let dir = std::env::temp_dir().join(format!("asagiri-mirror-pins-{}", std::process::id()));
        let store = asagiri_image::Store::open(&dir).unwrap();
        let raw = br#"{ "schemaVersion": 2, "manifests": [] }"#;
        let mut writer = store.ingest().unwrap();
        writer.write_all(raw).unwrap();
        let (digest, _) = writer.finish().unwrap();
        store.put_image(Image { name: "app:latest".into(), index_digest: Some(digest.clone()), ..made_here() }).unwrap();
        let request = |repo| lookup(&store, &format!("/v2/tok/docker.io/library/{repo}/manifests/{digest}"), "tok");
        assert!(matches!(request("app"), Some(Found::Manifest(bytes, media, hash)) if bytes == raw && media == OCI_INDEX && hash == digest));
        assert!(request("other").is_none());
        std::fs::write(store.blob_path(&digest).unwrap(), b"corrupt").unwrap();
        assert!(request("app").is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn parses_mirror_paths() {
        assert_eq!(parse("/v2/tok/docker.io/library/myapp/manifests/dev", "tok"), Some(("docker.io".into(), "library/myapp".into(), true, "dev".into())));
        assert_eq!(parse("/v2/tok/ghcr.io/o/r/blobs/sha256:ab", "tok"), Some(("ghcr.io".into(), "o/r".into(), false, "sha256:ab".into())));
        assert_eq!(parse("/v2/wrong/docker.io/library/a/manifests/x", "tok"), None);
        assert_eq!(parse("/v2/tok/docker.io/manifests/x", "tok"), None);
        assert_eq!(parse("/v2/", "tok"), None);
    }

    #[test]
    fn mirror_host_matches_the_host_network() {
        let dir = std::env::temp_dir().join(format!("asagiri-mirror-host-{}", std::process::id()));
        let networks = super::super::network::Networks::load(&dir).unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(mirror_host(&networks), super::super::network::ip_str(networks.find("bridge").unwrap().gateway));
        #[cfg(not(target_os = "linux"))]
        assert_eq!(mirror_host(&networks), super::super::ports::HOST_ADDR);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tag_a_registry_published_is_left_to_it() {
        // Pulled: its digest is its manifest's, so a registry has it.
        let pulled = Image { name: "alpine:3.20".into(), digest: "sha256:manifest".into(), local_tag: false, ..made_here() };
        assert!(!serve_tag(&pulled));
        // Built or loaded here: nothing else can serve it, platform or not.
        assert!(serve_tag(&made_here()));
        // Tagged into another repository (docker tag alpine:3.20 mybase:dev):
        // that name is not in any registry, so the mirror answers for it.
        assert!(serve_tag(&Image { name: "mybase:dev".into(), ..made_here() }));
    }

    #[test]
    fn a_local_tag_in_the_pulled_repository_is_still_available() {
        let pulled = Image { name: "alpine:3.20".into(), digest: "sha256:manifest".into(), local_tag: false, ..made_here() };
        let local = pulled.tagged("alpine:my-local-base".into());
        assert_eq!(local.repo_digest(), pulled.repo_digest());
        assert!(serve_tag(&local), "repository digests do not mean this local tag was published");
        assert!(!serve_tag(&pulled.tagged(pulled.name.clone())));
    }

    #[test]
    fn old_image_records_do_not_shadow_registry_platforms() {
        let mut stored = serde_json::to_value(made_here()).unwrap();
        stored.as_object_mut().unwrap().remove("local_tag");
        stored["digest"] = json!("sha256:registry-manifest");
        let pulled: Image = serde_json::from_value(stored.clone()).unwrap();
        assert!(!serve_tag(&pulled));
        stored["digest"] = stored["config_digest"].clone();
        let local: Image = serde_json::from_value(stored).unwrap();
        assert!(serve_tag(&local));
    }

    /// An image built here: no registry has it.
    fn made_here() -> Image {
        let config_digest = "sha256:config".to_owned();
        Image {
            name: "myapp:dev".into(),
            digest: config_digest.clone(),
            index_digest: None,
            local_tag: true,
            config_digest,
            layers: Vec::new(),
            size: 0,
            platform: "linux/amd64".into(),
            created: String::new(),
            pulled_at: 0,
            config: asagiri_image::RunConfig::default(),
        }
    }

    #[test]
    fn token_file_is_private_from_the_start() {
        use std::os::unix::fs::PermissionsExt;
        let p = std::env::temp_dir().join(format!("asagiri-mirror-token-{}", std::process::id()));
        // A leftover readable by everyone is replaced, not reused with its mode.
        std::fs::write(&p, "old").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        save_token(&p, "0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "0123456789abcdef0123456789abcdef");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn registry_transport_settings_share_one_table_with_mirrors() {
        let settings = asagiri_image::RegistrySettings::new(&["registry.example:5000".into(), "docker.io".into()], 4, 5).unwrap();
        let config = render_config(vec![("docker.io".into(), vec!["registry.example:5000/cache".into()])],
            vec!["127.0.0.1:1234".into()], &settings);
        assert_eq!(config.matches("[registry.\"docker.io\"]").count(), 1);
        assert_eq!(config.matches("[registry.\"registry.example:5000\"]").count(), 1);
        assert!(config.contains("mirrors = [\"registry.example:5000/cache\"]"));
        assert_eq!(config.matches("insecure = true").count(), 2);
        assert_eq!(config.matches("http = true").count(), 3);
        assert!(!config.contains("registry.example:5001"));
    }
}
