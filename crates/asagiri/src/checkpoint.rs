//! Read-only inventory before pausing workloads for a handoff.
use crate::client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs::File, io};

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Platform {
    pub engine_version: String,
    pub engine_os: String,
    pub guest_architecture: String,
    pub guest_kernel: String,
    pub live_capture: bool,
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
    if destination.guest_architecture.is_empty() || destination.guest_kernel.is_empty()
        || destination.engine_os.is_empty() || destination.engine_version.is_empty()
    {
        return Err(io::Error::other("incomplete destination platform record"));
    }
    let source = local_platform()?;
    let mut inventory = Vec::with_capacity(containers.len());
    for container in containers {
        if !client::valid_checkpoint_component(container) {
            return Err(io::Error::other(format!("invalid container name: {container}")));
        }
        let entry = client::get(&format!("/containers/{container}/json"))?;
        if entry["Id"].as_str().is_none_or(str::is_empty) {
            return Err(io::Error::other(format!("engine returned an incomplete inspection for {container}")));
        }
        inventory.push(dependencies(&entry));
    }
    let matches = source.guest_architecture == destination.guest_architecture;
    let report = json!({
        "source": source, "destination": destination,
        "guestArchitectureMatches": matches, "containers": inventory,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    if !matches {
        return Err(io::Error::other(format!("incompatible guest architectures: source {}, destination {}", source.guest_architecture, destination.guest_architecture)));
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
