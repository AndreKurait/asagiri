//! The two BuildKit protobuf messages the Docker driver needs adapted.
//! Field numbers follow moby/buildkit api/services/control/control.proto
//! and api/types/worker.proto (f55949dea4f03bb9d85b73e9afc9a609291289db).
//! Keep unknown fields byte for byte, including embedded LLB definitions:
//! this is not a second implementation of BuildKit's protocol.

use std::collections::BTreeMap;
use std::io;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[derive(Clone, Copy)]
struct Field<'a> {
    number: u64,
    wire: u8,
    data: &'a [u8],
    raw: &'a [u8],
}

fn varint(input: &[u8], at: &mut usize) -> io::Result<u64> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let byte = *input.get(*at).ok_or_else(|| invalid("truncated protobuf integer"))?;
        *at += 1;
        if shift == 63 && byte > 1 {
            return Err(invalid("protobuf integer overflow"));
        }
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Ok(value);
        }
    }
    Err(invalid("protobuf integer overflow"))
}

fn integer(output: &mut Vec<u8>, mut value: u64) {
    while value >= 128 {
        output.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    output.push(value as u8);
}

fn fields(input: &[u8]) -> io::Result<Vec<Field<'_>>> {
    let mut fields = Vec::new();
    let mut at = 0;
    while at < input.len() {
        // Even a tiny repeated field needs a descriptor here. Bound both
        // encoded bytes (in the caller) and their expanded representation.
        if fields.len() >= 65536 {
            return Err(invalid("too many protobuf fields"));
        }
        let start = at;
        let tag = varint(input, &mut at)?;
        let number = tag >> 3;
        if number == 0 || number > (1 << 29) - 1 {
            return Err(invalid("invalid protobuf field number"));
        }
        let wire = (tag & 7) as u8;
        let (from, end) = match wire {
            0 => {
                let from = at;
                varint(input, &mut at)?;
                (from, Some(at))
            }
            1 => (at, at.checked_add(8)),
            2 => {
                let length = usize::try_from(varint(input, &mut at)?)
                    .map_err(|_| invalid("protobuf length overflow"))?;
                (at, at.checked_add(length))
            }
            5 => (at, at.checked_add(4)),
            _ => return Err(invalid("unsupported protobuf wire type")),
        };
        let end = end.ok_or_else(|| invalid("protobuf length overflow"))?;
        if end > input.len() {
            return Err(invalid("truncated protobuf field"));
        }
        at = end;
        fields.push(Field { number, wire, data: &input[from..end], raw: &input[start..end] });
    }
    Ok(fields)
}

fn bytes(output: &mut Vec<u8>, number: u64, data: &[u8]) {
    integer(output, (number << 3) | 2);
    integer(output, data.len() as u64);
    output.extend_from_slice(data);
}

fn text<'a>(fields: &[Field<'a>], number: u64) -> io::Result<&'a str> {
    fields.iter().rev().find(|field| field.number == number && field.wire == 2)
        .map_or(Ok(""), |field| std::str::from_utf8(field.data).map_err(|_| invalid("invalid protobuf UTF-8")))
}

fn map(fields: &[Field<'_>], number: u64) -> io::Result<BTreeMap<String, String>> {
    let mut entries = BTreeMap::new();
    for field in fields.iter().filter(|field| field.number == number && field.wire == 2) {
        let entry = self::fields(field.data)?;
        entries.insert(text(&entry, 1)?.into(), text(&entry, 2)?.into());
    }
    Ok(entries)
}

fn map_entry(output: &mut Vec<u8>, number: u64, key: &str, value: &str) {
    let mut entry = Vec::new();
    bytes(&mut entry, 1, key.as_bytes());
    bytes(&mut entry, 2, value.as_bytes());
    bytes(output, number, &entry);
}

pub(super) struct Solve {
    pub data: Vec<u8>,
    pub session: String,
    pub load_exporters: Vec<usize>,
}

/// Only Moby's local image-store exporter changes. Explicit client file
/// outputs retain their exporter index and go back to the original client.
pub(super) fn solve(input: &[u8]) -> io::Result<Solve> {
    let fields = fields(input)?;
    let session = text(&fields, 5)?.to_owned();
    let modern = fields.iter().any(|field| field.number == 13 && field.wire == 2);
    let mut result = Solve { data: Vec::with_capacity(input.len()), session, load_exporters: Vec::new() };
    let mut index = 0;
    for field in &fields {
        if field.number == 13 && field.wire == 2 {
            let (data, load) = exporter(field.data)?;
            bytes(&mut result.data, 13, &data);
            if load { result.load_exporters.push(index); }
            index += 1;
        } else if !modern && matches!(field.number, 3 | 4) && field.wire == 2 {
            // Below: turn the legacy single exporter into the modern list.
        } else {
            result.data.extend_from_slice(field.raw);
        }
    }
    if !modern && !text(&fields, 3)?.is_empty() {
        let mut legacy = Vec::new();
        bytes(&mut legacy, 1, text(&fields, 3)?.as_bytes());
        for (key, value) in map(&fields, 4)? {
            map_entry(&mut legacy, 2, &key, &value);
        }
        let (data, load) = exporter(&legacy)?;
        bytes(&mut result.data, 13, &data);
        if load { result.load_exporters.push(0); }
    }
    if !result.load_exporters.is_empty() && (result.session.is_empty() || result.session.len() > 256) {
        return Err(invalid("local image exports require a BuildKit session"));
    }
    Ok(result)
}

fn exporter(input: &[u8]) -> io::Result<(Vec<u8>, bool)> {
    let fields = fields(input)?;
    if text(&fields, 1)? != "moby" {
        return Ok((input.to_vec(), false));
    }
    let mut attrs = map(&fields, 2)?;
    let no_store = attrs.get("store").is_some_and(|value| matches!(value.as_str(), "false" | "0" | "False" | "FALSE"));
    let load = !no_store;
    let mut output = Vec::with_capacity(input.len() + 32);
    bytes(&mut output, 1, if load { b"docker" } else { b"image" });
    if load {
        // With the classic Docker image store, Buildx pushes the loaded
        // image through Docker's API after Solve. Do not push a second time
        // in stock BuildKit or leave that client without a local image.
        attrs.remove("push");
        attrs.remove("push-by-digest");
        attrs.remove("store");
    } else {
        // Stock BuildKit owns its content cache, not Asagiri's image store.
        attrs.insert("store".into(), "false".into());
    }
    for (key, value) in attrs {
        map_entry(&mut output, 2, &key, &value);
    }
    for field in fields.iter().filter(|field| !matches!(field.number, 1 | 2)) {
        output.extend_from_slice(field.raw);
    }
    Ok((output, load))
}

pub(super) fn workers(input: &[u8], gateway: &str) -> io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len() + 128);
    for field in fields(input)? {
        if field.number != 1 || field.wire != 2 {
            output.extend_from_slice(field.raw);
            continue;
        }
        let mut worker = Vec::with_capacity(field.data.len());
        for property in fields(field.data)? {
            // Buildx interprets this label on the Docker driver as proof
            // that Docker has a containerd image store. Stock BuildKit's
            // OCI worker also sets it, but its snapshotter is not Asagiri's
            // image store: our imported images use their config digest.
            if property.number == 2 && property.wire == 2
                && text(&fields(property.data)?, 1)? == "org.mobyproject.buildkit.worker.snapshotter" {
                continue;
            }
            worker.extend_from_slice(property.raw);
        }
        // Last entry wins under protobuf map semantics. This is the
        // reachable host gateway, not the BuildKit container's localhost.
        map_entry(&mut worker, 2, "org.mobyproject.buildkit.worker.moby.host-gateway-ip", gateway);
        bytes(&mut output, 1, &worker);
    }
    Ok(output)
}

pub(super) fn payload(input: &[u8]) -> io::Result<&[u8]> {
    let fields = fields(input)?;
    Ok(fields.iter().rev().find(|field| field.number == 1 && field.wire == 2)
        .map_or(&[][..], |field| field.data))
}

pub(super) fn message(data: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len() + 6);
    bytes(&mut output, 1, data);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(kind: &str, attrs: &[(&str, &str)]) -> Vec<u8> {
        let mut output = Vec::new();
        bytes(&mut output, 1, kind.as_bytes());
        for (key, value) in attrs { map_entry(&mut output, 2, key, value); }
        output
    }

    #[test]
    fn local_load_does_not_capture_explicit_outputs_or_discard_unknown_fields() {
        let explicit = ex("docker", &[("name", "archive:test")]);
        let mut input = Vec::new();
        bytes(&mut input, 5, b"session-id");
        bytes(&mut input, 13, &explicit);
        bytes(&mut input, 13, &ex("moby", &[("name", "one:test,two:test")]));
        let mut unknown = Vec::new();
        bytes(&mut unknown, 100, b"future-option");
        input.extend_from_slice(&unknown);
        let result = solve(&input).unwrap();
        assert_eq!(result.session, "session-id");
        assert_eq!(result.load_exporters, vec![1]);
        let output = fields(&result.data).unwrap();
        let exports: Vec<_> = output.iter().filter(|field| field.number == 13).collect();
        assert_eq!(exports[0].data, explicit);
        let local = fields(exports[1].data).unwrap();
        assert_eq!(text(&local, 1).unwrap(), "docker");
        assert_eq!(map(&local, 2).unwrap()["name"], "one:test,two:test");
        assert!(result.data.ends_with(&unknown));
    }

    #[test]
    fn docker_push_loads_for_the_client_and_explicit_no_store_is_preserved() {
        let (output, load) = exporter(&ex("moby", &[("push", "true")])).unwrap();
        assert!(load);
        assert_eq!(text(&fields(&output).unwrap(), 1).unwrap(), "docker");
        assert!(!map(&fields(&output).unwrap(), 2).unwrap().contains_key("push"));
        let (output, load) = exporter(&ex("moby", &[("store", "false")])).unwrap();
        assert!(!load);
        assert_eq!(text(&fields(&output).unwrap(), 1).unwrap(), "image");
        assert_eq!(map(&fields(&output).unwrap(), 2).unwrap()["store"], "false");
    }

    #[test]
    fn old_solve_and_new_solve_agree_and_modern_exporters_win() {
        let mut old = Vec::new();
        bytes(&mut old, 3, b"moby");
        map_entry(&mut old, 4, "name", "legacy:test");
        bytes(&mut old, 5, b"session-id");
        assert_eq!(solve(&old).unwrap().load_exporters, vec![0]);
        bytes(&mut old, 13, &ex("local", &[]));
        assert!(solve(&old).unwrap().load_exporters.is_empty());
    }

    #[test]
    fn truncated_overflowing_and_excessive_fields_are_rejected() {
        for input in [&[0u8][..], &[10, 5, 1], &[10, 255, 255, 255, 255, 255, 255, 255, 255, 255, 2],
            &[9, 1], &[15], &[8, 128]] {
            assert!(fields(input).is_err(), "{input:?}");
        }
        assert!(fields(&[8, 0].repeat(65537)).is_err());
        let mut missing_session = Vec::new();
        bytes(&mut missing_session, 13, &ex("moby", &[]));
        assert!(solve(&missing_session).is_err());
    }

    #[test]
    fn worker_gateway_preserves_real_backend_capabilities() {
        let mut worker = Vec::new();
        bytes(&mut worker, 1, b"worker-id");
        map_entry(&mut worker, 2, "org.mobyproject.buildkit.worker.executor", "oci");
        map_entry(&mut worker, 2, "org.mobyproject.buildkit.worker.snapshotter", "overlayfs");
        let mut response = Vec::new();
        bytes(&mut response, 1, &worker);
        let output = workers(&response, "192.168.64.1").unwrap();
        let record = fields(&output).unwrap()[0].data;
        let labels = map(&fields(record).unwrap(), 2).unwrap();
        assert_eq!(labels["org.mobyproject.buildkit.worker.executor"], "oci");
        assert_eq!(labels["org.mobyproject.buildkit.worker.moby.host-gateway-ip"], "192.168.64.1");
        assert!(!labels.contains_key("org.mobyproject.buildkit.worker.snapshotter"));
    }
}
