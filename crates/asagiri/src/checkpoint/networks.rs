use crate::client;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

#[derive(Deserialize)]
struct Inventory {
    version: u64,
    networks: Vec<Value>,
    endpoints: Map<String, Value>,
}

fn inventory(mut file: impl Read + Seek) -> io::Result<Inventory> {
    let size = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(0))?;
    let mut archive = tar::Archive::new(&mut file);
    let mut end = 0;
    let mut result = None;
    let mut long_name = None;
    for entry in archive.entries_with_seek()?.raw(true) {
        let mut entry = entry?;
        end = entry.size().checked_next_multiple_of(512).and_then(|n| entry.raw_file_position().checked_add(n))
            .filter(|&n| n <= size).ok_or_else(|| io::Error::other("truncated checkpoint member"))?;
        let kind = entry.header().entry_type();
        if kind.is_gnu_longname() {
            // The Linux checkpoint importer accepts extended paths up to its PATH_MAX.
            if long_name.is_some() || entry.size() > 4096 { return Err(io::Error::other("invalid extended checkpoint path")); }
            let mut name = Vec::new();
            entry.read_to_end(&mut name)?;
            if name.pop() != Some(0) || name.contains(&0) { return Err(io::Error::other("invalid extended checkpoint path")); }
            long_name = Some(name);
            continue;
        }
        if !kind.is_file() && !kind.is_dir() { return Err(io::Error::other("unexpected checkpoint archive member type")); }
        let is_inventory = long_name.take().map_or_else(|| entry.path_bytes().as_ref() == b"network.json", |name| name == b"network.json");
        if is_inventory {
            if result.is_some() || !kind.is_file() || entry.size() > asagiri_proto::MAX_FRAME as u64 { return Err(io::Error::other("invalid checkpoint network inventory")); }
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            result = Some(serde_json::from_slice(&bytes)?);
        }
    }
    if long_name.is_some() { return Err(io::Error::other("missing checkpoint member after extended path")); }
    if size - end < 1024 || !(size - end).is_multiple_of(512) { return Err(io::Error::other("missing checkpoint archive end blocks")); }
    file.seek(SeekFrom::Start(end))?;
    let mut buf = [0; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 { break; }
        if buf[..n].iter().any(|&byte| byte != 0) { return Err(io::Error::other("data after checkpoint archive end")); }
    }
    result.ok_or_else(|| io::Error::other("checkpoint lacks network.json"))
}

pub(crate) fn prepare_networks(archives: &[String]) -> io::Result<()> {
    if archives.is_empty() { return Err(io::Error::other("usage: asagiri checkpoint prepare-networks INPUT.tar [INPUT.tar...]")); }
    let mut definitions = Vec::new();
    for path in archives {
        let record = File::open(path).and_then(inventory).map_err(|e| io::Error::other(format!("{path}: {e}")))?;
        if record.version != 1 { return Err(io::Error::other(format!("{path}: unsupported network inventory version {}", record.version))); }
        for (name, ep) in &record.endpoints {
            if !ep["NetworkID"].is_string() || !record.networks.iter().any(|net| net["Id"] == ep["NetworkID"] && (net["Name"] == name.as_str() || net["Id"] == name.as_str())) {
                return Err(io::Error::other(format!("{path}: missing network definition for {name}")));
            }
        }
        for net in record.networks {
            if !record.endpoints.values().any(|ep| ep["NetworkID"].is_string() && ep["NetworkID"] == net["Id"]) {
                return Err(io::Error::other(format!("{path}: network definition without an endpoint")));
            }
            if !definitions.contains(&net) { definitions.push(net); }
        }
    }
    let response = client::request("POST", "/asagiri/checkpoint-networks", Some(&Value::Array(definitions)))?;
    if !response.ok() { return Err(io::Error::other(response.error())); }
    println!("{}", serde_json::to_string_pretty(&response.json())?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn archive(names: &[&str]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        let record = br#"{"version":1,"networks":[],"endpoints":{}}"#;
        for name in names {
            let mut h = tar::Header::new_gnu();
            h.set_size(record.len() as u64);
            tar.append_data(&mut h, name, record.as_slice()).unwrap();
        }
        tar.into_inner().unwrap()
    }

    #[test]
    fn only_the_single_top_level_network_inventory_is_accepted() {
        let valid = archive(&[&"a".repeat(160), "network.json"]);
        assert_eq!(inventory(Cursor::new(&valid)).unwrap().version, 1);
        assert!(inventory(Cursor::new(&valid[..valid.len() - 512])).is_err());
        assert!(inventory(Cursor::new(archive(&["volumes/v/network.json"]))).is_err());
        assert!(inventory(Cursor::new(archive(&["network.json", "network.json"]))).is_err());
        let mut trailing = valid;
        trailing.extend_from_slice(&[1; 512]);
        assert!(inventory(Cursor::new(trailing)).is_err());
    }
}
