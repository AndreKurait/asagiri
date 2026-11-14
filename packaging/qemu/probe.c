/* Exercise a foreign static ELF directly: no host binfmt registration. */
#define _GNU_SOURCE
#include <assert.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

static _Atomic int counter;

static void *count(void *unused) {
    (void)unused;
    for (int i = 0; i < 10000; i++)
        atomic_fetch_add(&counter, 1);
    return NULL;
}

int main(int argc, char **argv) {
    assert(argc == 2 && !strcmp(argv[1], "asagiri-arm64-probe"));
    struct utsname platform;
    assert(uname(&platform) == 0 && !strcmp(platform.machine, "aarch64"));
    pthread_t workers[4];
    for (int i = 0; i < 4; i++)
        assert(pthread_create(&workers[i], NULL, count, NULL) == 0);
    for (int i = 0; i < 4; i++)
        assert(pthread_join(workers[i], NULL) == 0);
    assert(counter == 40000);
    long page = sysconf(_SC_PAGESIZE);
    char *memory = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    assert(memory != MAP_FAILED);
    strcpy(memory, "ARM64");
    assert(mprotect(memory, page, PROT_READ) == 0);
    assert(!strcmp(memory, "ARM64"));
    assert(munmap(memory, page) == 0);
    int channel[2];
    assert(pipe(channel) == 0);
    pid_t child = fork();
    assert(child >= 0);
    if (child == 0) {
        close(channel[0]);
        assert(write(channel[1], "child", 5) == 5);
        _exit(42);
    }
    close(channel[1]);
    char message[5];
    assert(read(channel[0], message, sizeof(message)) == 5);
    assert(!memcmp(message, "child", 5));
    close(channel[0]);
    int status;
    assert(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 42);
    char filename[] = "asagiri-qemu-probe-XXXXXX";
    int fd = mkstemp(filename);
    assert(fd >= 0);
    assert(write(fd, "disk", 4) == 4);
    assert(fsync(fd) == 0);
    assert(lseek(fd, 0, SEEK_SET) == 0);
    assert(read(fd, message, 4) == 4 && !memcmp(message, "disk", 4));
    assert(close(fd) == 0 && unlink(filename) == 0);
    puts("PASS ARM64 ELF: arguments, uname, threads, atomics, memory, fork, pipes and files");
    return 0;
}
