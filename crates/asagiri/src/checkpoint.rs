//! Checkpoint compatibility inventory and source handoff controls.
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

pub(crate) fn fence(args: &[String]) -> io::Result<()> {
    let [action, name, containers @ ..] = args else { return Err(io::Error::other("usage: asagiri checkpoint fence|release NAME CONTAINER [CONTAINER...]")); };
    if containers.is_empty() { return Err(io::Error::other("supply at least one source container")); }
    let response = client::request("POST", &format!("/asagiri/checkpoint-{action}"), Some(&json!({"name": name, "containers": containers})))?;
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
    let destination: Platform = serde_json::from_reader(File::open(file)?)?;
    if destination.guest.guest_architecture.is_empty() || destination.guest.guest_kernel.is_empty()
        || destination.engine_os.is_empty() || destination.engine_version.is_empty()
    {
        return Err(io::Error::other("incomplete destination platform record"));
    }
    let source = local_platform()?;
    let mut inventory = Vec::with_capacity(containers.len());
    let mut missing_mounts = BTreeSet::new();
    for container in containers {
        if !client::valid_checkpoint_component(container) {
            return Err(io::Error::other(format!("invalid container name: {container}")));
        }
        let entry = client::get(&format!("/containers/{container}/json"))?;
        let id = entry["Id"].as_str().filter(|s| !s.is_empty())
            .ok_or_else(|| io::Error::other(format!("engine returned an incomplete inspection for {container}")))?;
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
    Ok(())
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
