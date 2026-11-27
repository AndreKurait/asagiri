//! Read the GNU tar archives saved with a checkpoint. PAX lengths count bytes: an ACL, an
//! xattr value, or a path may contain embedded newlines. Keep file contents on
//! disk and pass one member at a time to the network file or OCI layer decoder.
use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use super::super::{layers, sys};

fn invalid() -> io::Error { io::Error::other("invalid checkpoint: malformed tar archive") }

#[derive(Default)]
pub(super) struct Fields {
    path: Option<Vec<u8>>,
    sparse_version: (Option<u8>, Option<u8>),
    sparse_name: Option<Vec<u8>>,
    sparse_size: Option<u64>,
    link: Option<Vec<u8>>,
    size: Option<u64>,
    uid: Option<u32>,
    gid: Option<u32>,
    mtime: Option<libc::timespec>,
    atime: Option<libc::timespec>,
    attrs: Vec<(Vec<u8>, Vec<u8>)>,
}

fn decimal<T: std::str::FromStr>(value: &[u8]) -> io::Result<T> {
    std::str::from_utf8(value).ok().and_then(|s| s.parse().ok()).ok_or_else(invalid)
}

impl Fields {
    pub(super) fn unpack_regular(&self, target: &Path, header: &tar::Header, member: &mut dyn Read) -> io::Result<File> {
        let mut tar = tar::Archive::new(member);
        let mut entries = tar.entries()?;
        let mut entry = entries.next().ok_or_else(|| io::Error::other("invalid checkpoint: missing regular file"))??;
        let mut out = File::create_new(target)?;
        if let Some(logical_size) = self.sparse_size {
            unpack_sparse(&mut entry, &mut out, header.entry_size()?, logical_size)?;
        } else if io::copy(&mut entry, &mut out)? != header.entry_size()? {
            return Err(io::Error::other("invalid checkpoint: truncated regular file"));
        }
        // The changeset reader validated the numeric IDs before this callback.
        std::os::unix::fs::fchown(&out, Some(header.uid()? as u32), Some(header.gid()? as u32))?;
        out.set_permissions(fs::Permissions::from_mode(header.mode()? & 0o7777))?;
        self.apply_attributes_and_times(target, header)?;
        Ok(out)
    }

    pub(super) fn apply_attributes_and_times(&self, path: &Path, header: &tar::Header) -> io::Result<()> {
        let target = sys::cstr(path)?;
        for (name, value) in &self.attrs {
            let name = std::ffi::CString::new(name.as_slice())?;
            // SAFETY: NUL-terminated names, exact value length, no final-link following.
            sys::check(unsafe { libc::lsetxattr(target.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0) } as _, "restore checkpoint xattr")?;
        }
        let atime = self.atime.unwrap_or(libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT });
        let mtime = match self.mtime {
            Some(time) => time,
            None => libc::timespec { tv_sec: header.mtime()?.try_into().map_err(io::Error::other)?, tv_nsec: 0 },
        };
        // SAFETY: two initialized timespecs; the target is a validated path in the private restore directory.
        sys::check(unsafe { libc::utimensat(libc::AT_FDCWD, target.as_ptr(), [atime, mtime].as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } as _, "restore checkpoint times")?;
        Ok(())
    }

    fn read_pax(&mut self, mut data: &[u8]) -> io::Result<()> {
        let (mut access_acl, mut default_acl) = (false, false);
        while !data.is_empty() {
            let space = data.iter().position(|&b| b == b' ').ok_or_else(invalid)?;
            let size: usize = decimal(&data[..space])?;
            let (line, rest) = data.split_at_checked(size).ok_or_else(invalid)?;
            let record = line.get(space + 1..).and_then(|s| s.strip_suffix(b"\n")).ok_or_else(invalid)?;
            let equals = record.iter().position(|&b| b == b'=').ok_or_else(invalid)?;
            let (key, value) = (&record[..equals], &record[equals + 1..]);
            match key {
                b"path" => self.path = Some(value.to_vec()),
                b"linkpath" => self.link = Some(value.to_vec()),
                b"size" => self.size = Some(decimal(value)?),
                b"uid" => self.uid = Some(decimal(value)?),
                b"gid" => self.gid = Some(decimal(value)?),
                b"mtime" => self.mtime = Some(layers::pax_node_time(value)?),
                b"atime" => self.atime = Some(layers::pax_node_time(value)?),
                b"SCHILY.acl.access" => access_acl = true,
                b"SCHILY.acl.default" => default_acl = true,
                b"GNU.sparse.major" => self.sparse_version.0 = Some(decimal(value)?),
                b"GNU.sparse.minor" => self.sparse_version.1 = Some(decimal(value)?),
                b"GNU.sparse.name" => self.sparse_name = Some(value.to_vec()),
                b"GNU.sparse.realsize" => self.sparse_size = Some(decimal(value)?),
                key if key.starts_with(b"GNU.sparse.") => return Err(io::Error::other("unsupported sparse checkpoint encoding")),
                key => if let Some(name) = key.strip_prefix(b"SCHILY.xattr.") {
                    if name.is_empty() || name.contains(&0) || name.starts_with(b"trusted.overlay.") {
                        return Err(invalid());
                    }
                    self.attrs.push((name.to_vec(), value.to_vec()));
                },
            }
            data = rest;
        }
        // Asagiri captures --acls and --xattrs together. The binary Linux ACL
        // preserves numeric IDs without resolving names on the destination.
        for (present, name) in [(access_acl, b"system.posix_acl_access".as_slice()), (default_acl, b"system.posix_acl_default".as_slice())] {
            if present && !self.attrs.iter().any(|(key, _)| key == name) {
                return Err(io::Error::other("invalid checkpoint: missing binary Linux ACL"));
            }
        }
        Ok(())
    }
}

/// GNU tar's sparse PAX 1.0 payload starts with decimal offset/length pairs,
/// padded to a tar block. Only the following extent bytes occupy disk space.
fn unpack_sparse(input: &mut dyn Read, out: &mut File, stored: u64, logical: u64) -> io::Result<()> {
    fn invalid() -> io::Error { io::Error::other("invalid checkpoint: malformed sparse file") }
    let mut input = BufReader::new(input);
    let mut consumed = 0u64;
    let mut number = || -> io::Result<u64> {
        let mut value = 0u64;
        for digits in 0..=20 {
            let mut byte = [0];
            input.read_exact(&mut byte)?;
            consumed += 1;
            if byte[0] == b'\n' && digits > 0 { return Ok(value); }
            if !byte[0].is_ascii_digit() { return Err(invalid()); }
            value = value.checked_mul(10).and_then(|n| n.checked_add((byte[0] - b'0').into())).ok_or_else(invalid)?;
        }
        Err(invalid())
    };
    let count = number()?;
    if count > stored / 4 || count > 1_048_576 { return Err(invalid()); }
    let mut extents = Vec::new();
    let mut end = 0;
    let mut bytes = 0u64;
    for _ in 0..count {
        let (offset, length) = (number()?, number()?);
        if offset < end { return Err(invalid()); }
        end = offset.checked_add(length).filter(|n| *n <= logical).ok_or_else(invalid)?;
        bytes = bytes.checked_add(length).ok_or_else(invalid)?;
        extents.push((offset, length));
    }
    let prefix = consumed.next_multiple_of(512);
    if prefix.checked_add(bytes) != Some(stored) { return Err(invalid()); }
    let mut pad = [0; 511];
    let pad = &mut pad[..(prefix - consumed) as usize];
    input.read_exact(pad)?;
    if pad.iter().any(|&b| b != 0) { return Err(invalid()); }
    for (offset, length) in extents {
        out.seek(SeekFrom::Start(offset))?;
        if io::copy(&mut (&mut input).take(length), out)? != length { return Err(invalid()); }
    }
    out.set_len(logical)
}

fn long_header(out: &mut Vec<u8>, kind: tar::EntryType, value: &[u8]) -> io::Result<()> {
    if value.is_empty() || value.contains(&0) || value.len() >= libc::PATH_MAX as usize { return Err(invalid()); }
    let mut header = tar::Header::new_gnu();
    header.set_path("././@LongLink")?;
    header.set_entry_type(kind);
    header.set_size(value.len() as u64 + 1);
    header.set_cksum();
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(value);
    out.push(0);
    out.resize(out.len().next_multiple_of(512), 0);
    Ok(())
}

pub(super) enum ArchiveKind { WritableLayer, NetworkFiles, Volume }

pub(super) fn visit(input: &mut File, archive: ArchiveKind, mut member: impl FnMut(&Path, &tar::Header, &Fields, &mut dyn Read) -> io::Result<()>) -> io::Result<()> {
    input.rewind()?;
    let length = input.metadata()?.len();
    let mut fields = Fields::default();
    let mut extensions = 0u8;
    loop {
        let mut header = tar::Header::new_old();
        input.read_exact(header.as_mut_bytes())?;
        if header.as_bytes().iter().all(|&b| b == 0) {
            let remaining = length.checked_sub(input.stream_position()?).ok_or_else(invalid)?;
            if extensions != 0 || remaining < 512 || !remaining.is_multiple_of(512) { return Err(invalid()); }
            let mut buffer = [0; 8192];
            loop {
                let n = input.read(&mut buffer)?;
                if n == 0 { return Ok(()); }
                if buffer[..n].iter().any(|&b| b != 0) { return Err(invalid()); }
            }
        }
        let checksum = header.cksum()?;
        header.set_cksum();
        if header.cksum()? != checksum { return Err(invalid()); }
        let kind = header.entry_type();
        let extension = kind.is_pax_local_extensions() || kind.is_gnu_longname() || kind.is_gnu_longlink();
        let size = if extension { header.entry_size()? } else { fields.size.unwrap_or(header.entry_size()?) };
        let padded = size.checked_next_multiple_of(512).ok_or_else(invalid)?;
        let end = input.stream_position()?.checked_add(padded).filter(|&n| n <= length).ok_or_else(invalid)?;
        if extension {
            if size > 16 * 1024 * 1024 { return Err(io::Error::other("invalid checkpoint: oversized tar metadata")); }
            let bit = if kind.is_pax_local_extensions() { 1 } else if kind.is_gnu_longname() { 2 } else { 4 };
            if extensions & bit != 0 { return Err(invalid()); }
            extensions |= bit;
            let mut data = vec![0; size as usize];
            input.read_exact(&mut data)?;
            if kind.is_pax_local_extensions() { fields.read_pax(&data)?; }
            else {
                if data.pop() != Some(0) { return Err(invalid()); }
                if kind.is_gnu_longname() { fields.path = Some(data); } else { fields.link = Some(data); }
            }
        } else {
            if !(kind.is_file() || kind.is_dir() || kind.is_fifo() || kind.is_symlink() || kind.is_hard_link()
                || kind.is_character_special() || kind.is_block_special()) { return Err(invalid()); }
            if fields.sparse_version != (None, None) || fields.sparse_name.is_some() || fields.sparse_size.is_some() {
                if matches!(archive, ArchiveKind::WritableLayer) || fields.sparse_version != (Some(1), Some(0))
                    || !kind.is_file() || fields.sparse_size.is_none() || fields.sparse_name.is_none() { return Err(invalid()); }
                fields.path = fields.sparse_name.take();
            }
            let original = header.path_bytes().into_owned();
            let path = Path::new(std::ffi::OsStr::from_bytes(fields.path.as_deref().unwrap_or(&original)));
            if path.as_os_str().is_empty() || !path.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)) { return Err(invalid()); }
            if let Some(uid) = fields.uid { header.set_uid(uid.into()); }
            if let Some(gid) = fields.gid { header.set_gid(gid.into()); }
            // Early checkpoint writers left the synthetic OCI whiteout's
            // ownership fields blank. The kernel marker has no source owner.
            let marker = matches!(archive, ArchiveKind::WritableLayer) && path.file_name().is_some_and(|s| s.as_encoded_bytes().starts_with(b".wh."));
            if marker {
                if !kind.is_file() || size != 0 { return Err(invalid()); }
                header.set_uid(0);
                header.set_gid(0);
                header.set_mtime(0);
            } else if header.uid()? >= u64::from(u32::MAX) || header.gid()? >= u64::from(u32::MAX) { return Err(invalid()); }
            if let Some(time) = fields.mtime { header.set_mtime(time.tv_sec.max(0) as u64); }
            header.set_size(size);
            header.set_cksum();
            let mut prefix = Vec::new();
            if let Some(path) = &fields.path { long_header(&mut prefix, tar::EntryType::GNULongName, path)?; }
            if let Some(link) = &fields.link { long_header(&mut prefix, tar::EntryType::GNULongLink, link)?; }
            let mut stream = prefix.as_slice().chain(header.as_bytes().as_slice()).chain((&mut *input).take(padded));
            member(path, &header, &fields, &mut stream)?;
            fields = Fields::default();
            extensions = 0;
        }
        input.seek(SeekFrom::Start(end))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_multiline_acl_and_binary_xattr_in_gnu_pax_records() {
        let mut tar = tar::Builder::new(Vec::new());
        let binary = b"\x02\0\0\0\n\xff";
        tar.append_pax_extensions([
            ("SCHILY.acl.access", b"user::rwx\ngroup::r-x\nother::--x".as_slice()),
            ("SCHILY.xattr.system.posix_acl_access", binary.as_slice()),
            ("path", b"./line\nbreak".as_slice()),
            ("mtime", b"1710000011.123456789".as_slice()),
        ]).unwrap();
        let bytes = tar.into_inner().unwrap();
        let mut archive = tar::Archive::new(bytes.as_slice());
        let mut entries = archive.entries().unwrap().raw(true);
        let mut payload = Vec::new();
        entries.next().unwrap().unwrap().read_to_end(&mut payload).unwrap();
        let mut fields = Fields::default();
        fields.read_pax(&payload).unwrap();
        assert_eq!(fields.path.as_deref(), Some(b"./line\nbreak".as_slice()));
        assert_eq!(fields.attrs, [(b"system.posix_acl_access".to_vec(), binary.to_vec())]);
        assert_eq!(fields.mtime.unwrap().tv_nsec, 123456789);
        assert!(Fields::default().read_pax(&payload[..payload.len() - 1]).is_err());
    }
}
