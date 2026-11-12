//! Syscall names to numbers for this machine's architecture, as seccomp
//! profiles spell them. The numbers are libc's `SYS_*` constants.

/// Pairs each constant with its own name, `SYS_` included.
macro_rules! numbers {
    ($($name:ident)*) => { &[$((stringify!($name), libc::$name as u32)),*] };
}

/// On both architectures.
const SHARED: &[(&str, u32)] = numbers!(
    SYS_io_setup SYS_io_destroy SYS_io_submit SYS_io_cancel SYS_io_getevents SYS_setxattr SYS_lsetxattr SYS_fsetxattr SYS_getxattr
    SYS_lgetxattr SYS_fgetxattr SYS_listxattr SYS_llistxattr SYS_flistxattr SYS_removexattr SYS_lremovexattr SYS_fremovexattr
    SYS_getcwd SYS_lookup_dcookie SYS_eventfd2 SYS_epoll_create1 SYS_epoll_ctl SYS_epoll_pwait SYS_dup SYS_dup3 SYS_fcntl
    SYS_inotify_init1 SYS_inotify_add_watch SYS_inotify_rm_watch SYS_ioctl SYS_ioprio_set SYS_ioprio_get SYS_flock SYS_mknodat
    SYS_mkdirat SYS_unlinkat SYS_symlinkat SYS_linkat SYS_renameat SYS_umount2 SYS_mount SYS_pivot_root SYS_nfsservctl SYS_statfs
    SYS_fstatfs SYS_truncate SYS_ftruncate SYS_fallocate SYS_faccessat SYS_chdir SYS_fchdir SYS_chroot SYS_fchmod SYS_fchmodat
    SYS_fchownat SYS_fchown SYS_openat SYS_close SYS_vhangup SYS_pipe2 SYS_quotactl SYS_getdents64 SYS_lseek SYS_read SYS_write
    SYS_readv SYS_writev SYS_pread64 SYS_pwrite64 SYS_preadv SYS_pwritev SYS_sendfile SYS_pselect6 SYS_ppoll SYS_signalfd4
    SYS_vmsplice SYS_splice SYS_tee SYS_readlinkat SYS_newfstatat SYS_fstat SYS_sync SYS_fsync SYS_fdatasync SYS_sync_file_range
    SYS_timerfd_create SYS_timerfd_settime SYS_timerfd_gettime SYS_utimensat SYS_acct SYS_capget SYS_capset SYS_personality SYS_exit
    SYS_exit_group SYS_waitid SYS_set_tid_address SYS_unshare SYS_futex SYS_set_robust_list SYS_get_robust_list SYS_nanosleep
    SYS_getitimer SYS_setitimer SYS_kexec_load SYS_init_module SYS_delete_module SYS_timer_create SYS_timer_gettime
    SYS_timer_getoverrun SYS_timer_settime SYS_timer_delete SYS_clock_settime SYS_clock_gettime SYS_clock_getres SYS_clock_nanosleep
    SYS_syslog SYS_ptrace SYS_sched_setparam SYS_sched_setscheduler SYS_sched_getscheduler SYS_sched_getparam SYS_sched_setaffinity
    SYS_sched_getaffinity SYS_sched_yield SYS_sched_get_priority_max SYS_sched_get_priority_min SYS_sched_rr_get_interval
    SYS_restart_syscall SYS_kill SYS_tkill SYS_tgkill SYS_sigaltstack SYS_rt_sigsuspend SYS_rt_sigaction SYS_rt_sigprocmask
    SYS_rt_sigpending SYS_rt_sigtimedwait SYS_rt_sigqueueinfo SYS_rt_sigreturn SYS_setpriority SYS_getpriority SYS_reboot
    SYS_setregid SYS_setgid SYS_setreuid SYS_setuid SYS_setresuid SYS_getresuid SYS_setresgid SYS_getresgid SYS_setfsuid
    SYS_setfsgid SYS_times SYS_setpgid SYS_getpgid SYS_getsid SYS_setsid SYS_getgroups SYS_setgroups SYS_uname SYS_sethostname
    SYS_setdomainname SYS_getrlimit SYS_setrlimit SYS_getrusage SYS_umask SYS_prctl SYS_getcpu SYS_gettimeofday SYS_settimeofday
    SYS_adjtimex SYS_getpid SYS_getppid SYS_getuid SYS_geteuid SYS_getgid SYS_getegid SYS_gettid SYS_sysinfo SYS_mq_open
    SYS_mq_unlink SYS_mq_timedsend SYS_mq_timedreceive SYS_mq_notify SYS_mq_getsetattr SYS_msgget SYS_msgctl SYS_msgrcv SYS_msgsnd
    SYS_semget SYS_semctl SYS_semtimedop SYS_semop SYS_shmget SYS_shmctl SYS_shmat SYS_shmdt SYS_socket SYS_socketpair SYS_bind
    SYS_listen SYS_accept SYS_connect SYS_getsockname SYS_getpeername SYS_sendto SYS_recvfrom SYS_setsockopt SYS_getsockopt
    SYS_shutdown SYS_sendmsg SYS_recvmsg SYS_readahead SYS_brk SYS_munmap SYS_mremap SYS_add_key SYS_request_key SYS_keyctl
    SYS_clone SYS_execve SYS_mmap SYS_fadvise64 SYS_swapon SYS_swapoff SYS_mprotect SYS_msync SYS_mlock SYS_munlock SYS_mlockall
    SYS_munlockall SYS_mincore SYS_madvise SYS_remap_file_pages SYS_mbind SYS_get_mempolicy SYS_set_mempolicy SYS_migrate_pages
    SYS_move_pages SYS_rt_tgsigqueueinfo SYS_perf_event_open SYS_accept4 SYS_recvmmsg SYS_wait4 SYS_prlimit64 SYS_fanotify_init
    SYS_fanotify_mark SYS_name_to_handle_at SYS_open_by_handle_at SYS_clock_adjtime SYS_syncfs SYS_setns SYS_sendmmsg
    SYS_process_vm_readv SYS_process_vm_writev SYS_kcmp SYS_finit_module SYS_sched_setattr SYS_sched_getattr SYS_renameat2
    SYS_seccomp SYS_getrandom SYS_memfd_create SYS_bpf SYS_execveat SYS_userfaultfd SYS_membarrier SYS_mlock2 SYS_copy_file_range
    SYS_preadv2 SYS_pwritev2 SYS_pkey_mprotect SYS_pkey_alloc SYS_pkey_free SYS_statx SYS_rseq SYS_pidfd_send_signal
    SYS_io_uring_setup SYS_io_uring_enter SYS_io_uring_register SYS_open_tree SYS_move_mount SYS_fsopen SYS_fsconfig SYS_fsmount
    SYS_fspick SYS_pidfd_open SYS_clone3 SYS_close_range SYS_openat2 SYS_pidfd_getfd SYS_faccessat2 SYS_process_madvise
    SYS_epoll_pwait2 SYS_mount_setattr SYS_quotactl_fd SYS_landlock_create_ruleset SYS_landlock_add_rule SYS_landlock_restrict_self
    SYS_memfd_secret SYS_process_mrelease SYS_futex_waitv SYS_set_mempolicy_home_node SYS_mseal
);

/// What only x86-64 has: the calls `openat` and its kind replaced, and its own.
#[cfg(target_arch = "x86_64")]
const OWN: &[(&str, u32)] = numbers!(
    SYS_open SYS_stat SYS_lstat SYS_poll SYS_access SYS_pipe SYS_select SYS_dup2 SYS_pause SYS_alarm SYS_fork SYS_vfork SYS_getdents
    SYS_rename SYS_mkdir SYS_rmdir SYS_creat SYS_link SYS_unlink SYS_symlink SYS_readlink SYS_chmod SYS_chown SYS_lchown SYS_getpgrp
    SYS_utime SYS_mknod SYS_uselib SYS_ustat SYS_sysfs SYS_modify_ldt SYS__sysctl SYS_arch_prctl SYS_iopl SYS_ioperm SYS_getpmsg
    SYS_putpmsg SYS_afs_syscall SYS_tuxcall SYS_security SYS_time SYS_set_thread_area SYS_get_thread_area SYS_epoll_create
    SYS_epoll_ctl_old SYS_epoll_wait_old SYS_epoll_wait SYS_utimes SYS_vserver SYS_inotify_init SYS_futimesat SYS_signalfd
    SYS_eventfd SYS_kexec_file_load
);
#[cfg(target_arch = "aarch64")]
const OWN: &[(&str, u32)] = &[];

/// Syscalls that not every libc variant has a constant for yet. Since
/// Linux 5.1 a new syscall has one number on every architecture. A rule for
/// a name that is not listed is skipped, so new syscalls belong here.
const LATER: &[(&str, u32)] = &[
    #[cfg(target_arch = "aarch64")]
    ("io_pgetevents", 292),
    #[cfg(target_arch = "aarch64")]
    ("kexec_file_load", 294),
    #[cfg(target_arch = "x86_64")]
    ("create_module", 174),
    #[cfg(target_arch = "x86_64")]
    ("get_kernel_syms", 177),
    #[cfg(target_arch = "x86_64")]
    ("query_module", 178),
    #[cfg(target_arch = "x86_64")]
    ("io_pgetevents", 333),
    #[cfg(target_arch = "x86_64")]
    ("uretprobe", 335),
    #[cfg(target_arch = "x86_64")]
    ("uprobe", 336),
    ("cachestat", 451),
    ("fchmodat2", 452),
    ("map_shadow_stack", 453),
    ("futex_wake", 454),
    ("futex_wait", 455),
    ("futex_requeue", 456),
    ("statmount", 457),
    ("listmount", 458),
    ("lsm_get_self_attr", 459),
    ("lsm_set_self_attr", 460),
    ("lsm_list_modules", 461),
    ("setxattrat", 463),
    ("getxattrat", 464),
    ("listxattrat", 465),
    ("removexattrat", 466),
    ("open_tree_attr", 467),
    ("file_getattr", 468),
    ("file_setattr", 469),
];

/// The number of the syscall a profile calls `name`, if this architecture has it.
pub fn number(name: &str) -> Option<u32> {
    let named = SHARED.iter().chain(OWN).find(|(constant, _)| constant.strip_prefix("SYS_") == Some(name));
    named.or_else(|| LATER.iter().find(|(later, _)| *later == name)).map(|(_, number)| *number)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_resolve_to_this_architectures_numbers() {
        #[cfg(target_arch = "aarch64")]
        let known = [("read", 63), ("openat", 56), ("fchmodat", 53), ("io_pgetevents", 292), ("kexec_file_load", 294)];
        #[cfg(target_arch = "x86_64")]
        let known = [("read", 0), ("openat", 257), ("chmod", 90), ("io_pgetevents", 333), ("_sysctl", 156), ("kexec_file_load", 320), ("uretprobe", 335), ("uprobe", 336)];
        for (name, expected) in known.into_iter().chain([("clone3", 435), ("fchmodat2", 452), ("mseal", 462), ("file_setattr", 469)]) {
            assert_eq!(number(name), Some(expected), "{name}");
        }
        // Another architecture's syscall, a constant's full name, no name at all.
        for name in ["socketcall", "SYS_read", "sys_read", "", "no_such_syscall"] {
            assert_eq!(number(name), None, "{name}");
        }
        #[cfg(target_arch = "aarch64")]
        assert_eq!(number("open"), None);
    }

    #[test]
    fn every_name_has_one_number() {
        let all: Vec<(&str, u32)> = SHARED.iter().chain(OWN).map(|(name, number)| (&name[4..], *number)).chain(LATER.iter().copied()).collect();
        for (i, (name, number)) in all.iter().enumerate() {
            assert!(!all[..i].iter().any(|(other, _)| other == name), "{name} listed twice");
            assert!(!all[..i].iter().any(|(_, other)| other == number), "{name} shares number {number}");
        }
    }
}
