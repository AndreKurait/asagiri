//! Run inside a fresh Alpine ARM64 container on a 2 CPU, 8 GiB Asagiri Mac VM.
//! Cross-compile with `rustc --edition=2024 --target aarch64-unknown-linux-musl -C linker=rust-lld -O`.
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::fd::AsRawFd,
    time::{Duration, Instant},
};
unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
}
fn field(path: &str, key: &str) -> u64 {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .find_map(|line| {
            let mut p = line.split_whitespace();
            (p.next() == Some(key)).then(|| p.next().unwrap().parse().unwrap())
        })
        .unwrap()
}
fn main() {
    let len = 512 * 1024 * 1024;
    let mut f = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open("/tmp/mapped")
        .unwrap();
    let data = vec![73u8; 1024 * 1024];
    for _ in 0..512 {
        f.write_all(&data).unwrap()
    }
    f.sync_all().unwrap();
    drop(f);
    let f = File::open("/tmp/mapped").unwrap();
    let p = unsafe { mmap(std::ptr::null_mut(), len, 1, 1, f.as_raw_fd(), 0) };
    assert_ne!(p as usize, usize::MAX);
    println!("pass,elapsed_s,sweep_ms,read_bytes,cgroup_file_bytes,refault_pages,guest_cached_kib");
    let start = Instant::now();
    let mut warm_counters = None;
    for n in 0..=24 {
        let t = Instant::now();
        let mut sum = 0u64;
        for i in (0..len).step_by(4096) {
            sum += unsafe { p.add(i).read_volatile() } as u64
        }
        assert_eq!(sum, (len / 4096 * 73) as u64);
        let reads = field("/proc/self/io", "read_bytes:");
        let refaults = field("/sys/fs/cgroup/memory.stat", "workingset_refault_file");
        println!(
            "{n},{:.3},{:.3},{},{},{},{}",
            start.elapsed().as_secs_f64(),
            t.elapsed().as_secs_f64() * 1000.,
            reads,
            field("/sys/fs/cgroup/memory.stat", "file"),
            refaults,
            field("/proc/meminfo", "Cached:")
        );
        if n == 1 {
            warm_counters = Some((reads, refaults));
        }
        if n == 24 {
            assert_eq!(Some((reads, refaults)), warm_counters);
        }
        if n < 24 {
            std::thread::sleep(Duration::from_secs(5));
        }
    }
}
