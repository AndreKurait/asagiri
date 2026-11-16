//! Register classic-build parents from the completed solve's provenance.
//! Never parse Dockerfiles/progress output, resolve a mutable tag again, or pull
//! as a side effect of image inspection. Buildx's separate-store behavior stays
//! unchanged; this is the Engine's classic POST /build completion contract.
use serde_json::Value;
use asagiri_image::{Client, Credentials, Platform, Reference};
use super::{Engine, Options, Request};

#[derive(Debug)]
struct Source {
    original: Reference,
    pinned: Reference,
    resolved: String,
    platform: Platform,
}

fn decode(value: &str) -> Result<String, String> {
    let mut bytes = Vec::new();
    let mut input = value.bytes();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let a = input.next().and_then(|b| (b as char).to_digit(16));
            let b = input.next().and_then(|b| (b as char).to_digit(16));
            bytes.push(match (a, b) { (Some(a), Some(b)) => (a * 16 + b) as u8,
                _ => return Err("invalid source package URL escape".into()) });
        } else { bytes.push(byte); }
    }
    String::from_utf8(bytes).map_err(|_| "invalid source package URL".into())
}

fn sources(provenance: &Value, default_platform: &str) -> Result<Vec<Source>, String> {
    let predicate = provenance.get("predicate").unwrap_or(provenance);
    let materials = predicate.get("materials")
        .or_else(|| predicate.pointer("/buildDefinition/resolvedDependencies"))
        .and_then(Value::as_array).ok_or("build provenance has no materials")?;
    let mut sources = Vec::new();
    for material in materials {
        let uri = material["uri"].as_str().ok_or("source material has no URI")?;
        let Some(image) = uri.strip_prefix("pkg:docker/") else { continue };
        let (image, query) = image.split_once('?').unwrap_or((image, ""));
        let (name, version) = image.rsplit_once('@').unwrap_or((image, ""));
        let mut reference = decode(name)?;
        if !version.is_empty() { reference.push(':'); reference.push_str(&decode(version)?); }
        let mut platform = default_platform.to_owned();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').ok_or("invalid source qualifier")?;
            if key == "digest" { reference.push('@'); reference.push_str(&decode(value)?); }
            if key == "platform" { platform = decode(value)?; }
        }
        let original = Reference::parse(&reference).map_err(|e| e.to_string())?;
        let resolved = format!("sha256:{}", material["digest"]["sha256"].as_str().ok_or("source has no resolved digest")?);
        asagiri_image::store::digest_hex(&resolved).map_err(|e| e.to_string())?;
        let mut pinned = original.clone();
        // Keep an explicitly requested index digest. Pulling just its platform
        // child loses the index identity required by inspect(name@index).
        if !pinned.is_digest() { pinned.reference = resolved.clone(); }
        let platform = if platform.is_empty() { Platform::host_linux() }
            else { Platform::parse(&platform).ok_or("invalid source platform")? };
        sources.push(Source { original, pinned, resolved, platform });
    }
    Ok(sources)
}

fn credentials(header: Option<&str>, registry: &str) -> Option<Credentials> {
    let values: Value = serde_json::from_slice(&asagiri_image::b64::decode(header?)?).ok()?;
    let values = values.as_object()?;
    for (server, value) in values {
        let host = server.strip_prefix("https://").or_else(|| server.strip_prefix("http://")).unwrap_or(server)
            .trim_end_matches('/');
        if host == registry || (registry == "docker.io" && matches!(host, "index.docker.io/v1" | "registry-1.docker.io")) {
            return Credentials::from_header(&asagiri_image::b64::encode(&serde_json::to_vec(value).ok()?));
        }
    }
    None
}

fn capture(engine: &Engine, id: &str, args: &[&str]) -> Result<String, String> {
    let (code, output) = engine.exec_capture(id, args).map_err(|e| format!("reading build sources: {e:?}"))?;
    if code != 0 { return Err(format!("reading build sources exited {code}")); }
    if output.len() > 16 * 1024 * 1024 { return Err("build source metadata is too large".into()); }
    Ok(output)
}

fn descriptors(history: &str, reference: &str) -> Result<Vec<(String, u64)>, String> {
    for line in history.lines().filter(|line| !line.trim().is_empty()) {
        let event: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        let record = &event["record"];
        if record["Ref"] != reference { continue; }
        if !record["error"].is_null() { return Err("source record belongs to a failed solve".into()); }
        let mut results = Vec::new();
        if record["Result"].is_object() { results.push(&record["Result"]); }
        if let Some(platforms) = record["Results"].as_object() { results.extend(platforms.values()); }
        let mut descriptors = Vec::new();
        for result in results {
            for item in result["Attestations"].as_array().into_iter().flatten() {
                if item["media_type"] == "application/vnd.in-toto+json"
                    && item["annotations"]["in-toto.io/predicate-type"].as_str().is_some_and(|s| s.starts_with("https://slsa.dev/provenance/")) {
                    let digest = item["digest"].as_str().ok_or("provenance descriptor has no digest")?;
                    asagiri_image::store::digest_hex(digest).map_err(|e| e.to_string())?;
                    let size = item["size"].as_u64().filter(|s| *s <= 16 * 1024 * 1024).ok_or("invalid provenance size")?;
                    descriptors.push((digest.to_owned(), size));
                }
            }
        }
        if !descriptors.is_empty() { return Ok(descriptors); }
    }
    Err("completed build has no source provenance".into())
}

// A zero-filesystem Dockerfile result has no ResultProxy, so BuildKit's
// history writer cannot attach provenance. The native Dockerfile frontend
// still emits a base config for registry parents, including zero-layer ones.
// Accept only the completed, no-base, empty-rootfs result, then verify its
// exported manifest against the image actually imported from this solve.
// Older histories omit Frontend; this caller itself fixes dockerfile.v0 in
// Options::argv and binds the history to that exact invocation's build.ref.
fn empty_result(history: &str, reference: &str) -> Result<(String, u64), String> {
    for line in history.lines().filter(|line| !line.trim().is_empty()) {
        let event: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        let record = &event["record"];
        if record["Ref"] != reference { continue; }
        if !record["error"].is_null() || !record["CompletedAt"].is_object()
            || record.get("Frontend").is_some_and(|frontend| frontend != "dockerfile.v0") || !record["Results"].is_null()
            || record["Result"].get("Attestations").is_some() {
            return Err("empty build result is not a completed Dockerfile solve".into());
        }
        let metadata = record["ExporterResponse"].as_object().ok_or("empty build has no metadata")?;
        if metadata.keys().any(|key| key.starts_with("containerimage.base.config")) {
            return Err("build without provenance has a base image".into());
        }
        let config: Value = serde_json::from_str(metadata.get("containerimage.config")
            .and_then(Value::as_str).ok_or("empty build has no config")?).map_err(|e| e.to_string())?;
        let diff_ids = config.pointer("/rootfs/diff_ids").ok_or("empty build has no rootfs")?;
        if config.pointer("/rootfs/type") != Some(&Value::String("layers".into()))
            || !(diff_ids.is_null() || diff_ids.as_array().is_some_and(Vec::is_empty)) {
            return Err("build without provenance has filesystem layers".into());
        }
        let descriptor = &record["Result"]["ResultDeprecated"];
        if !matches!(descriptor["media_type"].as_str(), Some("application/vnd.oci.image.manifest.v1+json" | "application/vnd.docker.distribution.manifest.v2+json")) {
            return Err("empty build has no image manifest descriptor".into());
        }
        let digest = descriptor["digest"].as_str().ok_or("empty manifest has no digest")?;
        asagiri_image::store::digest_hex(digest).map_err(|e| e.to_string())?;
        let size = descriptor["size"].as_u64().filter(|s| *s <= 16 * 1024 * 1024).ok_or("invalid empty manifest size")?;
        return Ok((digest.to_owned(), size));
    }
    Err("completed build has no source provenance".into())
}

fn read_blob(engine: &Engine, id: &str, digest: &str, size: u64) -> Result<Value, String> {
    let bytes = capture(engine, id, &["buildctl", "debug", "get", digest])?;
    if bytes.len() as u64 != size || asagiri_image::store::sha256_digest(bytes.as_bytes()) != digest {
        return Err("build source metadata does not match its descriptor".into());
    }
    serde_json::from_str(&bytes).map_err(|e| format!("invalid build source metadata: {e}"))
}

fn empty_manifest_matches(manifest: &Value, images: &[asagiri_image::Image]) -> bool {
    manifest["schemaVersion"] == 2 && manifest["layers"].as_array().is_some_and(Vec::is_empty)
        && manifest["config"]["digest"].as_str().is_some_and(|digest| images.iter()
            .any(|image| image.config_digest == digest && image.layers.is_empty()))
}

pub(super) fn register(engine: &Engine, id: &str, directory: &str, req: &Request, options: &Options, images: &[asagiri_image::Image]) -> Result<(), String> {
    let reference = capture(engine, id, &["cat", &format!("{directory}/build.ref")])?;
    let reference = reference.trim();
    if reference.is_empty() || !reference.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("invalid completed build reference".into());
    }
    // histories has no ref flag; filter inside its Go template so old builds'
    // metadata is not copied into the Engine. It emits blank lines for them.
    let format = format!("{{{{if eq .Record.Ref \"{reference}\"}}}}{{{{json .}}}}{{{{end}}}}");
    let history = capture(engine, id, &["buildctl", "debug", "histories", "--format", &format])?;
    let descriptors = match descriptors(&history, reference) {
        Ok(descriptors) => descriptors,
        Err(_) => {
            let (digest, size) = empty_result(&history, reference)?;
            let manifest = read_blob(engine, id, &digest, size)?;
            if !empty_manifest_matches(&manifest, images) {
                return Err("empty build result differs from the imported image".into());
            }
            return Ok(());
        }
    };
    for (digest, size) in descriptors {
        let provenance = read_blob(engine, id, &digest, size)?;
        for source in sources(&provenance, &options.platform)? {
            let existing = engine.image(&source.original.short()).map_err(|e| e.to_string())?;
            if existing.is_some_and(|image| image.platform == source.platform.to_string()
                && (image.digest == source.resolved || image.index_digest.as_deref() == Some(&source.resolved))) { continue; }
            // The same request credentials are scoped to this source's registry.
            // Existing verified blobs are reused by the normal image-store pull.
            let client = Client::new().with_credentials(credentials(req.header("X-Registry-Config"), &source.original.registry));
            let image = client.pull(&engine.store, &source.pinned, &source.platform, &|_| {})
                .map_err(|e| format!("registering classic build parent: {e}"))?;
            if image.digest != source.resolved && image.index_digest.as_deref() != Some(&source.resolved) {
                return Err("registered parent differs from the completed solve".into());
            }
            engine.store.put_image(asagiri_image::Image { name: source.original.short(), ..image.clone() }).map_err(|e| e.to_string())?;
            engine.emit_image("pull", &image.config_digest, &source.original.short());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn material(uri: &str) -> Value {
        json!({"uri": uri, "digest": {"sha256": "a".repeat(64)}})
    }

    #[test]
    fn tag_is_registered_but_only_resolved_digest_is_pulled() {
        let source = sources(&json!({"materials": [material("pkg:docker/registry.example/team/base@stable?platform=linux%2Farm64")]}), "linux/amd64").unwrap().remove(0);
        assert_eq!(source.original.short(), "registry.example/team/base:stable");
        assert_eq!(source.pinned.reference, format!("sha256:{}", "a".repeat(64)));
        assert_eq!(source.platform.to_string(), "linux/arm64");
    }

    #[test]
    fn requested_index_identity_is_preserved_with_resolved_child() {
        let uri = format!("pkg:docker/registry.example/base?digest=sha256%3A{}", "b".repeat(64));
        let source = sources(&json!({"predicate": {"materials": [material(&uri)]}}), "linux/amd64").unwrap().remove(0);
        assert_eq!(source.original.reference, format!("sha256:{}", "b".repeat(64)));
        assert_eq!(source.pinned.reference, source.original.reference);
        assert_ne!(source.resolved, source.pinned.reference);
    }

    #[test]
    fn v1_dependencies_and_non_registry_sources() {
        let proof = json!({"predicate": {"buildDefinition": {"resolvedDependencies": [
            material("pkg:docker/alpine@latest"),
            {"uri": "pkg:oci/local@latest"}, {"uri": "git+https://example.org/source"}
        ]}}});
        let found = sources(&proof, "linux/amd64").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].original.registry, "docker.io");
        assert!(sources(&json!({"materials": []}), "").unwrap().is_empty());
    }

    #[test]
    fn missing_or_invalid_source_digest_is_rejected() {
        assert!(sources(&json!({"materials": [{"uri": "pkg:docker/alpine@latest"}]}), "").is_err());
        assert!(sources(&json!({"materials": [{"uri": "pkg:docker/alpine@latest", "digest": {"sha256": "bad"}}]}), "").is_err());
        assert!(sources(&json!({"materials": [material("pkg:docker/alpine@%XY")]}), "").is_err());
        assert!(sources(&json!({}), "").is_err());
    }

    fn history(reference: &str) -> Value {
        json!({"record": {"Ref": reference, "Result": {"Attestations": [{
            "media_type": "application/vnd.in-toto+json", "digest": format!("sha256:{}", "c".repeat(64)),
            "size": 123, "annotations": {"in-toto.io/predicate-type": "https://slsa.dev/provenance/v0.2"}
        }]}}})
    }

    #[test]
    fn history_must_match_exact_successful_solve() {
        let foreign = history("foreign");
        assert!(descriptors(&foreign.to_string(), "selected").is_err());
        let selected = history("selected");
        let rows = format!("{foreign}\n{selected}\n");
        assert_eq!(descriptors(&rows, "selected").unwrap().len(), 1);
        let mut failed = selected;
        failed["record"]["error"] = json!({"code": 2, "message": "failed"});
        assert!(descriptors(&failed.to_string(), "selected").is_err());
    }

    #[test]
    fn platform_results_and_size_limits() {
        let mut event = history("selected");
        let result = event["record"].as_object_mut().unwrap().remove("Result").unwrap();
        event["record"]["Results"] = json!({"linux/amd64": result});
        assert_eq!(descriptors(&event.to_string(), "selected").unwrap().len(), 1);
        event["record"]["Results"]["linux/amd64"]["Attestations"][0]["size"] = json!(16 * 1024 * 1024 + 1);
        assert!(descriptors(&event.to_string(), "selected").is_err());
    }

    #[test]
    fn registry_credentials_never_fall_back_to_another_authority() {
        let header = asagiri_image::b64::encode(&serde_json::to_vec(&json!({
            "https://registry.example/": {"username": "alice", "password": "one"},
            "https://index.docker.io/v1/": {"username": "bob", "password": "two"}
        })).unwrap());
        assert_eq!(credentials(Some(&header), "registry.example").unwrap().username, "alice");
        assert_eq!(credentials(Some(&header), "docker.io").unwrap().username, "bob");
        assert!(credentials(Some(&header), "other.example").is_none());
        assert!(credentials(None, "registry.example").is_none());
    }
    fn scratch_history() -> Value {
        json!({"record": {"Ref": "scratch", "CompletedAt": {"seconds": 1},
            "ExporterResponse": {"containerimage.config": json!({"rootfs": {"type": "layers", "diff_ids": null}}).to_string()},
            "Result": {"ResultDeprecated": {"media_type": "application/vnd.oci.image.manifest.v1+json",
                "digest": format!("sha256:{}", "d".repeat(64)), "size": 200}}
        }})
    }

    #[test]
    fn empty_result_requires_completed_no_base_empty_rootfs() {
        let record = scratch_history();
        assert!(empty_result(&format!("\n{record}\n\n"), "scratch").is_ok());
        assert!(empty_result(&record.to_string(), "another").is_err());
        for (pointer, value) in [
            ("/record/CompletedAt", Value::Null),
            ("/record/error", json!({"code": 2})),
            ("/record/Frontend", json!("gateway.v0")),
            ("/record/ExporterResponse/containerimage.base.config", json!("{}")),
            ("/record/ExporterResponse/containerimage.config", json!("{}")),
            ("/record/Result/ResultDeprecated/digest", json!("invalid")),
        ] {
            let mut invalid = record.clone();
            if pointer.ends_with("containerimage.base.config") {
                invalid["record"]["ExporterResponse"]["containerimage.base.config"] = value;
            } else if pointer.ends_with("error") { invalid["record"]["error"] = value; }
            else if pointer.ends_with("Frontend") { invalid["record"]["Frontend"] = value; }
            else { *invalid.pointer_mut(pointer).unwrap() = value; }
            assert!(empty_result(&invalid.to_string(), "scratch").is_err(), "{pointer}");
        }
        let mut malformed = record.clone();
        malformed["record"]["Result"]["Attestations"] = json!([{"digest": "bad"}]);
        assert!(empty_result(&malformed.to_string(), "scratch").is_err());
        let mut layered = record;
        layered["record"]["ExporterResponse"]["containerimage.config"] = json!(json!({"rootfs": {"type": "layers", "diff_ids": ["sha256:layer"]}}).to_string());
        assert!(empty_result(&layered.to_string(), "scratch").is_err());
    }

    #[test]
    fn empty_result_manifest_is_bound_to_imported_config_and_layers() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let image = asagiri_image::Image {
            name: "empty:latest".into(), digest: digest.clone(), index_digest: None, local_tag: true,
            config_digest: digest.clone(), layers: Vec::new(), size: 0, platform: "linux/amd64".into(),
            created: String::new(), pulled_at: 0, config: asagiri_image::RunConfig::default(),
        };
        let manifest = json!({"schemaVersion": 2, "config": {"digest": digest}, "layers": []});
        assert!(empty_manifest_matches(&manifest, std::slice::from_ref(&image)));
        assert!(!empty_manifest_matches(&manifest, &[]));
        let mut changed = manifest.clone(); changed["config"]["digest"] = json!("sha256:other");
        assert!(!empty_manifest_matches(&changed, std::slice::from_ref(&image)));
        changed = manifest; changed["layers"] = json!([{"digest": "sha256:layer"}]);
        assert!(!empty_manifest_matches(&changed, &[image]));
    }

}
