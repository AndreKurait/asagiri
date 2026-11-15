/* Private extraction for Asagiri's pinned AppImage runtime.
 * Included before running AppRun, so cached application code is never trusted.
 */
#ifndef ASAGIRI_APPIMAGE_EXTRACTION_H
#define ASAGIRI_APPIMAGE_EXTRACTION_H

#include <errno.h>
#include <fcntl.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

struct asagiri_extraction {
    char *path;
    struct stat identity;
};

static const char *asagiri_temp_base(bool elevated, const char *requested) {
    return !elevated && requested && requested[0] ? requested : "/tmp";
}

/* Check every resolved parent. A private child is not safe under a directory
 * another user owns: that owner could rename the child and replace its name.
 */
static bool asagiri_safe_parents(const char *path, uid_t owner) {
    if (path[0] != '/') {
        errno = EINVAL;
        return false;
    }
    char *copy = strdup(path);
    if (!copy)
        return false;
    int fd = open("/", O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
    char *next = NULL;
    char *part = strtok_r(copy, "/", &next);
    bool safe = false;
    while (fd >= 0) {
        struct stat st;
        if (fstat(fd, &st) != 0)
            break;
        if (!S_ISDIR(st.st_mode) || (st.st_uid != 0 && st.st_uid != owner) ||
            ((st.st_mode & (S_IWGRP | S_IWOTH)) &&
             !(st.st_uid == 0 && (st.st_mode & S_ISVTX)))) {
            errno = EPERM;
            break;
        }
        if (!part) {
            safe = true;
            break;
        }
        if (!strcmp(part, ".") || !strcmp(part, "..")) {
            errno = EINVAL;
            break;
        }
        int child = openat(fd, part, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
        int error = errno;
        close(fd);
        fd = child;
        errno = error;
        part = strtok_r(NULL, "/", &next);
    }
    int error = errno;
    if (fd >= 0)
        close(fd);
    free(copy);
    errno = error;
    return safe;
}

static bool asagiri_create_extraction(struct asagiri_extraction *extraction) {
    memset(extraction, 0, sizeof(*extraction));
    bool elevated = geteuid() == 0 || geteuid() != getuid() || getegid() != getgid();
    const char *requested = asagiri_temp_base(elevated, getenv("TMPDIR"));
    if (elevated) {
        struct stat st;
        if (lstat("/tmp", &st) != 0)
            return false;
        if (!S_ISDIR(st.st_mode) || st.st_uid != 0 || !(st.st_mode & S_ISVTX)) {
            errno = EPERM;
            return false;
        }
    }
    char *base = realpath(requested, NULL);
    if (!base)
        return false;
    if (!asagiri_safe_parents(base, geteuid())) {
        int error = errno;
        free(base);
        errno = error;
        return false;
    }
    static const char suffix[] = "/asagiri-appimage-XXXXXX";
    char *path = malloc(strlen(base) + sizeof(suffix));
    if (path)
        sprintf(path, "%s%s", base, suffix);
    free(base);
    if (!path)
        return false;
    if (!mkdtemp(path)) {
        free(path);
        return false;
    }
    if (lstat(path, &extraction->identity) != 0 ||
        !S_ISDIR(extraction->identity.st_mode) ||
        extraction->identity.st_uid != geteuid() ||
        (extraction->identity.st_mode & 07777) != 0700) {
        /* Ownership was not established; never remove an unverified name. */
        free(path);
        errno = EPERM;
        return false;
    }
    extraction->path = path;
    return true;
}

static bool asagiri_owned_extraction(const struct asagiri_extraction *extraction) {
    struct stat st;
    if (lstat(extraction->path, &st) != 0)
        return false;
    if (!S_ISDIR(st.st_mode) || st.st_uid != geteuid() ||
        (st.st_mode & 07777) != 0700 || st.st_dev != extraction->identity.st_dev ||
        st.st_ino != extraction->identity.st_ino) {
        errno = EPERM;
        return false;
    }
    return true;
}

static bool asagiri_valid_apprun(const struct asagiri_extraction *extraction) {
    if (!asagiri_owned_extraction(extraction))
        return false;
    int dir = open(extraction->path, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
    if (dir < 0)
        return false;
    int file = openat(dir, "AppRun", O_RDONLY | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK);
    int error = errno;
    close(dir);
    if (file < 0) {
        errno = error;
        return false;
    }
    struct stat st;
    bool valid = fstat(file, &st) == 0 && S_ISREG(st.st_mode) &&
        st.st_uid == geteuid() && (st.st_mode & S_IXUSR) &&
        !(st.st_mode & (S_IWGRP | S_IWOTH | S_ISUID | S_ISGID));
    error = valid ? 0 : EPERM;
    close(file);
    errno = error;
    return valid;
}

#endif
