/*
 * A real glibc NSS provider used only inside disposable CI containers.
 * No fixture account is added to passwd, group, shadow or PAM.
 * Public regression: https://github.com/libracoder/kiri/issues/349
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <nss.h>
#include <pwd.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#define MODE_PATH "/tmp/asagiri-nss-fixture/mode"
#define USER_ID 700001
#define PRIMARY_ID 700003
#define USER_GROUP 700002
#define ACCESS_GROUP 700010

static int mode(char out[32]) {
    int fd = open(MODE_PATH, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0)
        return errno;
    ssize_t len = read(fd, out, 31);
    int saved = errno;
    close(fd);
    if (len <= 0)
        return len == 0 ? EIO : saved;
    out[len] = '\0';
    return 0;
}

static enum nss_status failure(const char *state, int *error) {
    if (strcmp(state, "temporary") == 0) {
        *error = EAGAIN;
        return NSS_STATUS_TRYAGAIN;
    }
    if (strcmp(state, "unavailable") == 0) {
        *error = EIO;
        return NSS_STATUS_UNAVAIL;
    }
    return NSS_STATUS_SUCCESS;
}

static enum nss_status account(uid_t id, struct passwd *record,
                               char *buffer, size_t size, int *error) {
    char state[32];
    int err = mode(state);
    if (err) {
        *error = err;
        return NSS_STATUS_UNAVAIL;
    }
    enum nss_status status = failure(state, error);
    if (status != NSS_STATUS_SUCCESS)
        return status;
    if ((id != USER_ID && id != PRIMARY_ID) || strcmp(state, "missing") == 0) {
        *error = ENOENT;
        return NSS_STATUS_NOTFOUND;
    }
    const char *name = id == USER_ID ? "directory-fixture" : "directory-primary";
    const char *values[] = {name, "x", "Asagiri CI NSS fixture", "/nonexistent", "/bin/false"};
    size_t need = 0;
    for (size_t i = 0; i < 5; i++)
        need += strlen(values[i]) + 1;
    if (size < need) {
        *error = ERANGE;
        return NSS_STATUS_TRYAGAIN;
    }
    char **fields[] = {&record->pw_name, &record->pw_passwd, &record->pw_gecos,
                       &record->pw_dir, &record->pw_shell};
    for (size_t i = 0; i < 5; i++) {
        size_t len = strlen(values[i]) + 1;
        memcpy(buffer, values[i], len);
        *fields[i] = buffer;
        buffer += len;
    }
    record->pw_uid = id;
    record->pw_gid = id == USER_ID ? USER_GROUP : ACCESS_GROUP;
    return NSS_STATUS_SUCCESS;
}

enum nss_status _nss_asagirifixture_getpwnam_r(const char *name, struct passwd *record,
                                             char *buffer, size_t size, int *error) {
    uid_t id = strcmp(name, "directory-fixture") == 0 ? USER_ID :
               strcmp(name, "directory-primary") == 0 ? PRIMARY_ID : (uid_t)-1;
    return account(id, record, buffer, size, error);
}

enum nss_status _nss_asagirifixture_getpwuid_r(uid_t id, struct passwd *record,
                                            char *buffer, size_t size, int *error) {
    return account(id, record, buffer, size, error);
}

enum nss_status _nss_asagirifixture_getgrnam_r(const char *name, struct group *record,
                                            char *buffer, size_t size, int *error) {
    char state[32];
    int err = mode(state);
    if (err) {
        *error = err;
        return NSS_STATUS_UNAVAIL;
    }
    enum nss_status status = failure(state, error);
    if (status != NSS_STATUS_SUCCESS)
        return status;
    if (strcmp(name, "asagiri-nss-fixture") != 0 || strcmp(state, "group-missing") == 0) {
        *error = ENOENT;
        return NSS_STATUS_NOTFOUND;
    }
    const char group_name[] = "asagiri-nss-fixture";
    const char user_name[] = "directory-fixture";
    size_t padding = (sizeof(char *) - (uintptr_t)buffer % sizeof(char *)) % sizeof(char *);
    size_t need = padding + 2 * sizeof(char *) + sizeof(group_name) + sizeof(user_name) + 2;
    if (size < need) {
        *error = ERANGE;
        return NSS_STATUS_TRYAGAIN;
    }
    record->gr_mem = (char **)(buffer + padding);
    buffer += padding + 2 * sizeof(char *);
    record->gr_name = buffer;
    memcpy(buffer, group_name, sizeof(group_name));
    buffer += sizeof(group_name);
    record->gr_passwd = buffer;
    memcpy(buffer, "x", 2);
    buffer += 2;
    memcpy(buffer, user_name, sizeof(user_name));
    record->gr_mem[0] = strcmp(state, "revoked") == 0 ? NULL : buffer;
    record->gr_mem[1] = NULL;
    record->gr_gid = ACCESS_GROUP;
    return NSS_STATUS_SUCCESS;
}
