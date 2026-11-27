#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/fuse.h>
#include <linux/magic.h>
#include <poll.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <sys/vfs.h>
#include <sys/wait.h>
#include <unistd.h>

/* A synthetic FUSE share entirely inside the diskless guest.
 * Only the test's server state can change the served bytes.
 * No host filesystem or device is supplied to the VM. */
static const char contents[] = "unchanged guest fixture\n";
/* The fixture changes the server's pathname without sending a FUSE
 * invalidation, like a rename or deletion on a virtiofs host. */
static struct fixture_state {
    uint32_t file_node;
    unsigned char first_byte;
} *fixture;

static _Noreturn void poweroff(void)
{
    sync();
    reboot(RB_POWER_OFF);
    _exit(1);
}

static _Noreturn void fail(const char *what)
{
    dprintf(STDOUT_FILENO, "ASAGIRI_FSNOTIFY_TEST_FAIL: %s: %s\n", what, strerror(errno));
    poweroff();
}

static void require(int condition, const char *what)
{
    if (!condition) {
        errno = EINVAL;
        fail(what);
    }
}

static void reply(int fd, uint64_t unique, int error, const void *data, size_t size)
{
    struct fuse_out_header header = {
        .len = (uint32_t)(sizeof(header) + size), .error = -error, .unique = unique,
    };
    struct iovec vec[] = {{&header, sizeof(header)}, {(void *)data, size}};
    ssize_t written = writev(fd, vec, 2);
    if (written != (ssize_t)(sizeof(header) + size))
        fail("synthetic FUSE reply");
}

static struct fuse_attr attributes(uint64_t node)
{
    struct fuse_attr attr = {
        .ino = node, .nlink = 1, .uid = 0, .gid = 0, .blksize = 4096,
        .atime = 1, .mtime = 1, .ctime = 1,
    };
    if (node == FUSE_ROOT_ID || node == 4) {
        attr.mode = S_IFDIR | 0755;
        attr.nlink = 2;
    } else if (node == 2 || node == 5) {
        attr.mode = S_IFREG | 0644;
        attr.size = sizeof(contents) - 1;
        attr.blocks = 1;
    } else {
        attr.mode = S_IFLNK | 0777;
        attr.size = strlen("/outside");
    }
    return attr;
}

static _Noreturn void serve_fuse(int fd)
{
    union {
        struct fuse_in_header alignment;
        unsigned char bytes[131072];
    } buffer;
    for (;;) {
        ssize_t length = read(fd, buffer.bytes, sizeof(buffer.bytes));
        if (length < 0 && errno == EINTR)
            continue;
        if (length < 0 && errno == ENODEV)
            _exit(0);
        if (length < (ssize_t)sizeof(struct fuse_in_header))
            fail("synthetic FUSE request");
        const struct fuse_in_header *in = (const void *)buffer.bytes;
        require(in->len == (uint32_t)length, "complete FUSE request");
        const void *data = buffer.bytes + sizeof(*in);
        switch (in->opcode) {
        case FUSE_INIT: {
            require((size_t)length >= sizeof(*in) + sizeof(struct fuse_init_in), "complete FUSE init");
            const struct fuse_init_in *init = data;
            struct fuse_init_out out = {
                .major = FUSE_KERNEL_VERSION,
                .minor = init->minor < FUSE_KERNEL_MINOR_VERSION ? init->minor : FUSE_KERNEL_MINOR_VERSION,
                .max_readahead = 4096, .flags = FUSE_ASYNC_READ,
                .max_background = 16, .congestion_threshold = 12,
                .max_write = 4096, .time_gran = 1,
            };
            reply(fd, in->unique, 0, &out, sizeof(out));
            break;
        }
        case FUSE_LOOKUP: {
            const char *name = data;
            require(memchr(name, '\0', (size_t)length - sizeof(*in)) != NULL, "terminated FUSE name");
            uint64_t node = !strcmp(name, "file") ? __atomic_load_n(&fixture->file_node, __ATOMIC_RELAXED) :
                            !strcmp(name, "link") ? 3 : !strcmp(name, "nested") ? 4 : 0;
            if (!node) {
                reply(fd, in->unique, ENOENT, NULL, 0);
                break;
            }
            struct fuse_entry_out out = {
                .nodeid = node, .generation = 1, .entry_valid = 60, .attr_valid = 60,
                .attr = attributes(node),
            };
            reply(fd, in->unique, 0, &out, sizeof(out));
            break;
        }
        case FUSE_GETATTR: {
            struct fuse_attr_out out = {.attr_valid = 60, .attr = attributes(in->nodeid)};
            reply(fd, in->unique, 0, &out, sizeof(out));
            break;
        }
        case FUSE_OPEN:
        case FUSE_OPENDIR: {
            struct fuse_open_out out = {.fh = in->nodeid, .open_flags = FOPEN_KEEP_CACHE};
            reply(fd, in->unique, 0, &out, sizeof(out));
            break;
        }
        case FUSE_READ: {
            require((size_t)length >= sizeof(*in) + sizeof(struct fuse_read_in), "complete FUSE read");
            const struct fuse_read_in *read_request = data;
            size_t offset = read_request->offset < sizeof(contents) - 1 ? (size_t)read_request->offset : sizeof(contents) - 1;
            size_t size = sizeof(contents) - 1 - offset;
            if (size > read_request->size)
                size = read_request->size;
            char bytes[sizeof(contents)];
            memcpy(bytes, contents, sizeof(bytes));
            if (in->nodeid == 2)
                bytes[0] = __atomic_load_n(&fixture->first_byte, __ATOMIC_RELAXED);
            reply(fd, in->unique, 0, bytes + offset, size);
            break;
        }
        case FUSE_READLINK:
            reply(fd, in->unique, 0, "/outside", strlen("/outside"));
            break;
        case FUSE_STATFS: {
            struct fuse_statfs_out out = {.st = {.blocks = 1, .files = 4, .bsize = 4096, .namelen = 255, .frsize = 4096}};
            reply(fd, in->unique, 0, &out, sizeof(out));
            break;
        }
        case FUSE_FORGET:
        case FUSE_BATCH_FORGET:
            break; /* These requests have no reply. */
        case FUSE_WRITE:
        case FUSE_SETATTR:
        case FUSE_CREATE:
        case FUSE_UNLINK:
        case FUSE_RENAME:
        case FUSE_RENAME2:
            require(0, "notification attempted a filesystem mutation");
            break;
        case FUSE_ACCESS:
        case FUSE_FLUSH:
        case FUSE_RELEASE:
        case FUSE_RELEASEDIR:
        case FUSE_FSYNC:
        case FUSE_FSYNCDIR:
        case FUSE_READDIR:
            reply(fd, in->unique, 0, NULL, 0);
            break;
        default:
            reply(fd, in->unique, ENOSYS, NULL, 0);
            break;
        }
    }
}

static void unchanged(int file)
{
    char bytes[sizeof(contents)] = {0};
    require(lseek(file, 0, SEEK_SET) == 0, "rewind fixture");
    require(read(file, bytes, sizeof(bytes)) == (ssize_t)sizeof(contents) - 1, "read fixture");
    require(!memcmp(bytes, contents, sizeof(contents) - 1), "notifications preserved file contents");
}

static void change_cached_name(int control, char operation, uint32_t node)
{
    char line[] = "M\t/mnt/mac/file\n";
    line[0] = operation;
    __atomic_store_n(&fixture->file_node, node, __ATOMIC_RELAXED);
    if (write(control, line, sizeof(line) - 1) != sizeof(line) - 1)
        fail("refresh cached filename");
    struct stat info;
    if (node) {
        require(stat("/mnt/mac/file", &info) == 0 && info.st_ino == node,
                "lookup sees replacement inode without waiting for cache timeout");
    } else {
        errno = 0;
        require(stat("/mnt/mac/file", &info) == -1 && errno == ENOENT,
                "lookup no longer sees deleted file");
    }
}

static void read_held_byte(int file, char expected)
{
    char byte;
    require(pread(file, &byte, 1, 0) == 1, "read held descriptor");
    if (byte != expected)
        dprintf(STDOUT_FILENO, "held descriptor expected %c, got %c\n", expected, byte);
    require(byte == expected, "held descriptor reports the expected byte");
}

static void check_repeated_edits(int control, int file)
{
    const char line[] = "M\t/mnt/mac/file\n";
    const char edits[] = {'1', '2', contents[0]};
    for (size_t i = 0; i < sizeof(edits); ++i) {
        __atomic_store_n(&fixture->first_byte, edits[i], __ATOMIC_RELAXED);
        require(write(control, line, sizeof(line) - 1) == sizeof(line) - 1,
                "inject same-inode edit");
        read_held_byte(file, edits[i]);
    }
    dprintf(STDOUT_FILENO, "ASAGIRI_FSNOTIFY_CONTROL_PASS: repeated edits refresh the held descriptor without reopen\n");
}

static void drain(int fd)
{
    char buffer[4096];
    while (read(fd, buffer, sizeof(buffer)) > 0) {}
    require(errno == EAGAIN, "drain inotify");
}

static void notify(int control, int events, int directory, int file_watch,
                   char operation, uint32_t expected_directory, uint32_t expected_file)
{
    char line[] = "M\t/mnt/mac/file\n";
    line[0] = operation;
    drain(events);
    require(write(control, line, sizeof(line) - 1) == (ssize_t)sizeof(line) - 1, "inject notification");
    uint32_t directory_mask = 0, file_mask = 0;
    for (int attempt = 0; attempt < 4; ++attempt) {
        struct pollfd ready = {.fd = events, .events = POLLIN};
        require(poll(&ready, 1, 1000) == 1, "inotify notification ready");
        union {
            struct inotify_event alignment;
            char bytes[4096];
        } buffer;
        ssize_t size = read(events, buffer.bytes, sizeof(buffer.bytes));
        require(size > 0, "read notification");
        for (size_t at = 0; at < (size_t)size;) {
            require(at + sizeof(struct inotify_event) <= (size_t)size, "complete notification header");
            struct inotify_event *event = (void *)(buffer.bytes + at);
            require(at + sizeof(*event) + event->len <= (size_t)size, "complete notification");
            require(!event->len || memchr(event->name, '\0', event->len) != NULL, "terminated notification name");
            if (event->wd == directory && event->len && !strcmp(event->name, "file"))
                directory_mask |= event->mask;
            if (event->wd == file_watch)
                file_mask |= event->mask;
            at += sizeof(*event) + event->len;
        }
        if ((directory_mask & expected_directory) == expected_directory &&
            (file_mask & expected_file) == expected_file) {
            dprintf(STDOUT_FILENO, "ASAGIRI_FSNOTIFY_CONTROL_PASS: %c directory=%x file=%x\n",
                    operation, directory_mask, file_mask);
            return;
        }
    }
    require(0, "expected notification masks");
}

static void rejected(int control, int events, const char *line, const char *name, int expected)
{
    drain(events);
    errno = 0;
    ssize_t written = write(control, line, strlen(line));
    int error = errno;
    dprintf(STDOUT_FILENO, "ASAGIRI_FSNOTIFY_REJECTION_RESULT: %s result=%zd errno=%d\n",
            name, written, error);
    /* A no-follow lookup of the parent can reject its final symlink as
     * ENOTDIR before walking the remaining component. Both outcomes deny
     * traversal; the ordinary stat below proves the target really exists. */
    require(written == -1 && (error == expected || (expected == ELOOP && error == ENOTDIR)), name);
    struct pollfd ready = {.fd = events, .events = POLLIN};
    require(poll(&ready, 1, 100) == 0, "rejected path emitted no notification");
    dprintf(STDOUT_FILENO, "ASAGIRI_FSNOTIFY_CONTROL_PASS: rejected %s\n", name);
}

static void guard_file(const char *path)
{
    int fd = open(path, O_CREAT | O_EXCL | O_WRONLY | O_CLOEXEC, 0600);
    if (fd < 0)
        fail("create existing rejection target");
    require(write(fd, contents, sizeof(contents) - 1) == (ssize_t)sizeof(contents) - 1,
            "write existing rejection target");
    require(close(fd) == 0, "close existing rejection target");
}

static void child_control(int control, int change_namespace)
{
    pid_t child = fork();
    if (child < 0)
        fail("fork rejection control");
    if (!child) {
        if (change_namespace) {
            if (unshare(CLONE_NEWNS) < 0)
                _exit(2);
            const char line[] = "M\t/mnt/mac/file\n";
            errno = 0;
            _exit(write(control, line, sizeof(line) - 1) == -1 && errno == EPERM ? 0 : 3);
        }
        close(control);
        if (setgid(65534) < 0 || setuid(65534) < 0)
            _exit(4);
        errno = 0;
        int opened = open("/proc/asagiri-fsnotify", O_WRONLY | O_CLOEXEC);
        _exit(opened == -1 && (errno == EACCES || errno == EPERM) ? 0 : 5);
    }
    int status = 0;
    require(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0,
            change_namespace ? "container namespace denied" : "unprivileged open denied");
    dprintf(STDOUT_FILENO, "ASAGIRI_FSNOTIFY_CONTROL_PASS: %s\n",
            change_namespace ? "container namespace denied with inherited descriptor" : "unprivileged open denied");
}

int main(void)
{
    int console = open("/dev/console", O_RDWR);
    if (console >= 0)
        for (int fd = 0; fd < 3; ++fd)
            dup2(console, fd);
    setvbuf(stdout, NULL, _IONBF, 0);
    if (mount("proc", "/proc", "proc", 0, NULL) < 0)
        fail("mount proc");
    int control = open("/proc/asagiri-fsnotify", O_WRONLY | O_CLOEXEC);
    if (control < 0)
        fail("open guest notification interface");
    require(mkdir("/mnt", 0755) == 0 && mkdir("/mnt/mac", 0755) == 0, "create synthetic share directory");
    require(mkdir("/outside", 0755) == 0, "create symlink rejection target");
    guard_file("/outside/guard");
    guard_file("/mnt/outside");
    if (mknod("/dev/fuse", S_IFCHR | 0600, makedev(10, 229)) < 0)
        fail("create guest FUSE device");
    int fuse = open("/dev/fuse", O_RDWR | O_CLOEXEC);
    if (fuse < 0)
        fail("open guest FUSE device");
    char options[128];
    snprintf(options, sizeof(options), "fd=%d,rootmode=40000,user_id=0,group_id=0,max_read=4096", fuse);
    if (mount("guest-fixture", "/mnt/mac", "fuse", MS_NOSUID | MS_NODEV | MS_NOATIME, options) < 0)
        fail("mount synthetic share");
    fixture = mmap(NULL, sizeof(*fixture), PROT_READ | PROT_WRITE,
                   MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (fixture == MAP_FAILED)
        fail("share synthetic filename state");
    *fixture = (struct fixture_state){.file_node = 2, .first_byte = contents[0]};
    pid_t server = fork();
    if (server < 0)
        fail("fork synthetic FUSE server");
    if (!server)
        serve_fuse(fuse);
    close(fuse);
    struct statfs info;
    require(statfs("/mnt/mac", &info) == 0 && info.f_type == FUSE_SUPER_MAGIC, "real guest FUSE filesystem");
    int file = open("/mnt/mac/file", O_RDONLY | O_CLOEXEC);
    if (file < 0)
        fail("open fixture file");
    unchanged(file);
    int events = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    if (events < 0)
        fail("open inotify");
    int directory = inotify_add_watch(events, "/mnt/mac", IN_ALL_EVENTS);
    int file_watch = inotify_add_watch(events, "/mnt/mac/file", IN_ALL_EVENTS);
    require(directory >= 0 && file_watch >= 0, "watch directory and file");
    notify(control, events, directory, file_watch, 'C', IN_CREATE, 0);
    unchanged(file);
    notify(control, events, directory, file_watch, 'M', IN_MODIFY | IN_CLOSE_WRITE, IN_MODIFY | IN_CLOSE_WRITE);
    unchanged(file);
    notify(control, events, directory, file_watch, 'A', IN_ATTRIB, IN_ATTRIB);
    unchanged(file);
    check_repeated_edits(control, file);
    child_control(control, 0);
    child_control(control, 1);
    rejected(control, events, "M\t/outside/guard\n", "outside-share path", EINVAL);
    rejected(control, events, "M\t/mnt/mac/../outside\n", "parent traversal", EXDEV);
    struct stat direct, through_link;
    require(stat("/outside/guard", &direct) == 0 &&
            stat("/mnt/mac/link/guard", &through_link) == 0 &&
            S_ISREG(through_link.st_mode) && direct.st_dev == through_link.st_dev &&
            direct.st_ino == through_link.st_ino, "symlink leads to an existing outside target");
    rejected(control, events, "M\t/mnt/mac/link/guard\n", "symlink traversal", ELOOP);
    if (mount("tmpfs", "/mnt/mac/nested", "tmpfs", MS_NOSUID | MS_NODEV, "size=4096") < 0)
        fail("mount crossing control");
    guard_file("/mnt/mac/nested/guard");
    rejected(control, events, "M\t/mnt/mac/nested/guard\n", "nested mount", EXDEV);
    notify(control, events, directory, file_watch, 'D', IN_DELETE, IN_DELETE_SELF);
    unchanged(file);
    struct stat cached;
    require(stat("/mnt/mac/file", &cached) == 0 && cached.st_ino == 2,
            "cache the original pathname before replacement");
    change_cached_name(control, 'M', 5);
    unchanged(file);
    dprintf(STDOUT_FILENO, "ASAGIRI_FSNOTIFY_CONTROL_PASS: replacement looks up new inode and preserves open file\n");
    close(events);
    change_cached_name(control, 'M', 2);
    dprintf(STDOUT_FILENO, "ASAGIRI_FSNOTIFY_CONTROL_PASS: replacement expires cache without watchers\n");
    change_cached_name(control, 'D', 0);
    dprintf(STDOUT_FILENO, "ASAGIRI_FSNOTIFY_CONTROL_PASS: deletion expires cache without watchers\n");
    dprintf(STDOUT_FILENO, "ASAGIRI_FSNOTIFY_TEST_PASS: events scopes namespace contents\n");
    poweroff();
}
