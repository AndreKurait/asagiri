//! The agent's event loop: one poll over the host connection, a signalfd for
//! child exits, and every process's output (and pending input) descriptors.
//! Single-threaded, which is also what lets clone3 copy the process safely.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use asagiri_proto::{Exited, MAX_OUTPUT_CHUNK, MemoryPressure, Msg, Stats, Stream};

use super::container::{self, NetCtx, Paths, Process};
use super::net::{self, Bridges, Dhcp};
use super::roots::Roots;
use super::{fsnotify, layers, meminfo, sys};

/// Input held for a process that is not reading yet. The host sends at most
/// `INPUT_WINDOW` bytes beyond what `InputConsumed` acknowledged, so a
/// correct host never gets here; a broken one has the process's stdin closed
/// rather than the agent holding unbounded memory.
const MAX_PENDING_INPUT: usize = 2 * asagiri_proto::INPUT_WINDOW;

struct Proc {
    run: Process,
    received_ns: u64,
    cloned_ns: u64,
    /// Set once reaped: (exit code, when).
    exited: Option<(i32, u64)>,
    input: VecDeque<u8>,
    close_stdin_when_drained: bool,
    /// When a running TTY process last had its terminal hung up: no one
    /// held the pty open, as while systemd reopens /dev/console. Kept, and
    /// looked at again shortly, rather than taken for the end of output.
    tty_hup: Option<std::time::Instant>,
    // A reaped container is retained until its filesystem worker finishes.
    cleanup: Option<libc::pid_t>,
    cleanup_finished: bool,
    /// Retained before early host-PID cleanup removes the cgroup.
    oom_killed: Option<bool>,
    exit_announced: bool,
    root_released: bool,
}

// Linux can reuse a reaped PID while its output is draining or its filesystem
// worker is still running. Container/exec IDs remain reserved until cleanup.
type Procs = HashMap<String, Proc>;

/// How long a hung-up terminal waits before it's polled again.
const TTY_HUP_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

pub(super) fn serve(mut conn: File, paths: &Paths, runtime: Option<&super::checkpoint::Runtime>) -> io::Result<()> {
    // The VM agent is already PID 1. On a Linux host, orphaned descendants
    // of host-PID containers (including ones without --init) belong to
    // this agent and its existing waitpid loop, not an unrelated host init.
    super::pid1::adopt_orphans()?;
    let sigfd = child_signals()?;
    // In the VM a DHCP lease gives the uplink and DNS; on a Linux host the
    // host has them already.
    let host_process = super::transport::is_host_process();
    // The published ports the kernel forwards, by container (on a Linux
    // host; in the VM, TCP ones reached over the NAT network).
    let mut published: Vec<(String, super::netlink::Dnat)> = Vec::new();
    let host_dns = if host_process { super::transport::host_dns() } else { Vec::new() };
    let mut dhcp: Option<Dhcp> = if host_process {
        if let Err(e) = net::host_uplink() {
            eprintln!("asagiri-init: no uplink: {e}");
        }
        None
    } else {
        match net::uplink() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("asagiri-init: no uplink: {e}");
                None
            }
        }
    };
    // DHCP continues in the event loop after the initial uplink grace period.
    let uplink_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    // On a Linux host, SIGTERM or SIGINT stops the agent as Shutdown does.
    let term = if host_process { Some(term_signals()?) } else { None };
    let mut bridges = Bridges::default();
    // In the VM, lookups the gateway's resolver fails go through the host.
    let mut dns = super::dns::Dns::default();
    dns.relay = !host_process;
    let forward = super::relay::listen()?;
    // host.docker.internal's relay to the Mac (VM only). A Linux host maps
    // that name to the bridge gateway and redirects nothing here, so the
    // listener would only be a port open to the LAN and a clash with any
    // host service on it.
    let host_bound = if host_process { None } else { Some(super::relay::listen_host_bound(net::HOST_RELAY_PORT)?) };
    let framed = super::relay::listen_framed()?;
    let buildkit = super::relay::listen_vsock(asagiri_proto::BUILDKIT_PORT)?;
    let ssh_agent = super::relay::listen_ssh_agent()?;
    // The engine's API for containers that mount the docker socket (VM only).
    let engine_api = if host_process {
        None
    } else {
        super::relay::listen_engine().inspect_err(|e| eprintln!("asagiri-init: no engine socket for containers: {e}")).ok()
    };
    let copies = super::copy::listen()?;
    let layer_uploads = layers::listen()?;
    // USB passthrough (VM only: on a Linux host containers see the host's devices).
    let usb = if host_process { None } else { super::usb::listen().inspect_err(|e| eprintln!("asagiri-init: no USB passthrough: {e}")).ok() };
    let udp = super::relay::listen_udp()?;
    let pressure = memory_pressure_trigger();
    let mut last_pressure = std::time::Instant::now() - std::time::Duration::from_secs(1);
    // `docker system df`: measured on a thread, which wakes the loop through
    // this pipe; the loop sends the `Usage` lines (it is the only writer).
    let (usage_wake, usage_ring) = usage_pipe()?;
    let usage_ring = std::sync::Arc::new(usage_ring);
    let (usage_tx, usage_rx) = std::sync::mpsc::channel::<Vec<String>>();
    let mut measuring = false;
    let mut relays: Vec<super::relay::Relay> = Vec::new();
    let mut procs = Procs::new();
    let roots = Roots::default();
    let mut volumes = super::volumes::Volumes::new(Path::new(container::VOLUMES))?;
    // Drop/stop the direct copy children before their lease table and its
    // filesystem workers, including when an I/O error leaves this loop.
    let mut copy_jobs = super::copy::Broker::new(&roots);
    let mut checkpoint: Option<super::checkpoint::Capture> = None;
    let mut fds: Vec<libc::pollfd> = Vec::with_capacity(16);
    let mut frame = Vec::with_capacity(4096);
    let mut out = Vec::with_capacity(4096);
    let mut chunk = vec![0u8; MAX_OUTPUT_CHUNK];
    let mut fs_buf = Vec::new();
    let mut fs_events = fsnotify::Pending::default();

    loop {
        if let Some(capture) = checkpoint.as_mut() {
            let result = match capture.poll() {
                Ok(None) => None,
                Ok(Some(())) => Some(String::new()),
                Err(error) => Some(format!("checkpoint: {error}")),
            };
            if let Some(error) = result {
                checkpoint = None;
                // A target exit may already have woken poll while the dump
                // still owned its PID. Reap it now without another signal.
                reap(&mut procs, &mut dns, &roots, &mut copy_jobs, &mut volumes, None);
                out.clear();
                Msg::CheckpointResult { error: &error }.encode(&mut out);
                conn.write_all(&out)?;
            }
        }
        roots.start(paths, std::time::Instant::now());
        volumes.start(std::time::Instant::now());
        copy_jobs.advance(paths, |id| copy_view(&procs, id), |volume| checkpoint.as_ref().is_some_and(|c| c.uses_volume(volume)));
        root_events(&roots, &procs, &mut conn, &mut out)?;
        fds.clear();
        fds.push(libc::pollfd { fd: conn.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: sigfd.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        // Always present (fd -1 is ignored by poll), so process fds start at 3.
        fds.push(libc::pollfd { fd: dhcp.as_ref().map_or(-1, Dhcp::fd), events: libc::POLLIN, revents: 0 });
        for fd in dns.fds() {
            fds.push(libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
        }
        fds.push(libc::pollfd { fd: forward.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: framed.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: buildkit.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: host_bound.as_ref().map_or(-1, |l| l.as_raw_fd()), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: ssh_agent.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: engine_api.as_ref().map_or(-1, |l| l.as_raw_fd()), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: copies.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: layer_uploads.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: usb.as_ref().map_or(-1, |l| l.as_raw_fd()), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: udp.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: pressure.as_ref().map_or(-1, |f| f.as_raw_fd()), events: libc::POLLPRI, revents: 0 });
        fds.push(libc::pollfd { fd: term.as_ref().map_or(-1, |f| f.as_raw_fd()), events: libc::POLLIN, revents: 0 });
        fds.push(libc::pollfd { fd: usage_wake.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        for r in &relays {
            r.interest(&mut fds);
        }
        copy_jobs.interest(&mut fds);
        let mut hung = false;
        for p in procs.values() {
            // A hung-up pty polls as ready all the time; skip it a moment.
            if p.tty_hup.is_some_and(|t| t.elapsed() < TTY_HUP_WAIT) {
                hung = true;
                continue;
            }
            for fd in [&p.run.stdout, &p.run.stderr].into_iter().flatten() {
                fds.push(libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 });
            }
            if let (Some(fd), false) = (&p.run.stdin, p.input.is_empty()) {
                fds.push(libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLOUT, revents: 0 });
            }
        }
        // SAFETY: `fds` is a valid array of pollfd of the given length.
        let mut timeout = dhcp.as_ref().map_or(-1, Dhcp::timeout_ms);
        if roots.needs_retry_poll() || volumes.needs_retry_poll() {
            timeout = if timeout < 0 { 250 } else { timeout.min(250) };
        }
        if hung {
            let wait = TTY_HUP_WAIT.as_millis() as i32;
            timeout = if timeout < 0 { wait } else { timeout.min(wait) };
        }
        let wait = dns.timeout_ms();
        if wait >= 0 {
            timeout = if timeout < 0 { wait } else { timeout.min(wait) };
        }
        // Changes from the Mac still to raise: a few now, the rest after
        // whatever else is ready.
        if !fs_events.is_empty() {
            fs_events.raise_some();
            if !fs_events.is_empty() {
                timeout = 0;
            }
        }
        // SAFETY: `fds` is a valid array of pollfd of the given length.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }

        if let Some(d) = dhcp.as_mut() {
            if fds[2].revents != 0 {
                match d.on_readable() {
                    Ok(true) => {
                        if let Some(l) = &d.lease {
                            eprintln!("asagiri-init: {} is {}/{} via {}", net::UPLINK, net::ip_str(l.ip), l.prefix, net::ip_str(l.gateway));
                        }
                    }
                    Ok(false) => {}
                    Err(e) => eprintln!("asagiri-init: dhcp: {e}"),
                }
            }
            if let Err(e) = d.on_timer() {
                eprintln!("asagiri-init: dhcp: {e}");
            }
        }

        dns.on_timer();

        // Output and input first, so a process's last words go out before its exit.
        for pfd in &fds[3..] {
            if pfd.revents == 0 {
                continue;
            }
            if pfd.fd == layer_uploads.as_raw_fd() {
                layers::accept(&layer_uploads, &paths.layers);
                continue;
            }
            if term.as_ref().is_some_and(|t| t.as_raw_fd() == pfd.fd) {
                stop_containers(&mut procs, &mut dns, &roots, &mut copy_jobs, &mut volumes, paths);
                return Ok(());
            }
            if pfd.fd == usage_wake.as_raw_fd() {
                let mut sink = [0u8; 16];
                // SAFETY: reading into a buffer we own from a descriptor we own.
                while unsafe { libc::read(usage_wake.as_raw_fd(), sink.as_mut_ptr().cast(), sink.len()) } > 0 {}
                while let Ok(lines) = usage_rx.try_recv() {
                    measuring = false;
                    out.clear();
                    asagiri_proto::encode_usage(&mut out, &lines);
                    conn.write_all(&out)?;
                }
                continue;
            }
            if pfd.fd == forward.as_raw_fd() {
                while let Some(r) = super::relay::accept(&forward) {
                    relays.push(r);
                }
                continue;
            }
            if pfd.fd == framed.as_raw_fd() {
                while let Some(r) = super::relay::accept_framed(&framed) {
                    relays.push(r);
                }
                continue;
            }
            if pfd.fd == buildkit.as_raw_fd() {
                while let Some(r) = super::relay::accept_buildkit(&buildkit) {
                    relays.push(r);
                }
                continue;
            }
            if pressure.as_ref().is_some_and(|f| f.as_raw_fd() == pfd.fd) {
                // At most ten a second: the host reacts to the first one.
                if last_pressure.elapsed() >= std::time::Duration::from_millis(100) {
                    last_pressure = std::time::Instant::now();
                    out.clear();
                    pressure_reading().encode(&mut out);
                    conn.write_all(&out)?;
                }
                continue;
            }
            if pfd.fd == udp.as_raw_fd() {
                while let Some(r) = super::relay::accept_udp(&udp) {
                    relays.push(r);
                }
                continue;
            }
            if let Some(l) = usb.as_ref().filter(|l| pfd.fd == l.as_raw_fd()) {
                super::usb::accept(l);
                continue;
            }
            if pfd.fd == copies.as_raw_fd() {
                copy_jobs.accept(&copies, paths);
                continue;
            }
            if pfd.fd == ssh_agent.as_raw_fd() {
                while let Some(r) = super::relay::accept_ssh_agent(&ssh_agent) {
                    relays.push(r);
                }
                continue;
            }
            if let Some(l) = engine_api.as_ref().filter(|l| l.as_raw_fd() == pfd.fd) {
                while let Some(r) = super::relay::accept_engine(l) {
                    relays.push(r);
                }
                continue;
            }
            if let Some(l) = host_bound.as_ref().filter(|l| l.as_raw_fd() == pfd.fd) {
                while let Some(r) = super::relay::accept_host_bound(l, super::net::HOST_ADDR) {
                    relays.push(r);
                }
                continue;
            }
            if let Some(r) = relays.iter_mut().find(|r| r.owns(pfd.fd)) {
                r.step();
                continue;
            }
            if dns.owns(pfd.fd) {
                let upstream = dhcp.as_ref().and_then(|d| d.lease.as_ref()).map_or(&host_dns[..], |l| &l.dns);
                dns.on_readable(pfd.fd, upstream);
                continue;
            }
            if copy_jobs.owns(pfd.fd) {
                copy_jobs.readable(pfd.fd);
                continue;
            }
            // Only process stdin is polled for writing beyond this point.
            if pfd.events & libc::POLLOUT != 0 {
                flush_input(&mut procs, pfd.fd, &mut conn, &mut out)?;
                continue;
            }
            let Some((p, stream)) = owner(&mut procs, pfd.fd) else { continue };
            let slot = if stream == Stream::Stdout { &mut p.run.stdout } else { &mut p.run.stderr };
            let fd = slot.as_ref().expect("polled fd is open").as_raw_fd();
            // SAFETY: reading into a buffer we own from a descriptor we own.
            let got = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if got > 0 {
                p.tty_hup = None;
                out.clear();
                Msg::Output { id: &p.run.id, stream, data: &chunk[..got as usize] }.encode(&mut out);
                conn.write_all(&out)?;
            } else if got < 0 && p.run.tty && p.exited.is_none() && io::Error::last_os_error().raw_os_error() == Some(libc::EIO) {
                // Nobody holds the terminal right now, but the process lives
                // and may open it again (systemd does): keep it.
                p.tty_hup = Some(std::time::Instant::now());
            } else if got == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN) {
                // EOF, or EIO from a pty whose other side has closed.
                *slot = None;
                if p.run.tty {
                    p.run.stdin = None;
                }
            }
        }

        relays.retain(|r| !r.dead);

        if fds[1].revents != 0 {
            drain_signals(&sigfd);
            reap(&mut procs, &mut dns, &roots, &mut copy_jobs, &mut volumes, checkpoint.as_mut());
        }

        // A private PID 1's exit makes the kernel kill its descendants.
        // A shared-PID process has no such kernel cleanup: its orphan may
        // still hold stdout/stderr open. Kill only its cgroup before waiting
        // for pipe EOF, preserving the existing output drain and per-ID
        // cleanup barrier. Execs never own this cleanup.
        for p in procs.values_mut().filter(|p| p.run.pid_shared && p.run.container.is_none()
            && p.exited.is_some() && p.cleanup.is_none() && !p.cleanup_finished) {
            if p.oom_killed.is_none() {
                p.oom_killed = Some(container::oom_killed(&p.run));
            }
            p.cleanup = container::begin_teardown(&p.run);
            p.cleanup_finished = p.cleanup.is_none();
        }

        // Announce the process exit promptly. A separate per-ID cleanup barrier
        // prevents restart/removal from racing the filesystem worker, without
        // making stop or unrelated control requests wait for overlay writeback.
        let done: Vec<String> = procs.iter()
            .filter(|(_, p)| p.exited.is_some() && p.run.stdout.is_none() && p.run.stderr.is_none())
            .map(|(id, _)| id.clone()).collect();
        for id in done {
            let shared = {
                let p = &procs[&id];
                p.run.netns != 0 && procs.values().any(|o| o.exited.is_none() && o.run.netns == p.run.netns)
            };
            let p = procs.get_mut(&id).expect("listed above");
            if !p.exit_announced {
                for (bridge, ip) in &p.run.dns_entries { dns.unregister(bridge, *ip); }
                container::teardown_network(&p.run, shared);
                let (code, reaped_ns) = p.exited.expect("filtered on exited");
                out.clear();
                if container::has_filesystem_teardown(&p.run) {
                    Msg::CleanupPending(&p.run.id).encode(&mut out);
                }
                let oom_killed = p.oom_killed.unwrap_or_else(|| container::oom_killed(&p.run));
                // A cgroup-limited workload can OOM before the first periodic
                // sample. Send current readings before announcing its exit.
                if oom_killed {
                    pressure_reading().encode(&mut out);
                }
                Msg::Exited(Exited { id: &p.run.id, code, setup_ns: p.cloned_ns - p.received_ns,
                    run_ns: reaped_ns - p.cloned_ns,
                    oom_killed }).encode(&mut out);
                conn.write_all(&out)?;
                p.exit_announced = true;
            }
            if p.cleanup.is_some() { continue; }
            if !p.cleanup_finished {
                p.cleanup = container::begin_teardown(&p.run);
                if p.cleanup.is_some() { continue; }
                p.cleanup_finished = true;
            }
            if roots.cleaning(&id) { continue; }
            let p = procs.remove(&id).expect("listed above");
            if container::has_filesystem_teardown(&p.run) {
                out.clear();
                Msg::CleanupDone(&p.run.id).encode(&mut out);
                conn.write_all(&out)?;
            }
        }

        if fds[0].revents != 0 {
            if !read_frame(&mut conn, &mut frame)? {
                // The host went away: on a Linux host, the daemon died. Its
                // containers do not outlive it (there is no live restore).
                if host_process {
                    stop_containers(&mut procs, &mut dns, &roots, &mut copy_jobs, &mut volumes, paths);
                }
                return Ok(());
            }
            let received_ns = sys::boottime_ns();
            out.clear();
            match asagiri_proto::decode(&frame) {
                Ok(Msg::Ping(v)) => Msg::Pong(v).encode(&mut out),
                Ok(Msg::Shutdown) => {
                    if host_process {
                        // The daemon has stopped them Docker's way already;
                        // what is left is killed and torn down.
                        stop_containers(&mut procs, &mut dns, &roots, &mut copy_jobs, &mut volumes, paths);
                    } else {
                        for p in procs.values().filter(|p| p.exited.is_none()) {
                            // SAFETY: signalling a child we started and have not reaped.
                            unsafe { libc::kill(p.run.pid, libc::SIGKILL) };
                        }
                    }
                    return Ok(());
                }
                Ok(Msg::StatsRequest) => {
                    let (mem_total_kib, mem_available_kib) = meminfo();
                    let agent_rss_kib = self_rss_kib();
                    let (mem_stall, oom_kills) = super::cgroup::memory_trouble();
                    let memory = memory_sample(paths, agent_rss_kib);
                    Msg::Stats(Stats { uptime_ns: sys::boottime_ns(), mem_total_kib, mem_available_kib, agent_rss_kib,
                        mem_used_kib: memory.held >> 10, mem_stall, oom_kills,
                        mem_working_set_kib: memory.working_set >> 10, mem_refaults: memory.refaults })
                        .encode(&mut out)
                }
                Ok(Msg::LayerQuery(digest)) => Msg::LayerStatus(layers::present(&paths.layers, digest)).encode(&mut out),
                // The blob follows the frame on the same stream.
                Ok(Msg::LayerBegin(begin)) => match layers::receive(&mut conn, &begin, &paths.layers) {
                    Ok(stored) => Msg::LayerStored(stored).encode(&mut out),
                    Err(e) => Msg::Error(&format!("unpack {}: {e}", begin.digest)).encode(&mut out),
                },
                Ok(Msg::Run(spec)) if checkpoint.as_ref().is_some_and(|c| spec.mounts.iter().filter_map(container::writable_volume).any(|name| c.uses_volume(name))) => {
                    Msg::Error("volume checkpoint in progress").encode(&mut out);
                }
                Ok(Msg::Run(spec)) if procs.contains_key(spec.id) || roots.removing(spec.id) => {
                    Msg::Error(&format!("container {} is running or being cleaned up", spec.id)).encode(&mut out);
                }
                Ok(Msg::Run(spec)) if roots.cleaning(spec.id) => {
                    // Ordinary control replies are FIFO. Answer now, then
                    // let only this host caller wait for CleanupDone/retry.
                    Msg::CleanupPending(spec.id).encode(&mut out);
                    Msg::Error(asagiri_proto::ROOT_CLEANUP_PENDING).encode(&mut out);
                }
                Ok(Msg::Run(spec)) => match {
                    // A container that wants the network must not start before the
                    // VM has it: right after boot the DHCP lease can be a few
                    // hundred milliseconds away.
                    if spec.flags & (asagiri_proto::run_flags::NET_NONE | asagiri_proto::run_flags::NET_HOST) == 0
                        && std::time::Instant::now() < uplink_deadline
                    {
                        wait_for_lease(&mut dhcp, uplink_deadline);
                    }
                    let join_pid = asagiri_proto::net_opts::values(spec.net.opts, asagiri_proto::net_opts::NETNS).next().and_then(|t| running_pid(&procs, t));
                    container::spawn(
                    &spec,
                    paths,
                    &roots,
                    NetCtx { bridges: &mut bridges, dns: &mut dns, upstream_dns: dhcp.as_ref().and_then(|d| d.lease.as_ref()).map_or(&host_dns[..], |l| &l.dns), join_pid },
                    |id| running_pid(&procs, id),
                )
                } {
                    Ok(run) => {
                        let cloned_ns = sys::boottime_ns();
                        Msg::Started { id: &run.id, pid: run.pid as u32 }.encode(&mut out);
                        procs.insert(run.id.clone(), new_proc(run, received_ns, cloned_ns));
                    }
                    Err(e) if super::startup::rejected(&e) => Msg::StartRejected(&e.to_string()).encode(&mut out),
                    Err(e) => Msg::Error(&format!("run {}: {e}", spec.id)).encode(&mut out),
                },
                Ok(Msg::Exec(spec)) if procs.contains_key(spec.exec_id) => {
                    Msg::Error(&format!("exec {} is running or being cleaned up", spec.exec_id)).encode(&mut out);
                }
                Ok(Msg::Exec(spec)) => {
                    let target = procs.values().find(|p| p.run.container.is_none() && p.run.id == spec.container_id && p.exited.is_none());
                    match target.map(|p| p.run.pid) {
                        None => Msg::Error(&format!("container {} is not running", spec.container_id)).encode(&mut out),
                        Some(pid) => match container::exec(&spec, pid, paths) {
                            Ok(run) => {
                                let cloned_ns = sys::boottime_ns();
                                Msg::Started { id: &run.id, pid: run.pid as u32 }.encode(&mut out);
                                procs.insert(run.id.clone(), new_proc(run, received_ns, cloned_ns));
                            }
                            Err(e) => Msg::Error(&format!("exec in {}: {e}", spec.container_id)).encode(&mut out),
                        },
                    }
                }
                Ok(Msg::Input { id, data }) => {
                    let mut target = None;
                    if let Some(p) = procs.values_mut().find(|p| p.run.id == id) {
                        if let Some(fd) = p.run.stdin.as_ref().map(|f| f.as_raw_fd()) {
                            if p.input.len() + data.len() > MAX_PENDING_INPUT {
                                p.input.clear();
                                p.run.stdin = None;
                            } else {
                                p.input.extend(data);
                                target = Some(fd);
                            }
                        }
                    }
                    if let Some(fd) = target {
                        flush_input(&mut procs, fd, &mut conn, &mut out)?;
                    }
                    continue;
                }
                Ok(Msg::CloseStdin { id }) => {
                    if let Some(p) = procs.values_mut().find(|p| p.run.id == id) {
                        // A terminal has no separate end of input; keep it open.
                        if !p.run.tty {
                            if p.input.is_empty() {
                                p.run.stdin = None;
                            } else {
                                p.close_stdin_when_drained = true;
                            }
                        }
                    }
                    continue;
                }
                Ok(Msg::Resize { id, rows, cols }) => {
                    resize(&procs, id, rows, cols);
                    continue;
                }
                Ok(Msg::Kill { id, signal }) => {
                    if let Some(p) = procs.values().find(|p| p.run.id == id && p.exited.is_none()) {
                        // SAFETY: signalling a child we started and have not reaped.
                        unsafe { libc::kill(p.run.pid, signal as libc::c_int) };
                    }
                    continue; // no reply: the exit, if any, is reported as Exited
                }
                Ok(Msg::Remove { id }) => {
                    if !procs.contains_key(id) && container::valid_id(id).is_ok() {
                        roots.remove(id);
                    }
                    continue;
                }
                Ok(Msg::Publish(p)) => {
                    // In the VM: from the Mac over the NAT network, TCP only.
                    let gateway = dhcp.as_ref().and_then(|d| d.lease.as_ref()).map(|l| l.gateway);
                    if !host_process && (p.udp || gateway.is_none()) {
                        Msg::Error("published ports are relayed on this platform").encode(&mut out);
                    } else {
                        let d = super::netlink::Dnat { udp: p.udp, host_ip: p.host_ip, host_port: p.host_port, to_ip: p.to_ip, to_port: p.to_port };
                        published.push((p.id.to_owned(), d));
                        let all = published.iter().map(|(_, d)| *d).collect::<Vec<_>>();
                        match if host_process { net::publish(&all) } else { net::publish_vm(&all, gateway.expect("checked")) } {
                            Ok(()) => Msg::Ack.encode(&mut out),
                            Err(e) => {
                                published.pop();
                                Msg::Error(&format!("publish {}:{}: {e}", p.host_port, p.to_port)).encode(&mut out)
                            }
                        }
                    }
                }
                Ok(Msg::Unpublish { id }) => {
                    let before = published.len();
                    published.retain(|(i, _)| i != id);
                    if published.len() != before {
                        let all = published.iter().map(|(_, d)| *d).collect::<Vec<_>>();
                        let gateway = dhcp.as_ref().and_then(|d| d.lease.as_ref()).map_or([0; 4], |l| l.gateway);
                        if let Err(e) = if host_process { net::publish(&all) } else { net::publish_vm(&all, gateway) } {
                            eprintln!("asagiri-init: unpublish {id}: {e}");
                        }
                    }
                    continue;
                }
                Ok(Msg::UsageQuery) => {
                    // No reply here: replies go in order, and this can take
                    // seconds. The lines follow as a `Usage` message.
                    if !measuring {
                        measuring = true;
                        let (layers, containers, tx, ring) = (paths.layers.clone(), paths.containers.clone(), usage_tx.clone(), usage_ring.clone());
                        let _ = std::thread::Builder::new().name("usage".into()).spawn(move || {
                            let _ = tx.send(super::usage::measure(&layers, &containers, Path::new(container::VOLUMES)));
                            // SAFETY: writing one byte from a buffer we own to a descriptor we own.
                            unsafe { libc::write(ring.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
                        });
                    }
                    continue;
                }
                Ok(Msg::RemoveVolume { name }) => {
                    if let Err(error) = volumes.remove(name) {
                        eprintln!("asagiri-init: volume {name}: {error}");
                    }
                    continue;
                }
                Ok(Msg::RemoveBridge { bridge }) => {
                    // Only Asagiri's own bridges, never another interface.
                    if bridge.starts_with("asagiri") && bridge != "asagiri0" {
                        dns.forget_bridge(bridge);
                        bridges.remove(bridge);
                    }
                    continue;
                }
                Ok(Msg::FsEvents(lines)) => {
                    fs_events.add(lines.iter());
                    continue;
                }
                Ok(Msg::Retain(keep)) => {
                    roots.retain(keep);
                    let leased = roots.unfinished();
                    let running: Vec<&str> = procs.values().map(|p| p.run.id.as_str()).chain(leased.iter().map(String::as_str)).collect();
                    container::retain(paths, keep, &running);
                    continue;
                }
                Ok(Msg::RetainLayers(keep)) => {
                    layers::retain(&paths.layers, keep.iter());
                    continue;
                }
                Ok(Msg::SetTime(ns)) => {
                    // A host process shares the host's clock; only the VM needs
                    // telling. The host sends it again every minute and after
                    // the Mac wakes: stepped only when off by more than a
                    // quarter second, so the clock is not nudged back and forth.
                    if !host_process && clock_off(ns) {
                        let ts = libc::timespec { tv_sec: (ns / 1_000_000_000) as _, tv_nsec: (ns % 1_000_000_000) as _ };
                        // SAFETY: `ts` is a valid timespec.
                        unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) };
                    }
                    continue;
                }
                Ok(Msg::Update { id, limits }) => match running_cgroup(&procs, id) {
                    Some((_, path)) => match super::cgroup::update_limits(path, &limits) {
                        Ok(()) => Msg::Ack.encode(&mut out),
                        Err(e) => Msg::Error(&format!("update: {e}")).encode(&mut out),
                    },
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(Msg::Freeze { id, frozen: false }) if checkpoint.as_ref().is_some_and(|c| procs.get(id)
                    .is_some_and(|p| p.run.writable_volumes.iter().any(|name| c.uses_volume(name)))) => {
                    Msg::Error("volume checkpoint in progress").encode(&mut out);
                }
                Ok(Msg::Freeze { id, frozen }) => match running_cgroup(&procs, id) {
                    Some((_, path)) => match super::cgroup::freeze(path, frozen) {
                        Ok(()) => Msg::Ack.encode(&mut out),
                        Err(e) => Msg::Error(&format!("freeze: {e}")).encode(&mut out),
                    },
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(Msg::GuestPlatformQuery) => super::checkpoint::platform(&mut out, runtime),
                Ok(Msg::CheckpointList { id }) => match super::checkpoint::list(&paths.containers, id) {
                    Ok(names) => asagiri_proto::encode_checkpoints(&mut out, &names),
                    Err(e) => Msg::Error(&format!("list checkpoints: {e}")).encode(&mut out),
                },
                Ok(Msg::CheckpointCreate { id, name, leave_running, volume_mounts }) => {
                    let result = if !leave_running {
                        Err(io::Error::other("only live checkpoint capture is implemented"))
                    } else if checkpoint.is_some() {
                        Err(io::Error::new(io::ErrorKind::WouldBlock, "a checkpoint capture is already in progress"))
                    } else if let Some(runtime) = runtime {
                        match running_cgroup(&procs, id) {
                            Some((pid, group)) => super::checkpoint::Capture::start(runtime, pid, group, &paths.containers.join(id), name, volume_mounts,
                                |volume| copy_jobs.writes_volume(volume) || other_volume_writer(&procs, pid, volume)),
                            None => Err(io::Error::other(format!("container {id} is not running"))),
                        }
                    } else {
                        Err(io::Error::other("checkpoint runtime is unavailable"))
                    };
                    match result {
                        Ok(capture) => {
                            checkpoint = Some(capture);
                            Msg::Ack.encode(&mut out);
                        }
                        Err(e) => Msg::Error(&format!("checkpoint: {e}")).encode(&mut out),
                    }
                }
                Ok(Msg::StatsQuery { id }) => match running_cgroup(&procs, id) {
                    Some((pid, path)) => match super::cgroup::stats(path, pid) {
                        Ok(s) => Msg::ContainerStats(s).encode(&mut out),
                        Err(e) => Msg::Error(&format!("stats: {e}")).encode(&mut out),
                    },
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(Msg::TopQuery { id }) => match running_cgroup(&procs, id) {
                    Some((_, path)) => {
                        let lines = super::cgroup::top(path);
                        asagiri_proto::encode_top(&mut out, &lines);
                    }
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(Msg::UsbNodesQuery) => asagiri_proto::encode_usb_nodes(&mut out, &super::usb::nodes()),
                Ok(Msg::ListenQuery { id }) => match running_pid(&procs, id) {
                    Some(pid) => asagiri_proto::encode_listening(&mut out, &net::listening(pid)),
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(Msg::NetAttach { id, ifname, net: n }) => match running_pid(&procs, id) {
                    Some(pid) => {
                        let num: u32 = ifname.strip_prefix("eth").and_then(|v| v.parse().ok()).unwrap_or(1);
                        let host_if = net::extra_veth(id, num);
                        // A previous run's veth can outlive its namespace for a moment;
                        // the name is this container's own, so it can go.
                        let _ = net::detach(&host_if);
                        let mac = asagiri_proto::net_opts::values(n.opts, asagiri_proto::net_opts::MAC).next().and_then(net::parse_mac);
                        let v6 = net::Ip6::from_opts(n.opts);
                        let plug = net::Plug::from_opts(n.opts);
                        // A container that started on the default bridge takes up
                        // the embedded DNS with its first user network.
                        let late = procs.get(id).and_then(|p| p.run.late_dns.as_ref());
                        let owner = late.map_or(n.ip, |l| l.ip);
                        // A macvlan or ipvlan network has no gateway address to answer on.
                        let added_route = net::WayOut::of(ifname, n.gateway, v6, plug);
                        let attached = net::attach(&mut bridges, n.bridge, &host_if, pid, num, n.ip, n.prefix, n.gateway, mac, v6, plug).and_then(|link| {
                            let served = if plug.no_ipv4 || plug.own_link() { Ok(()) } else { dns.serve(n.bridge, n.gateway) }
                                .and_then(|()| std::fs::File::open(format!("/proc/{pid}/ns/net")).and_then(|ns| dns.serve_in(&ns, pid, owner, n.bridge)));
                            let served = served.and_then(|()| match (&added_route, procs.get(id)) {
                                (Some(way), Some(p)) => net::prioritize_attached_route(pid, &p.run.ways_out, way),
                                _ => Ok(()),
                            });
                            // Nothing outside the container names such a
                            // link for the next try to clear away.
                            if let (Err(_), Some(m)) = (&served, &link) {
                                let _ = net::unplug(m);
                            }
                            served.map(|()| link)
                        });
                        match attached {
                            Ok(link) => {
                                dns.describe_network(pid, n);
                                if !plug.no_ipv4 {
                                    dns.register(n.bridge, n.dns_names.iter(), n.ip);
                                }
                                if let Some(v) = v6 {
                                    dns.register6(n.bridge, n.dns_names.iter(), n.ip, v.ip);
                                }
                                if let Some(p) = procs.get_mut(id) {
                                    if let Some(late) = p.run.late_dns.take() {
                                        // The network is joined either way: a file
                                        // that cannot be written is not its failure.
                                        if let Err(e) = late.switch() {
                                            eprintln!("asagiri-init: resolv.conf of {id} not switched to the embedded DNS: {e}");
                                        }
                                        dns.set_upstream(late.ip, late.servers);
                                        // Dropped with the container, as its names are.
                                        if !p.run.dns_entries.iter().any(|(b, a)| *b == late.bridge && *a == late.ip) {
                                            p.run.dns_entries.push((late.bridge, late.ip));
                                        }
                                    }
                                    p.run.dns_entries.push((n.bridge.to_owned(), n.ip));
                                    match link {
                                        Some(m) => p.run.macvlans.push(m),
                                        None => p.run.veths.push(host_if.clone()),
                                    }
                                    // The interface is new: nothing kept under its name holds.
                                    p.run.ways_out.retain(|w| w.ifname != ifname);
                                    p.run.ways_out.extend(added_route);
                                }
                                Msg::Ack.encode(&mut out)
                            }
                            Err(e) => {
                                // The daemon takes the network as not joined: no
                                // interface, and no route through it, stays behind.
                                let _ = net::detach(&host_if);
                                if added_route.is_some() {
                                    if let Some(p) = procs.get(id) {
                                        let _ = net::restore_default_routes(pid, &p.run.ways_out);
                                    }
                                }
                                Msg::Error(&format!("connect: {e}")).encode(&mut out)
                            }
                        }
                    }
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(Msg::UplinkQuery) => match dhcp.as_ref().and_then(|d| d.lease.as_ref()) {
                    Some(l) => Msg::Uplink { ip: l.ip, gateway: l.gateway }.encode(&mut out),
                    None => Msg::Error("no uplink lease").encode(&mut out),
                },
                Ok(Msg::SetUplink { ip, prefix, gateway, dns: dns_ip }) => match dhcp.as_mut() {
                    _ if host_process => Msg::Error("set uplink is only for the VM").encode(&mut out),
                    Some(d) => match d.apply_static(ip, prefix, gateway, dns_ip) {
                        Ok(false) => {
                            let lease = d.lease.as_ref().expect("existing uplink lease");
                            Msg::Uplink { ip: lease.ip, gateway: lease.gateway }.encode(&mut out);
                        }
                        Ok(true) => {
                            eprintln!("asagiri-init: DHCP did not answer; configured {} statically as {}/{} via {} from host", net::UPLINK, net::ip_str(ip), prefix, net::ip_str(gateway));
                            Msg::Ack.encode(&mut out);
                        }
                        Err(e) => Msg::Error(&format!("set uplink: {e}")).encode(&mut out),
                    },
                    None => Msg::Error("no uplink interface").encode(&mut out),
                },
                Ok(Msg::Route { dst, prefix, via, add }) => match net::route(dst, prefix, via, add) {
                    Ok(()) => Msg::Ack.encode(&mut out),
                    Err(e) => Msg::Error(&format!("route: {e}")).encode(&mut out),
                },
                Ok(Msg::DnsRename { id, bridge, ip, names }) => {
                    if procs.get(id).is_some_and(|p| p.run.dns_entries.iter().any(|(b, a)| b == bridge && *a == ip)) {
                        dns.rename(bridge, ip, names);
                    }
                    Msg::Ack.encode(&mut out);
                }
                Ok(Msg::NetDetach { id, ifname, bridge, ip }) => match running_pid(&procs, id) {
                    Some(pid) => {
                        let run = &mut procs.get_mut(id).expect("running process").run;
                        // The initial veth has a per-start name (kv...), while
                        // interfaces added after launch use kx<id><number>.
                        let host_if = if ifname == "eth0" {
                            run.veths.iter().find(|name| name.starts_with("kv")).cloned()
                        } else {
                            ifname.strip_prefix("eth").and_then(|n| n.parse().ok()).map(|n| net::extra_veth(id, n))
                        };
                        let macvlan = run.macvlans.iter().position(|m| m.ifname == ifname);
                        let detached = match (macvlan, host_if.as_deref()) {
                            (Some(i), _) => net::unplug(&run.macvlans[i]),
                            (_, Some(name)) => net::detach(name),
                            _ => Err(std::io::Error::other(format!("unknown interface {ifname}"))),
                        };
                        match detached {
                            Ok(()) => {
                                dns.unregister(bridge, ip);
                                dns.leave(pid, bridge);
                                run.dns_entries.retain(|(b, a)| !(b == bridge && *a == ip));
                                if let Some(i) = macvlan {
                                    run.macvlans.remove(i);
                                } else if let Some(name) = host_if {
                                    run.veths.retain(|v| *v != name);
                                }
                                run.ways_out.retain(|w| w.ifname != ifname);
                                let _ = net::restore_default_routes(pid, &run.ways_out);
                                Msg::Ack.encode(&mut out)
                            }
                            Err(e) => Msg::Error(&format!("disconnect: {e}")).encode(&mut out),
                        }
                    }
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(Msg::FsStatRequest { id, path }) => match running_rootfs(&procs, id) {
                    Some(root) => match super::fsview::stat(&root, path) {
                        Ok(meta) => {
                            let name = path.rsplit('/').next().unwrap_or("");
                            Msg::FsStat(super::fsview::entry(name, meta)).encode(&mut out)
                        }
                        Err(e) => Msg::Error(&format!("stat {path}: {e}")).encode(&mut out),
                    },
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(Msg::FsListRequest { id, path }) => match running_rootfs(&procs, id) {
                    Some(root) => match super::fsview::list(&root, path) {
                        Ok(list) => {
                            let entries: Vec<_> = list.iter().map(|(n, m)| super::fsview::entry(n, *m)).collect();
                            asagiri_proto::encode_fs_list(&mut out, &entries)
                        }
                        Err(e) => Msg::Error(&format!("list {path}: {e}")).encode(&mut out),
                    },
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(Msg::FsRead { id, path, offset, len }) => match running_rootfs(&procs, id) {
                    Some(root) => match super::fsview::read(&root, path, offset, len as usize, &mut fs_buf) {
                        Ok(()) => Msg::FsData(&fs_buf).encode(&mut out),
                        Err(e) => Msg::Error(&format!("read {path}: {e}")).encode(&mut out),
                    },
                    None => Msg::Error(&format!("container {id} is not running")).encode(&mut out),
                },
                Ok(other) => Msg::Error(&format!("unexpected {other:?}")).encode(&mut out),
                Err(e) => Msg::Error(&format!("bad frame: {e:?}")).encode(&mut out),
            }
            conn.write_all(&out)?;
        }
    }
}

/// A PSI trigger: readable (POLLPRI) when tasks stall on memory for 50 ms
/// within any 500 ms window. None if the kernel has no PSI.
/// Runs the DHCP client alone until it has a lease or the startup deadline passes.
fn wait_for_lease(dhcp: &mut Option<Dhcp>, deadline: std::time::Instant) {
    let Some(d) = dhcp.as_mut() else { return };
    while d.lease.is_none() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            eprintln!("asagiri-init: no DHCP lease yet; starting the container without waiting longer");
            return;
        }
        let mut pfd = libc::pollfd { fd: d.fd(), events: libc::POLLIN, revents: 0 };
        let wait = (d.timeout_ms().max(0) as u128).min(left.as_millis()) as libc::c_int;
        // SAFETY: one valid pollfd.
        unsafe { libc::poll(&mut pfd, 1, wait.max(1)) };
        if pfd.revents != 0 {
            if let Ok(true) = d.on_readable()
                && let Some(l) = &d.lease
            {
                eprintln!("asagiri-init: {} is {}/{} via {}", net::UPLINK, net::ip_str(l.ip), l.prefix, net::ip_str(l.gateway));
            }
        }
        let _ = d.on_timer();
    }
}

fn pressure_reading() -> Msg<'static> {
    let (_, mem_available_kib) = meminfo();
    let (mem_stall, _) = super::cgroup::memory_trouble();
    Msg::MemoryPressure(MemoryPressure { mem_available_kib, mem_stall })
}

fn memory_pressure_trigger() -> Option<std::fs::File> {
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open("/proc/pressure/memory").ok()?;
    f.write_all(b"some 50000 500000\0").ok()?;
    Some(f)
}

/// A running container's init (not an exec).
fn running_cgroup<'a>(procs: &'a Procs, id: &str) -> Option<(libc::pid_t, &'a std::path::Path)> {
    let pid = running_pid(procs, id)?;
    Some((pid, container::cgroup(&procs.get(id)?.run)?))
}

fn other_volume_writer(procs: &Procs, target: libc::pid_t, volume: &str) -> bool {
    procs.values().any(|p| {
        if p.run.pid == target || !p.run.writable_volumes.iter().any(|v| v == volume) { return false; }
        let Some(group) = container::cgroup(&p.run) else { return false };
        let Ok(events) = std::fs::read_to_string(group.join("cgroup.events")) else {
            return p.exited.is_none() || group.exists();
        };
        events.lines().any(|line| line == "populated 1") && !events.lines().any(|line| line == "frozen 1")
    })
}

/// A reaped entry may retain its terminal while output and cleanup drain.
fn resize(procs: &Procs, id: &str, rows: u16, cols: u16) {
    if let Some(fd) = procs.values().find(|p| p.run.id == id && p.exited.is_none() && p.run.tty).and_then(|p| p.run.stdout.as_ref()) {
        sys::resize_pty(fd.as_raw_fd(), rows, cols);
    }
}

fn running_pid(procs: &Procs, id: &str) -> Option<libc::pid_t> {
    procs.get(id).filter(|p| p.run.container.is_none() && p.exited.is_none()).map(|p| p.run.pid)
}

/// The host side of a container's `eth{n}` for n > 0: 15 characters at most.
/// The init's mounted view, including volumes and binds in its private mount
/// namespace. fsview still resolves every requested path inside this root.
/// Reaped entries remain unavailable while their asynchronous cleanup runs.
fn running_rootfs(procs: &Procs, id: &str) -> Option<std::path::PathBuf> {
    running_pid(procs, id).map(|pid| std::path::PathBuf::from(format!("/proc/{pid}/root")))
}

#[cfg(test)]
#[path = "agent_files_tests.rs"]
mod files_view_tests;

fn new_proc(run: Process, received_ns: u64, cloned_ns: u64) -> Proc {
    Proc { run, received_ns, cloned_ns, exited: None, input: VecDeque::new(), close_stdin_when_drained: false, tty_hup: None, cleanup: None, cleanup_finished: false, oom_killed: None, exit_announced: false, root_released: false }
}

fn copy_view(procs: &Procs, id: &str) -> super::copy::View {
    match procs.get(id).filter(|p| p.run.container.is_none()) {
        Some(p) if p.exited.is_none() => super::copy::View::Running(p.run.pid),
        Some(_) => super::copy::View::Cleaning,
        None => super::copy::View::Stopped,
    }
}

fn root_events(roots: &Roots, procs: &Procs, conn: &mut File, out: &mut Vec<u8>) -> io::Result<()> {
    out.clear();
    for (id, _) in roots.events() {
        // Reconcile both owners of the same per-ID barrier. A root worker
        // finishing cannot clear a process cleanup that is still pending.
        let pending = roots.cleaning(&id)
            || procs.get(&id).is_some_and(|p| p.exited.is_some() && container::has_filesystem_teardown(&p.run));
        if pending { Msg::CleanupPending(&id).encode(out); }
        else { Msg::CleanupDone(&id).encode(out); }
    }
    if !out.is_empty() { conn.write_all(out)?; }
    Ok(())
}

/// Writes as much pending input as the process will take without blocking,
/// and tells the host how much, so it can send that much more.
fn flush_input(procs: &mut Procs, fd: RawFd, conn: &mut File, out: &mut Vec<u8>) -> io::Result<()> {
    let Some(p) = procs.values_mut().find(|p| p.run.stdin.as_ref().is_some_and(|f| f.as_raw_fd() == fd)) else { return Ok(()) };
    let mut consumed = 0usize;
    while !p.input.is_empty() {
        let (front, _) = p.input.as_slices();
        // SAFETY: writing from a buffer we own to a non-blocking descriptor we own.
        let n = unsafe { libc::write(fd, front.as_ptr().cast(), front.len()) };
        if n > 0 {
            p.input.drain(..n as usize);
            consumed += n as usize;
        } else {
            if io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN) {
                // The reader is gone.
                p.input.clear();
                p.run.stdin = None;
            }
            break;
        }
    }
    if p.input.is_empty() && p.close_stdin_when_drained {
        p.run.stdin = None;
    }
    if consumed > 0 {
        out.clear();
        Msg::InputConsumed { id: &p.run.id, bytes: consumed as u32 }.encode(out);
        conn.write_all(out)?;
    }
    Ok(())
}

/// This process's resident memory, from /proc/self/statm (in pages).
/// What Asagiri's Linux holds, in KiB. In the VM, where this process is init,
/// it is the root cgroup: the whole guest. On a Linux host it is the
/// containers' cgroup and this process; the daemon adds itself.
fn memory_sample(paths: &Paths, agent_rss_kib: u64) -> super::cgroup::Memory {
    if std::process::id() == 1 {
        return super::cgroup::memory(std::path::Path::new("/sys/fs/cgroup")).unwrap_or_default();
    }
    match super::cgroup::memory(&paths.cgroup) {
        Some(mut memory) => {
            memory.held = memory.held.saturating_add(agent_rss_kib.saturating_mul(1024));
            memory.working_set = memory.working_set.saturating_add(agent_rss_kib.saturating_mul(1024));
            memory
        }
        None => super::cgroup::Memory::default(),
    }
}

fn self_rss_kib() -> u64 {
    let mut buf = [0u8; 128];
    let n = std::fs::File::open("/proc/self/statm").and_then(|mut f| f.read(&mut buf)).unwrap_or(0);
    let pages: u64 = std::str::from_utf8(&buf[..n]).ok().and_then(|s| s.split_whitespace().nth(1)?.parse().ok()).unwrap_or(0);
    // SAFETY: sysconf takes no pointers.
    pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64 / 1024
}

/// Whether the guest's clock is more than a quarter second from `ns`
/// (Unix nanoseconds, the host's time).
fn clock_off(ns: u64) -> bool {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    off_by_more(now, ns, 250_000_000)
}

fn off_by_more(a: u64, b: u64, limit: u64) -> bool {
    a.abs_diff(b) > limit
}

/// Reads one frame into `buf`. Returns false on a clean end of stream.
fn read_frame(conn: &mut File, buf: &mut Vec<u8>) -> io::Result<bool> {
    buf.resize(4, 0);
    if let Err(e) = conn.read_exact(&mut buf[..4]) {
        return if e.kind() == io::ErrorKind::UnexpectedEof { Ok(false) } else { Err(e) };
    }
    let len = asagiri_proto::frame_len(buf[..4].try_into().expect("4 bytes"))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))?;
    buf.resize(len, 0);
    conn.read_exact(&mut buf[4..])?;
    Ok(true)
}

fn owner(procs: &mut Procs, fd: RawFd) -> Option<(&mut Proc, Stream)> {
    procs.values_mut().find_map(|p| {
        let stream = if p.run.stdout.as_ref().is_some_and(|f| f.as_raw_fd() == fd) {
            Stream::Stdout
        } else if p.run.stderr.as_ref().is_some_and(|f| f.as_raw_fd() == fd) {
            Stream::Stderr
        } else {
            return None;
        };
        Some((p, stream))
    })
}

/// Blocks SIGCHLD and returns a signalfd that becomes readable when a child exits.
fn child_signals() -> io::Result<OwnedFd> {
    // SAFETY: sigset manipulation on a local set, then plain syscalls.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGCHLD);
        sys::check(libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) as _, "sigprocmask")?;
        let fd = sys::check(libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) as _, "signalfd")?;
        Ok(OwnedFd::from_raw_fd(fd as RawFd))
    }
}

/// Linux host: kill and reap running processes, then join filesystem workers
/// before tearing down containers. Retry admission is bounded; joining an
/// active filesystem worker still waits for its I/O to finish.
fn stop_containers(procs: &mut Procs, dns: &mut super::dns::Dns, roots: &Roots, copies: &mut super::copy::Broker<'_>, volumes: &mut super::volumes::Volumes, paths: &Paths) {
    copies.shutdown();
    for p in procs.values().filter(|p| p.exited.is_none()) {
        // SAFETY: signalling a child we started and have not reaped.
        unsafe { libc::kill(p.run.pid, libc::SIGKILL) };
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        reap(procs, dns, roots, copies, volumes, None);
        if procs.values().all(|p| p.exited.is_some()) || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // A worker shares these paths/mounts. Join it before shutdown touches them.
    let removal_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        reap(procs, dns, roots, copies, volumes, None);
        let removing = roots.poll_shutdown(paths, std::time::Instant::now(), removal_deadline);
        if !removing && procs.values().all(|p| p.cleanup.is_none()) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    for id in roots.unfinished() {
        eprintln!("asagiri-init: remove {id}: unfinished at shutdown; container files remain");
    }
    for p in procs.values().filter(|p| !p.cleanup_finished) {
        container::teardown(&p.run, false);
    }
    procs.clear();
}

/// The read end of a pipe written to on SIGTERM and SIGINT. A handler, not
/// a blocked signal read from a signalfd: a blocked mask would be inherited
/// by every container process, and a caught signal is reset by exec.
fn term_signals() -> io::Result<OwnedFd> {
    let mut fds = [0; 2];
    // SAFETY: plain pipe2 and sigaction; the handler only calls write(2),
    // which is async-signal-safe, on a descriptor that is never closed.
    unsafe {
        sys::check(libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) as _, "pipe")?;
        TERM_PIPE.store(fds[1], std::sync::atomic::Ordering::SeqCst);
        AGENT_PID.store(libc::getpid(), std::sync::atomic::Ordering::SeqCst);
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_term as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        for sig in [libc::SIGTERM, libc::SIGINT] {
            sys::check(libc::sigaction(sig, &sa, std::ptr::null_mut()) as _, "sigaction")?;
        }
        Ok(OwnedFd::from_raw_fd(fds[0]))
    }
}

static TERM_PIPE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
static AGENT_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn on_term(_: libc::c_int) {
    // A container's process, between its clone and its exec, still has
    // this handler (and the pipe): a signal meant for it must not stop the
    // agent.
    // SAFETY: getpid and write(2) are async-signal-safe.
    unsafe {
        if libc::getpid() == AGENT_PID.load(std::sync::atomic::Ordering::SeqCst) {
            libc::write(TERM_PIPE.load(std::sync::atomic::Ordering::SeqCst), b"t".as_ptr().cast(), 1);
        }
    }
}

fn drain_signals(fd: &OwnedFd) {
    let mut info = [0u8; 128 * 8];
    // SAFETY: reading into a local buffer from a non-blocking signalfd.
    while unsafe { libc::read(fd.as_raw_fd(), info.as_mut_ptr().cast(), info.len()) } > 0 {}
}

fn reap(procs: &mut Procs, dns: &mut super::dns::Dns, roots: &Roots, copies: &mut super::copy::Broker<'_>, volumes: &mut super::volumes::Volumes, mut checkpoint: Option<&mut super::checkpoint::Capture>) {
    loop {
        let mut status = 0;
        let candidate = checkpoint.as_deref_mut().map_or(-1, |capture| capture.next_reapable_pid());
        // While capturing, a positive PID has been observed without being
        // reaped. Zero means no child is ready for the general reaper.
        // SAFETY: plain waitpid on any or one observed child.
        let pid = if candidate == 0 { 0 } else { unsafe { libc::waitpid(candidate, &mut status, libc::WNOHANG) } };
        if pid <= 0 {
            for p in procs.values_mut().filter(|p| p.exited.is_some() && !p.root_released) {
                container::release_root(&p.run, roots);
                p.root_released = true;
            }
            return;
        }
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else if libc::WIFSIGNALED(status) {
            128 + libc::WTERMSIG(status)
        } else {
            continue;
        };
        if !copies.reaped(pid, code) && !roots.reaped(pid, code, std::time::Instant::now())
            && !volumes.reaped(pid, code, std::time::Instant::now()) && record_exit(procs, pid, code) {
            // Drop the PID-owned resolver before another spawn can reuse this
            // PID. Output and filesystem cleanup retain the stable ID only.
            dns.drop_container(pid);
        }
    }
}

fn record_exit(procs: &mut Procs, pid: libc::pid_t, code: i32) -> bool {
    if let Some(p) = procs.values_mut().find(|p| p.cleanup == Some(pid)) {
        p.cleanup = None;
        p.cleanup_finished = code == 0;
        if code != 0 {
            eprintln!("asagiri-init: cleanup {} exited {code}; retrying", p.run.id);
        }
    } else if let Some(p) = procs.values_mut().find(|p| p.run.pid == pid && p.exited.is_none()) {
        p.exited = Some((code, sys::boottime_ns()));
        p.input.clear();
        p.run.stdin = None;
        return p.run.container.is_none();
    }
    false
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;

    #[test]
    fn resize_ignores_reaped_processes_and_preserves_live_terminals() {
        fn size(fd: &OwnedFd) -> (u16, u16) {
            let mut value = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
            // SAFETY: a valid output structure and a PTY descriptor owned by this test.
            assert_eq!(unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ as libc::Ioctl, &mut value) }, 0);
            (value.ws_row, value.ws_col)
        }
        let (old_master, old_slave) = sys::open_pty().unwrap();
        let (live_master, live_slave) = sys::open_pty().unwrap();
        sys::resize_pty(old_master.as_raw_fd(), 25, 81);
        sys::resize_pty(live_master.as_raw_fd(), 26, 82);
        let mut old = Process::test_process("old", 42);
        old.tty = true;
        old.stdout = Some(old_master);
        let mut live = Process::test_process("live", 42);
        live.tty = true;
        live.stdout = Some(live_master);
        let mut procs = Procs::new();
        procs.insert("old".into(), new_proc(old, 0, 0));
        assert!(record_exit(&mut procs, 42, 0));
        // Stable IDs retain the old output descriptor even when a new entry
        // has the same numeric PID. Neither entry owns an actual PID here.
        procs.insert("live".into(), new_proc(live, 0, 0));
        resize(&procs, "old", 40, 100);
        assert_eq!(size(&old_slave), (25, 81), "resized an exited process's retained terminal");
        assert_eq!(size(&live_slave), (26, 82));
        resize(&procs, "live", 44, 120);
        assert_eq!(size(&live_slave), (44, 120));
        assert_eq!(size(&old_slave), (25, 81));
        resize(&procs, "missing", 50, 140);
        assert_eq!(size(&live_slave), (44, 120));
        procs.get_mut("live").unwrap().run.tty = false;
        resize(&procs, "live", 50, 140);
        assert_eq!(size(&live_slave), (44, 120), "resized a non-TTY process");
        procs.get_mut("live").unwrap().run.tty = true;
        let _held_master = procs.get_mut("live").unwrap().run.stdout.take().unwrap();
        resize(&procs, "live", 50, 140);
        assert_eq!(size(&live_slave), (44, 120));
        assert!(procs["old"].exited.is_some() && procs["live"].exited.is_none());
        eprintln!("resize PTY checks: exited unchanged, live changed, numeric PID alias isolated, missing/non-TTY/no-output unchanged");
    }

    #[test]
    fn reused_pid_cannot_replace_cleanup_or_receive_its_exit() {
        let mut procs = Procs::new();
        let old = new_proc(Process::test_process("old", 42), 0, 0);
        procs.insert("old".into(), old);
        assert!(record_exit(&mut procs, 42, 0));
        procs.get_mut("old").unwrap().cleanup = Some(43);
        procs.insert("new".into(), new_proc(Process::test_process("new", 42), 0, 0));
        assert_eq!(running_pid(&procs, "new"), Some(42));
        assert_eq!(running_pid(&procs, "old"), None);
        assert!(!record_exit(&mut procs, 43, 0));
        assert!(procs["old"].cleanup_finished);
        assert!(procs["new"].exited.is_none());
        assert!(record_exit(&mut procs, 42, 7));
        assert_eq!(procs["new"].exited.unwrap().0, 7);
        assert_eq!(procs["old"].exited.unwrap().0, 0);
    }
}

#[cfg(test)]
mod clock_tests {
    use super::off_by_more;

    #[test]
    fn steps_only_when_the_clock_is_off() {
        let now = 1_792_800_000_000_000_000;
        assert!(!off_by_more(now, now, 250_000_000));
        assert!(!off_by_more(now, now + 200_000_000, 250_000_000));
        assert!(!off_by_more(now + 200_000_000, now, 250_000_000));
        // An hour behind, after the Mac slept.
        assert!(off_by_more(now - 3_600_000_000_000, now, 250_000_000));
        assert!(off_by_more(now + 300_000_000, now, 250_000_000));
    }
}

/// A non-blocking pipe: (read end the loop polls, write end the thread rings).
fn usage_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors pipe2 writes.
    sys::check(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } as _, "pipe")?;
    // SAFETY: pipe2 just opened both; nothing else owns them.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}
