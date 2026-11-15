//! Bounded HTTP replies for health probes over Unix sockets and Windows pipes.

use std::io::{self, BufRead, BufReader, Read};

const MAX_RESPONSE: u64 = 64 * 1024;

pub(super) fn read(connection: impl Read) -> io::Result<super::Response> {
    let mut reader = BufReader::new(connection.take(MAX_RESPONSE));
    let (status, length, chunked) = super::head(&mut reader)?;
    let mut body = Vec::new();
    let too_large = || io::Error::new(io::ErrorKind::InvalidData, "engine health response is too large");
    if status != 204 && status != 304 {
        if chunked {
            loop {
                let mut line = String::new();
                reader.read_line(&mut line)?;
                let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid health response chunk"))?;
                if size == 0 { break; }
                if size > MAX_RESPONSE as usize - body.len() { return Err(too_large()); }
                let start = body.len();
                body.resize(start + size, 0);
                reader.read_exact(&mut body[start..])?;
                let mut end = [0; 2];
                reader.read_exact(&mut end)?;
                if end != *b"\r\n" {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid health response chunk ending"));
                }
            }
        } else if let Some(length) = length {
            if length > MAX_RESPONSE as usize { return Err(too_large()); }
            body.resize(length, 0);
            reader.read_exact(&mut body)?;
        } else {
            reader.read_to_end(&mut body)?;
        }
    }
    if reader.get_ref().limit() == 0 { return Err(too_large()); }
    Ok(super::Response { status, body })
}
