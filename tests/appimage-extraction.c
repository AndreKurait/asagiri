/* Pure filesystem tests for the runtime's actual extraction/cleanup helpers. */
#include <ftw.h>
#include <limits.h>
#include "appimage-extraction.h"
#include "appimage-cleanup-under-test.h"

#define CHECK(condition, name) do { \
    if (!(condition)) { fprintf(stderr, "FAIL: %s\n", name); return 1; } \
    printf("PASS: %s\n", name); \
} while (0)

static bool write_file(const char *path) {
    int fd = open(path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0755);
    if (fd < 0)
        return false;
    bool success = write(fd, "#!/bin/sh\nexit 0\n", 17) == 17;
    close(fd);
    return success;
}

static void retain_identity(const char *path) {
    struct stat st;
    if (lstat(path, &st) == 0) {
        printf("OWNED\t%s\t%llu\t%llu\t%u\n", path,
               (unsigned long long) st.st_dev, (unsigned long long) st.st_ino, st.st_uid);
        fflush(stdout);
    }
}

int main(int argc, char **argv) {
    if (argc != 2)
        return 2;
    CHECK(!strcmp(asagiri_temp_base(true, "/attacker-controlled"), "/tmp"), "root ignores TMPDIR");
    CHECK(!strcmp(asagiri_temp_base(false, argv[1]), argv[1]), "unprivileged private TMPDIR remains supported");
    CHECK(!asagiri_safe_parents("relative", geteuid()), "relative ancestry is rejected");
    CHECK(asagiri_safe_parents(argv[1], geteuid()), "owned private ancestry is accepted");
    CHECK(chmod(argv[1], 0777) == 0, "prepare writable parent");
    CHECK(!asagiri_safe_parents(argv[1], geteuid()), "writable non-sticky parent is rejected");
    CHECK(chmod(argv[1], 0700) == 0, "restore owned parent");
    char link[PATH_MAX];
    CHECK(snprintf(link, sizeof(link), "%s/linked", argv[1]) < (int) sizeof(link), "bounded link path");
    CHECK(symlink(argv[1], link) == 0, "prepare linked parent");
    CHECK(!asagiri_safe_parents(link, geteuid()), "linked parent is rejected");
    CHECK(unlink(link) == 0, "remove owned parent link");
    CHECK(setenv("TMPDIR", argv[1], 1) == 0, "select private fixture TMPDIR");

    struct asagiri_extraction first, second;
    CHECK(asagiri_create_extraction(&first), "first invocation creates private extraction");
    retain_identity(first.path);
    CHECK(asagiri_create_extraction(&second), "overlapping invocation creates private extraction");
    retain_identity(second.path);
    CHECK(strcmp(first.path, second.path) != 0, "overlapping invocations have distinct paths");
    CHECK(asagiri_owned_extraction(&first) && asagiri_owned_extraction(&second), "both directories have owned identities and mode 0700");
    if (geteuid() == 0)
        CHECK(!strncmp(first.path, "/tmp/asagiri-appimage-", 22), "root extraction uses system tmp");
    char apprun[PATH_MAX];
    CHECK(snprintf(apprun, sizeof(apprun), "%s/AppRun", first.path) < (int) sizeof(apprun), "bounded AppRun path");
    CHECK(write_file(apprun), "create regular executable fixture");
    CHECK(asagiri_valid_apprun(&first), "owned regular AppRun is accepted");
    CHECK(chmod(apprun, 0777) == 0, "prepare writable AppRun");
    CHECK(!asagiri_valid_apprun(&first), "writable AppRun is rejected");
    CHECK(unlink(apprun) == 0 && symlink("/bin/sh", apprun) == 0, "prepare linked AppRun");
    CHECK(!asagiri_valid_apprun(&first), "linked AppRun is rejected");
    CHECK(unlink(apprun) == 0 && mkfifo(apprun, 0700) == 0, "prepare FIFO AppRun");
    CHECK(!asagiri_valid_apprun(&first), "FIFO AppRun is rejected without blocking");
    CHECK(unlink(apprun) == 0 && write_file(apprun), "restore regular AppRun");
    if (geteuid() == 0) {
        CHECK(chown(apprun, 65534, 65534) == 0, "prepare foreign-owned AppRun");
        CHECK(!asagiri_valid_apprun(&first), "foreign-owned AppRun is rejected");
        CHECK(chown(apprun, 0, 0) == 0, "restore root-owned AppRun");
    }
    char second_path[PATH_MAX];
    CHECK(snprintf(second_path, sizeof(second_path), "%s", second.path) < (int) sizeof(second_path), "retain second identity path");
    CHECK(asagiri_cleanup_extraction(&second), "completed invocation cleans its own directory");
    CHECK(access(second_path, F_OK) != 0 && asagiri_valid_apprun(&first), "first invocation survives second cleanup");

    char original[PATH_MAX], retained[PATH_MAX], marker[PATH_MAX];
    CHECK(snprintf(original, sizeof(original), "%s", first.path) < (int) sizeof(original), "retain first path");
    CHECK(snprintf(retained, sizeof(retained), "%s-retained", first.path) < (int) sizeof(retained), "bounded renamed path");
    CHECK(snprintf(marker, sizeof(marker), "%s/marker", first.path) < (int) sizeof(marker), "bounded replacement marker");
    struct stat identity = first.identity;
    CHECK(rename(original, retained) == 0 && mkdir(original, 0700) == 0 && write_file(marker), "replace an extraction name inside owned test scope");
    retain_identity(retained);
    retain_identity(original);
    CHECK(!asagiri_cleanup_extraction(&first), "cleanup refuses a replaced directory identity");
    CHECK(access(marker, F_OK) == 0 && access(retained, F_OK) == 0, "refusal preserves both replacement and original");
    CHECK(unlink(marker) == 0 && rmdir(original) == 0, "remove only the test-created replacement");
    first.path = strdup(retained);
    first.identity = identity;
    CHECK(first.path && asagiri_cleanup_extraction(&first), "remove retained original by its original identity");
    return 0;
}
