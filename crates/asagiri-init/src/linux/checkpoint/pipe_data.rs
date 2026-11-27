//! CRIU pipes-data.img records contain a framed PipeDataEntry followed by its raw pipe bytes.
//! Extract just the engine input pipe in the post-dump worker. An inherited pipe skips CRIU's replay.
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use super::{receive, stdio};

const COMMON_MAGIC: u32 = 0x54564319;
const PIPES_DATA_MAGIC: u32 = 0x56453709;

fn invalid() -> io::Error { io::Error::new(io::ErrorKind::InvalidData, "invalid CRIU pipe data image") }

fn number(input: &mut &[u8]) -> io::Result<u32> {
    let mut result = 0;
    for shift in (0..32).step_by(7) {
        let (byte, rest) = input.split_first().ok_or_else(invalid)?;
        *input = rest;
        if shift == 28 && *byte > 15 { return Err(invalid()); }
        result |= u32::from(*byte & 0x7f) << shift;
        if *byte < 128 { return Ok(result); }
    }
    Err(invalid())
}

fn word(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn kernel_input(images: &Path, id: u32, output: &mut File) -> io::Result<()> {
    let file = match File::open(images.join("pipes-data.img")) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let end = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    if word(&mut reader)? != COMMON_MAGIC || word(&mut reader)? != PIPES_DATA_MAGIC { return Err(invalid()); }
    while !reader.fill_buf()?.is_empty() {
        let len = word(&mut reader)? as usize;
        let mut header = [0; 128];
        let header = header.get_mut(..len).ok_or_else(invalid)?;
        reader.read_exact(header)?;
        let mut fields = &*header;
        let (mut pipe, mut bytes) = (None, None);
        while !fields.is_empty() {
            let key = number(&mut fields)?;
            // Current PipeDataEntry fields are uint32 (wire type 0).
            if key == 0 || key & 7 != 0 { return Err(invalid()); }
            let value = number(&mut fields)?;
            match key >> 3 {
                1 => pipe = Some(value),
                2 => bytes = Some(value),
                _ => {},
            }
        }
        let pipe = pipe.ok_or_else(invalid)?;
        let bytes = u64::from(bytes.ok_or_else(invalid)?);
        let next = reader.stream_position()?.checked_add(bytes).filter(|p| *p <= end).ok_or_else(invalid)?;
        if pipe == id {
            if io::copy(&mut reader.take(bytes), output)? != bytes { return Err(invalid()); }
            return Ok(());
        }
        reader.seek(SeekFrom::Start(next))?;
    }
    Ok(())
}

pub(super) fn capture(images: &Path) -> io::Result<()> {
    let streams: stdio::CheckpointStdio = receive::manifest(&images.join(stdio::MANIFEST))?;
    if let Some(stdin) = streams.stdin {
        let mut data = File::create_new(images.join("stdin.bin"))?;
        kernel_input(images, stdin.get(), &mut data)?;
        if let Some(path) = std::env::var_os("ASAGIRI_CHECKPOINT_REPLAY_FILE") {
            let mut replay = File::open(path)?;
            let offset = std::env::var("ASAGIRI_CHECKPOINT_REPLAY_OFFSET").map_err(io::Error::other)?.parse().map_err(io::Error::other)?;
            replay.seek(SeekFrom::Start(offset))?;
            io::copy(&mut replay, &mut data)?;
        }
        if let Some(path) = std::env::var_os("ASAGIRI_CHECKPOINT_QUEUED_INPUT") {
            io::copy(&mut File::open(path)?, &mut data)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn extracts_the_selected_pipe_and_rejects_incomplete_records() {
        let dir = std::env::temp_dir().join(format!("asagiri-pipe-data-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("pipes-data.img");
        let mut source = File::create(&image).unwrap();
        for magic in [COMMON_MAGIC, PIPES_DATA_MAGIC] { source.write_all(&magic.to_le_bytes()).unwrap(); }
        for (id, bytes) in [(3, &b"other"[..]), (7, &b"\x00\xffhello"[..])] {
            source.write_all(&4u32.to_le_bytes()).unwrap();
            source.write_all(&[8, id, 16, bytes.len() as u8]).unwrap();
            source.write_all(bytes).unwrap();
        }
        let output = dir.join("result");
        kernel_input(&dir, 7, &mut File::create(&output).unwrap()).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"\x00\xffhello");
        source.set_len(source.metadata().unwrap().len() - 1).unwrap();
        assert!(kernel_input(&dir, 7, &mut File::create(&output).unwrap()).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
