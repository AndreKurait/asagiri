//! Checkpoint compatibility inventory and source handoff controls.
mod networks;
mod receiver;
pub(crate) use receiver::create_receiver;
pub(crate) use networks::prepare_networks;

use crate::client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::{fs::File, io};

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Platform {
    pub engine_version: String,
    pub engine_os: String,
    #[serde(flatten)]
    pub guest: GuestPlatform,
    pub live_capture: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GuestPlatform {
    pub guest_architecture: String,
    pub guest_kernel: String,
    pub cpu_features: Option<BTreeSet<String>>,
    pub system_mount_targets: Option<BTreeSet<String>>,
    pub criu_package_version: Option<String>,
}

impl GuestPlatform {
    fn missing_features(&self, destination: &Self) -> Option<Vec<String>> {
        if self.guest_architecture != destination.guest_architecture { return None; }
        let source = self.cpu_features.as_ref()?;
        let destination = destination.cpu_features.as_ref()?;
        Some(source.difference(destination).cloned().collect())
    }
}

pub(crate) fn create(args: &[String]) -> io::Result<()> {
    let (external_network, rest) = match args.split_first() {
        Some((flag, rest)) if flag == "--external-network" => (true, rest),
        _ => (false, args),
    };
    let [container, name] = rest else { return Err(io::Error::other("usage: asagiri checkpoint create [--external-network] CONTAINER NAME")); };
    if !client::valid_checkpoint_component(container) || !client::valid_checkpoint_component(name) {
        return Err(io::Error::other("invalid container or checkpoint name"));
    }
    let response = client::request("POST", &format!("/asagiri/checkpoints/{container}/{name}"), Some(&json!({"externalNetwork": external_network})))?;
    if response.ok() { Ok(()) } else { Err(io::Error::other(response.error())) }
}

pub(crate) fn source_group(args: &[String]) -> io::Result<()> {
    let Some((action, rest)) = args.split_first() else { return Err(io::Error::other("missing checkpoint action")); };
    let mut rest = rest;
    let mut external_network = false;
    let mut destination = None;
    while action == "capture-group" {
        match rest {
            [flag, tail @ ..] if flag == "--external-network" && !external_network => {
                external_network = true;
                rest = tail;
            }
            [flag, file, tail @ ..] if flag == "--destination" && destination.is_none() => {
                destination = Some(file);
                rest = tail;
            }
            [flag, ..] if flag.starts_with("--") => return Err(io::Error::other(format!("invalid or incomplete capture option {flag}"))),
            _ => break,
        }
    }
    let Some((name, containers)) = rest.split_first() else { return Err(io::Error::other("missing checkpoint name")); };
    if containers.is_empty() { return Err(io::Error::other("supply at least one source container")); }
    // Bind the capture to the same immutable IDs whose dependencies were inspected.
    let inspected = destination.map(|file| verify_destination(file, containers, true)).transpose()?;
    let body = json!({"name": name, "containers": inspected.as_deref().unwrap_or(containers), "externalNetwork": external_network});
    let path = format!("/asagiri/checkpoint-{action}");
    let response = if action == "capture-group" {
        client::checkpoint_group_request(&path, &body)?
    } else { client::request("POST", &path, Some(&body))? };
    if response.ok() { Ok(()) } else { Err(io::Error::other(response.error())) }
}

pub(crate) fn restore_volume(args: &[String]) -> io::Result<()> {
    let [id, name, source, destination] = args else { return Err(io::Error::other("usage: asagiri checkpoint restore-volume CONTAINER NAME SOURCE_VOLUME NEW_VOLUME")); };
    if !client::valid_checkpoint_component(id) || !client::valid_checkpoint_component(name) { return Err(io::Error::other("invalid container or checkpoint name")); }
    let response = client::request("POST", &format!("/asagiri/checkpoints/{id}/{name}/volume"), Some(&json!({"source":source,"destination":destination})))?;
    if response.ok() { Ok(()) } else { Err(io::Error::other(response.error())) }
}

pub(crate) fn restore(args: &[String]) -> io::Result<()> {
    let [action, id, name] = args else { return Err(io::Error::other("usage: asagiri checkpoint restore-rootfs|restore-network-files|restore-stdin|verify-receiver-mounts|install-process CONTAINER NAME")); };
    let resource = match action.as_str() {
        "restore-rootfs" => "rootfs",
        "restore-network-files" => "network-files",
        "restore-stdin" => "stdin",
        "verify-receiver-mounts" => "verify-mounts",
        "install-process" => "install-process",
        _ => return Err(io::Error::other("unknown checkpoint restore operation")),
    };
    if !client::valid_checkpoint_component(id) || !client::valid_checkpoint_component(name) {
        return Err(io::Error::other("invalid container or checkpoint name"));
    }
    let response = client::request("POST", &format!("/asagiri/checkpoints/{id}/{name}/{resource}"), None)?;
    if response.ok() { Ok(()) } else { Err(io::Error::other(response.error())) }
}

pub(crate) fn fence_receiver(args: &[String]) -> io::Result<()> {
    let [action, id] = args else { return Err(io::Error::other("usage: asagiri checkpoint fence-receiver|unfence-receiver CONTAINER")); };
    if !client::valid_checkpoint_component(id) {
        return Err(io::Error::other("invalid receiver name"));
    }
    let operation = if action == "fence-receiver" { "fence" } else { "unfence" };
    let response = client::request("POST", &format!("/asagiri/checkpoint-receivers/{id}/{operation}"), None)?;
    if response.ok() { Ok(()) } else { Err(io::Error::other(response.error())) }
}

fn local_platform() -> io::Result<Platform> {
    serde_json::from_value(client::get("/asagiri/checkpoint-platform")?).map_err(io::Error::other)
}

pub(crate) fn print_platform() -> io::Result<()> {
    println!("{}", serde_json::to_string_pretty(&local_platform()?)?);
    Ok(())
}

pub(crate) fn preflight(args: &[String]) -> io::Result<()> {
    let [option, file, containers @ ..] = args else { return Err(usage()); };
    if option != "--destination" || containers.is_empty() { return Err(usage()); }
    verify_destination(file, containers, false).map(|_| ())
}

fn verify_destination(file: &str, containers: &[String], require_runtime: bool) -> io::Result<Vec<String>> {
    let destination: Platform = serde_json::from_reader(File::open(file)?)?;
    if destination.guest.guest_architecture.is_empty() || destination.guest.guest_kernel.is_empty()
        || destination.engine_os.is_empty() || destination.engine_version.is_empty()
    {
        return Err(io::Error::other("incomplete destination platform record"));
    }
    let source = local_platform()?;
    let mut ids = Vec::with_capacity(containers.len());
    let mut inventory = Vec::with_capacity(containers.len());
    let mut missing_mounts = BTreeSet::new();
    for container in containers {
        if !client::valid_checkpoint_component(container) {
            return Err(io::Error::other(format!("invalid container name: {container}")));
        }
        let entry = client::get(&format!("/containers/{container}/json"))?;
        let id = entry["Id"].as_str().filter(|s| !s.is_empty())
            .ok_or_else(|| io::Error::other(format!("engine returned an incomplete inspection for {container}")))?;
        ids.push(id.to_owned());
        let mounted: Option<BTreeSet<String>> = if source.guest.system_mount_targets.is_some() && entry["State"]["Running"] == true {
            Some(serde_json::from_value(client::get(&format!("/asagiri/checkpoint-mounts/{id}"))?)?)
        } else { None };
        let missing = mounted.as_ref().zip(destination.guest.system_mount_targets.as_ref()).map(|(paths, targets)| {
            paths.difference(targets).cloned().collect::<Vec<_>>()
        });
        if let Some(paths) = &missing { missing_mounts.extend(paths.iter().cloned()); }
        let mut row = dependencies(&entry);
        row["protectedSystemMounts"] = json!(mounted);
        row["protectedSystemMountsMissingOnDestination"] = json!(missing);
        inventory.push(row);
    }
    let matches = source.guest.guest_architecture == destination.guest.guest_architecture;
    let report = json!({
        "source": source, "destination": destination,
        "guestArchitectureMatches": matches, "containers": inventory,
        "sourceCpuFeaturesMissingOnDestination": source.guest.missing_features(&destination.guest),
        "criuPackageVersionsMatch": source.guest.criu_package_version.as_ref().zip(destination.guest.criu_package_version.as_ref()).map(|(a, b)| a == b),
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    if !matches {
        return Err(io::Error::other(format!("incompatible guest architectures: source {}, destination {}", source.guest.guest_architecture, destination.guest.guest_architecture)));
    }
    if !missing_mounts.is_empty() {
        return Err(io::Error::other(format!("destination lacks required system mount targets: {}", missing_mounts.into_iter().collect::<Vec<_>>().join(", "))));
    }
    if require_runtime && !destination.live_capture {
        return Err(io::Error::other("destination checkpoint runtime is unavailable"));
    }
    Ok(ids)
}

fn usage() -> io::Error {
    io::Error::other("usage: asagiri checkpoint preflight --destination PLATFORM.json CONTAINER [CONTAINER...]")
}

fn dependencies(inspect: &Value) -> Value {
    let host = &inspect["HostConfig"];
    let network = &inspect["NetworkSettings"];
    json!({
        "id": inspect["Id"], "name": inspect["Name"], "image": inspect["Image"],
        "status": inspect["State"]["Status"], "paused": inspect["State"]["Paused"],
        "mounts": inspect["Mounts"], "tmpfs": host["Tmpfs"], "volumesFrom": host["VolumesFrom"],
        "devices": host["Devices"], "pidMode": host["PidMode"], "networkMode": host["NetworkMode"],
        "networks": network["Networks"], "ports": network["Ports"], "requestedPorts": host["PortBindings"],
        "links": host["Links"], "dns": host["Dns"], "dnsSearch": host["DnsSearch"], "dnsOptions": host["DnsOptions"], "extraHosts": host["ExtraHosts"],
        "restartPolicy": host["RestartPolicy"],
    })
}
