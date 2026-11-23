//! Expand the mounted ext4 filesystem to the backing device's capacity.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;

pub fn grow() -> io::Result<()> {
    let mut disk = File::open("/dev/vda")?;
    let bytes = disk.seek(SeekFrom::End(0))?;
    // The data disk has already mounted as ext4. Read its total block
    // count, including metadata (statfs excludes metadata from f_blocks).
    disk.seek(SeekFrom::Start(1024))?;
    let mut sb = [0; 1024];
    disk.read_exact(&mut sb)?;
    let word = |offset: usize| u32::from_le_bytes(sb[offset..offset + 4].try_into().unwrap());
    let mut current = u64::from(word(0x04));
    if word(0x60) & 0x80 != 0 {
        current |= u64::from(word(0x150)) << 32;
    }
    let fs = File::open(super::DATA)?;
    // SAFETY: statfs is plain data; fstatfs writes it before it is read.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: fs is open and stat is valid writable storage.
    super::sys::check(
        unsafe { libc::fstatfs(fs.as_raw_fd(), &mut stat) } as _,
        "data disk statfs",
    )?;
    let block_size = 1024u64.checked_shl(word(0x18));
    if sb[0x38..0x3a] != [0x53, 0xef]
        || stat.f_type as u64 != libc::EXT4_SUPER_MAGIC as u64
        || stat.f_bsize <= 0
        || block_size != Some(stat.f_bsize as u64)
        || fs.metadata()?.dev() != disk.metadata()?.rdev()
    {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected ext4 data device or superblock"));
    }
    let blocks = bytes / stat.f_bsize as u64;
    if current == 0 || current > blocks {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid ext4 block count for the data device"));
    }
    if blocks == current {
        return Ok(());
    }
    // EXT4_IOC_RESIZE_FS = _IOW('f', 16, __u64), on both guest architectures.
    // SAFETY: fs is the mounted ext4 directory and blocks is a valid u64.
    super::sys::check(
        unsafe { libc::ioctl(fs.as_raw_fd(), 0x40086610 as libc::Ioctl, &blocks) } as _,
        "grow ext4 data disk",
    )?;
    Ok(())
}

/// Set up the VM's swap before accepting container requests.
pub fn configure_swap() -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let cmdline = std::fs::read_to_string("/proc/cmdline")?;
    let size = cmdline.split_ascii_whitespace().find_map(|arg| arg.strip_prefix("asagiri.swap_mib="))
        .unwrap_or("0").parse::<u64>().map_err(io::Error::other)?;
    let path = c"/var/lib/asagiri/.vm-swap";
    let filename = std::path::Path::new(super::DATA).join(".vm-swap");
    match std::fs::remove_file(&filename) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    if size == 0 { return Ok(()); }
    // SAFETY: sysconf only queries the running kernel's page size.
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).map_err(io::Error::other)?;
    if page < 4096 { return Err(io::Error::other("invalid swap page size")); }
    let bytes = size.checked_mul(1 << 20).ok_or_else(|| io::Error::other("swap size overflow"))?;
    let last = u32::try_from(bytes / page as u64 - 1).map_err(io::Error::other)?;
    let mut file = File::options().write(true).create_new(true).mode(0o600).open(&filename)?;
    let result = (|| {
        // Allocate physical extents; Linux rejects sparse swap files.
        let err = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, i64::try_from(bytes).map_err(io::Error::other)?) };
        if err != 0 { return Err(io::Error::from_raw_os_error(err)); }
        let mut header = vec![0; page];
        header[1024..1028].copy_from_slice(&1u32.to_ne_bytes());
        header[1028..1032].copy_from_slice(&last.to_ne_bytes());
        header[page - 10..].copy_from_slice(b"SWAPSPACE2");
        file.write_all(&header)?;
        drop(file);
        // SAFETY: path is a static NUL-terminated pathname.
        super::sys::check(unsafe { libc::swapon(path.as_ptr(), 0) } as _, "enable guest swap")?;
        Ok(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(filename); }
    result
}
