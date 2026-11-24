/* Kiri #254/#255: run only as PID 1 in an isolated, diskless QEMU guest.
 * Attach our initramfs file to a new loop device, mount ext4, and verify that
 * data survives detach/reattach. No host devices or directories are shared.
 */
#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <sys/auxv.h>
#include <fcntl.h>
#include <linux/loop.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>

static void finish(int success, const char *operation) {
    if (success)
        puts("ASAGIRI_LOOP_TEST_PASS: attach ext4 write detach reattach read");
    else
        printf("ASAGIRI_LOOP_TEST_FAIL: %s: %s\n", operation, strerror(errno));
    fflush(stdout);
    sync();
    reboot(RB_POWER_OFF);
    _exit(success ? 0 : 1);
}

#define CHECK(test, operation) do { if (!(test)) finish(0, operation); } while (0)

#ifdef __aarch64__
static void verify_vdso_hashes(void) {
    const unsigned long base = getauxval(AT_SYSINFO_EHDR);
    CHECK(base != 0, "locate arm64 vDSO");
    const Elf64_Ehdr *header = (const Elf64_Ehdr *)base;
    const Elf64_Phdr *segments = (const Elf64_Phdr *)(base + header->e_phoff);
    int sysv = 0, gnu = 0;
    for (unsigned i = 0; i < header->e_phnum; ++i) {
        if (segments[i].p_type != PT_DYNAMIC)
            continue;
        const Elf64_Dyn *entries = (const Elf64_Dyn *)(base + segments[i].p_vaddr);
        for (size_t j = 0; j < segments[i].p_memsz / sizeof(*entries) && entries[j].d_tag != DT_NULL; ++j) {
            sysv |= entries[j].d_tag == DT_HASH;
            gnu |= entries[j].d_tag == DT_GNU_HASH;
        }
    }
    if (!sysv || !gnu) {
        errno = ENOEXEC;
        finish(0, "arm64 vDSO requires DT_HASH and DT_GNU_HASH");
    }
    puts("arm64 vDSO has DT_HASH and DT_GNU_HASH");
}
#endif

static void verify_copy_up_birth_time(void) {
    const char *dirs[] = {"/mnt/lower", "/mnt/upper", "/mnt/work", "/mnt/merged"};
    for (size_t i = 0; i < sizeof(dirs) / sizeof(dirs[0]); ++i)
        CHECK(mkdir(dirs[i], 0700) == 0, "create overlay test directory");
    int fd = open("/mnt/lower/node.lock", O_CREAT | O_EXCL | O_RDWR, 0600);
    CHECK(fd >= 0 && close(fd) == 0, "create lower lock file");
    CHECK(mount("overlay", "/mnt/merged", "overlay", 0,
                "lowerdir=/mnt/lower,upperdir=/mnt/upper,workdir=/mnt/work,metacopy=off,redirect_dir=off") == 0,
          "mount test overlay");
    const char *lock = "/mnt/merged/node.lock";
    struct statx before, after;
    CHECK(statx(AT_FDCWD, lock, 0, STATX_BTIME | STATX_INO, &before) == 0, "stat image lock");
    fd = open(lock, O_RDWR);
    CHECK(fd >= 0, "open image lock for writing");
    struct flock exclusive = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
    CHECK(fcntl(fd, F_SETLK, &exclusive) == 0, "lock copied file");
    CHECK(access("/mnt/upper/node.lock", F_OK) == 0, "confirm copy-up occurred");
    CHECK(statx(AT_FDCWD, lock, 0, STATX_BTIME | STATX_INO, &after) == 0, "stat copied lock");
    if (!(before.stx_mask & after.stx_mask & STATX_BTIME) || before.stx_ino != after.stx_ino ||
        before.stx_btime.tv_sec != after.stx_btime.tv_sec || before.stx_btime.tv_nsec != after.stx_btime.tv_nsec) {
        printf("node.lock inode %llu -> %llu, birth time %lld.%09u -> %lld.%09u\n",
               (unsigned long long)before.stx_ino, (unsigned long long)after.stx_ino,
               (long long)before.stx_btime.tv_sec, before.stx_btime.tv_nsec,
               (long long)after.stx_btime.tv_sec, after.stx_btime.tv_nsec);
        errno = ESTALE;
        finish(0, "lock identity and birth time must survive writable open");
    }
    CHECK(close(fd) == 0 && umount("/mnt/merged") == 0, "release test overlay");
}

int main(void) {
    /* Refuse accidental execution on the runner or a normal workstation. */
    if (getpid() != 1)
        return 2;
    if (mount("proc", "/proc", "proc", 0, NULL) != 0)
        return 2;
    char command[4096] = {0};
    int args = open("/proc/cmdline", O_RDONLY);
    if (args < 0 || read(args, command, sizeof(command) - 1) <= 0 ||
        !strstr(command, "asagiri.loop-selftest=1"))
        return 2;
    close(args);
    CHECK(mount("devtmpfs", "/dev", "devtmpfs", 0, NULL) == 0, "mount private guest devtmpfs");
    CHECK(mkdir("/mnt", 0700) == 0, "create guest mountpoint");
    int control = open("/dev/loop-control", O_RDWR);
    CHECK(control >= 0, "open loop-control");
#ifdef __aarch64__
    verify_vdso_hashes();
#endif
    int number = ioctl(control, LOOP_CTL_GET_FREE);
    CHECK(number >= 0, "allocate loop device");
    close(control);
    char device[64];
    CHECK(snprintf(device, sizeof(device), "/dev/loop%d", number) < (int)sizeof(device),
          "format loop path");
    if (access(device, F_OK) != 0)
        CHECK(mknod(device, S_IFBLK | 0600, makedev(7, number)) == 0, "create loop node");
    int disk = open("/filesystem.img", O_RDWR);
    CHECK(disk >= 0, "open owned image");
    const char contents[] = "Asagiri loop filesystem survives reattachment\n";
    for (int pass = 0; pass != 2; ++pass) {
        int loop = open(device, O_RDWR);
        CHECK(loop >= 0, "open new loop device");
        struct loop_config config = {0};
        config.fd = disk;
        CHECK(ioctl(loop, LOOP_CONFIGURE, &config) == 0, "attach owned image");
        /* CONFIG_BLK_DEV_WRITE_MOUNTED=n rejects a filesystem mount while
         * another writer holds the block device open. Match losetup's
         * lifetime: configure, close, mount; reopen only after unmounting.
         * LOOP_CONFIGURE did not request AUTOCLEAR, so closing keeps the
         * attachment alive for the actual mount/write/reattach checks.
         */
        CHECK(close(loop) == 0, "close loop configuration handle");
        CHECK(mount(device, "/mnt", "ext4", 0, NULL) == 0, "mount ext4 loop image");
        if (pass == 0) {
            verify_copy_up_birth_time();
            int file = open("/mnt/proof", O_WRONLY | O_CREAT | O_EXCL, 0600);
            CHECK(file >= 0, "create persistent proof");
            CHECK(write(file, contents, sizeof(contents)) == (ssize_t)sizeof(contents), "write proof");
            CHECK(fsync(file) == 0, "persist proof");
            close(file);
        } else {
            char actual[sizeof(contents)] = {0};
            int file = open("/mnt/proof", O_RDONLY);
            CHECK(file >= 0, "reopen persistent proof");
            CHECK(read(file, actual, sizeof(actual)) == (ssize_t)sizeof(actual), "read proof");
            CHECK(memcmp(actual, contents, sizeof(contents)) == 0, "verify proof");
            close(file);
        }
        CHECK(umount("/mnt") == 0, "unmount owned filesystem");
        loop = open(device, O_RDWR);
        CHECK(loop >= 0, "reopen unmounted loop device");
        CHECK(ioctl(loop, LOOP_CLR_FD) == 0, "detach owned image");
        struct loop_info64 info;
        errno = 0;
        CHECK(ioctl(loop, LOOP_GET_STATUS64, &info) == -1 && errno == ENXIO, "confirm detached");
        close(loop);
    }
    close(disk);
    finish(1, "");
}
