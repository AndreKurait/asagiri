//! Recreate the source endpoint settings on a stopped destination container.
use super::networks::{Inventory, prepare_definitions, read_inventory};
use crate::client;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{fs::File, io};

fn invalid(message: impl Into<String>) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message.into()) }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NetworkConfig {
    container_name: String,
    open_stdin: Option<bool>,
    stdin_once: Option<bool>,
    mode: String,
    hostname: String,
    domainname: String,
    dns: Vec<String>,
    dns_search: Vec<String>,
    dns_options: Vec<String>,
    extra_hosts: Vec<String>,
    links: Vec<Link>,
    exposed_ports: Map<String, Value>,
    ports: Map<String, Value>,
    requested_ports: Map<String, Value>,
}

#[derive(Deserialize)]
struct Link { name: String, alias: String }

/// Prefer an explicit input error over silently overriding the operator's network settings.
fn set_field(map: &mut Map<String, Value>, key: &str, value: Value) -> io::Result<()> {
    if map.get(key).is_some_and(|existing| *existing != value) {
        return Err(invalid(format!("{key} in the create request conflicts with the checkpoint")));
    }
    map.insert(key.into(), value);
    Ok(())
}

fn receiver(config: Value, inventory: &Inventory) -> io::Result<(String, Value)> {
    let saved: NetworkConfig = serde_json::from_value(Value::Object(inventory.settings.clone()))?;
    if !client::valid_checkpoint_component(&saved.container_name) {
        return Err(invalid("invalid captured container name"));
    }
    let Value::Object(mut body) = config else { return Err(invalid("expected a Docker container-create JSON object")); };
    if body.get("Image").and_then(Value::as_str).is_none_or(str::is_empty) { return Err(invalid("supply Image in the destination create request")); }
    if let Some(streams) = &inventory.stdio {
        set_field(&mut body, "Tty", json!(streams.tty))?;
        if !streams.tty {
            set_field(&mut body, "OpenStdin", json!(streams.stdin.is_some()))?;
        } else if let Some(open) = saved.open_stdin {
            set_field(&mut body, "OpenStdin", json!(open))?;
        }
        if let Some(once) = saved.stdin_once { set_field(&mut body, "StdinOnce", json!(once))?; }
    }
    let mut endpoints = Map::new();
    let mut primary = None;
    for net in &inventory.networks {
        let name = net["Name"].as_str().ok_or_else(|| invalid("missing captured network name"))?;
        let id = net["Id"].as_str().ok_or_else(|| invalid("missing captured network ID"))?;
        if saved.mode == name || saved.mode == id { primary = Some(name); }
        if saved.mode == "none" && name == "none" && net["Driver"] == "null" && inventory.networks.len() == 1 {
            continue;
        }
        if net["Driver"] != "bridge" { return Err(invalid("receiver endpoint recreation requires bridge or none networking")); }
        let source = inventory.endpoints.get(name).or_else(|| inventory.endpoints.get(id)).ok_or_else(|| invalid("missing captured endpoint"))?;
        let mut addresses = Map::new();
        for (enabled, observed, request) in [("EnableIPv4", "IPAddress", "IPv4Address"), ("EnableIPv6", "GlobalIPv6Address", "IPv6Address")] {
            if net[enabled] == true {
                let ip = source[observed].as_str().filter(|s| !s.is_empty()).ok_or_else(|| invalid(format!("missing {observed} on {name}")))?;
                addresses.insert(request.into(), json!(ip));
            }
        }
        let requested_aliases: Option<Vec<String>> = serde_json::from_value(source["Aliases"].clone())?;
        let mut aliases = requested_aliases.unwrap_or_default();
        // User bridges resolve the source short ID. Keep that name when the destination gets a new ID.
        let dns_names: Option<Vec<String>> = serde_json::from_value(source["DNSNames"].clone())?;
        for dns_name in dns_names.into_iter().flatten() {
            if dns_name != saved.container_name && dns_name != saved.hostname && !aliases.contains(&dns_name) { aliases.push(dns_name); }
        }
        endpoints.insert(name.into(), json!({"IPAMConfig": addresses, "MacAddress": source["MacAddress"], "GwPriority": source["GwPriority"], "Aliases": aliases}));
    }
    let primary = primary.ok_or_else(|| invalid("checkpoint lacks its primary bridge or none network"))?;
    let mut bindings = Map::new();
    for (port, published) in saved.ports {
        if published.is_null() { continue; }
        let rows = published.as_array().filter(|rows| !rows.is_empty()).ok_or_else(|| invalid(format!("missing bindings for {port}")))?;
        for row in rows {
            if row["HostIp"].as_str().and_then(|s| s.parse::<std::net::IpAddr>().ok()).is_none()
                || !row["HostPort"].as_str().and_then(|s| s.parse::<u16>().ok()).is_some_and(|port| port > 0) {
                return Err(invalid(format!("missing allocated host address or port for {port}")));
            }
        }
        bindings.insert(port, published);
    }
    for port in saved.requested_ports.keys() {
        if !bindings.contains_key(port) { return Err(invalid(format!("missing published port for {port}"))); }
    }
    let host = body.entry("HostConfig").or_insert_with(|| json!({})).as_object_mut().ok_or_else(|| invalid("expected a HostConfig object"))?;
    set_field(host, "NetworkMode", json!(primary))?;
    set_field(host, "PortBindings", Value::Object(bindings))?;
    set_field(host, "PublishAllPorts", json!(false))?;
    set_field(host, "Dns", json!(saved.dns))?;
    set_field(host, "DnsSearch", json!(saved.dns_search))?;
    set_field(host, "DnsOptions", json!(saved.dns_options))?;
    set_field(host, "ExtraHosts", json!(saved.extra_hosts))?;
    let links: Vec<_> = saved.links.iter().map(|link| format!("{}:{}", link.name, link.alias)).collect();
    set_field(host, "Links", json!(links))?;
    set_field(&mut body, "Hostname", json!(saved.hostname))?;
    set_field(&mut body, "Domainname", json!(saved.domainname))?;
    set_field(&mut body, "ExposedPorts", Value::Object(saved.exposed_ports))?;
    set_field(&mut body, "NetworkingConfig", json!({"EndpointsConfig": endpoints}))?;
    Ok((saved.container_name, Value::Object(body)))
}

pub(crate) fn create_receiver(args: &[String]) -> io::Result<()> {
    let (wait, args) = match args {
        [flag, rest @ ..] if flag == "--wait-for-restore" => (true, rest),
        _ => (false, args),
    };
    let [archive, request] = args else { return Err(invalid("usage: asagiri checkpoint create-receiver [--wait-for-restore] INPUT.tar CREATE.json")); };
    let inventory = read_inventory(archive)?;
    if wait && inventory.settings.get("externalNetwork").and_then(Value::as_str) != Some(asagiri_proto::CHECKPOINT_NETNS) {
        return Err(invalid("a waiting checkpoint receiver requires an archive captured with --external-network on an engine-managed bridge"));
    }
    let (name, body) = receiver(serde_json::from_reader(io::BufReader::new(File::open(request)?))?, &inventory)?;
    prepare_definitions(inventory.networks)?;
    let path = format!("/asagiri/checkpoint-receivers?name={name}&wait-for-restore={wait}");
    let response = client::request("POST", &path, Some(&body))?;
    if !response.ok() { return Err(io::Error::other(format!("could not create receiver {name}: {}", response.error()))); }
    let result = response.json();
    let id = result["Id"].as_str().ok_or_else(|| invalid("destination engine returned no container ID"))?;
    if let Some(warnings) = result["Warnings"].as_array() {
        for warning in warnings.iter().filter_map(Value::as_str) { eprintln!("{warning}"); }
    }
    println!("{id}");
    Ok(())
}
