//! A disposable Linux daemon for exercising the actual WSL startup script.
//! It deliberately removes its socket before a slow shutdown completes.
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn stop(_: i32) {
    STOP.store(true, Ordering::SeqCst);
}

unsafe extern "C" {
    fn signal(number: i32, handler: extern "C" fn(i32)) -> usize;
}

fn main() -> std::io::Result<()> {
    if std::env::var("ASAGIRI_TEST_WSL_START").as_deref() != Ok("1")
        || std::env::args().skip(1).collect::<Vec<_>>() != ["daemon"]
    {
        return Err(std::io::Error::other("Use only in the disposable WSL startup fixture"));
    }
    let mut launches = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/launches")?;
    writeln!(launches, "{}", std::process::id())?;
    drop(launches);
    let mode = std::fs::read_to_string("/tmp/mode")?;
    if mode.trim() == "exits" {
        std::process::exit(1);
    }
    // The handler only stores a lock-free flag. Files and waits stay here.
    if unsafe { signal(15, stop) } == usize::MAX {
        return Err(std::io::Error::last_os_error());
    }
    let socket = "/var/lib/asagiri-daemon/.asagiri/run/docker.sock";
    std::thread::sleep(Duration::from_millis(500));
    let _listener = if matches!(mode.trim(), "up" | "dies") {
        std::fs::create_dir_all("/var/lib/asagiri-daemon/.asagiri/run")?;
        Some(UnixListener::bind(socket)?)
    } else {
        None
    };
    if mode.trim() == "dies" {
        std::thread::sleep(Duration::from_millis(300));
        return Ok(());
    }
    while !STOP.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Err(error) = std::fs::remove_file(socket)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error);
    }
    let seconds = std::fs::read_to_string("/tmp/stopwait")
        .ok().and_then(|value| value.trim().parse().ok()).unwrap_or(4);
    std::thread::sleep(Duration::from_secs(seconds));
    Ok(())
}
