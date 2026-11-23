/* LD_PRELOAD fault injector for real-path archive verification (test only).
 *
 * Every fsync(2)/fdatasync(2) is logged (resolved path) to $FSYNC_FAULT_LOG
 * when set. While the file $FSYNC_FAULT_ARMED exists, an fsync of exactly the
 * directory $FSYNC_FAULT_PATH fails with EIO (the descriptor is untouched).
 *
 *   cc -shared -fPIC -O2 -o fsync_fault.so scripts/e2e/fsync_fault.c -ldl
 *   FSYNC_FAULT_PATH=/notes FSYNC_FAULT_ARMED=/tmp/armed \
 *     LD_PRELOAD=$PWD/fsync_fault.so cantrip e2e meeting …
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int path_of(int fd, char *buf, size_t n) {
    char link[64];
    snprintf(link, sizeof link, "/proc/self/fd/%d", fd);
    ssize_t len = readlink(link, buf, n - 1);
    if (len < 0) return -1;
    buf[len] = 0;
    return 0;
}

static int faulted(int fd, const char *op) {
    char p[4096];
    if (path_of(fd, p, sizeof p) != 0) return 0;
    const char *want = getenv("FSYNC_FAULT_PATH");
    const char *armed = getenv("FSYNC_FAULT_ARMED");
    int hit = want && armed && strcmp(p, want) == 0 && access(armed, F_OK) == 0;
    const char *log = getenv("FSYNC_FAULT_LOG");
    if (log) {
        FILE *f = fopen(log, "a");
        if (f) {
            fprintf(f, "%s\t%s\t%s\n", op, hit ? "EIO" : "ok", p);
            fclose(f);
        }
    }
    return hit;
}

int fsync(int fd) {
    static int (*real)(int);
    if (!real) real = (int (*)(int))dlsym(RTLD_NEXT, "fsync");
    if (faulted(fd, "fsync")) { errno = EIO; return -1; }
    return real(fd);
}

int fdatasync(int fd) {
    static int (*real)(int);
    if (!real) real = (int (*)(int))dlsym(RTLD_NEXT, "fdatasync");
    if (faulted(fd, "fdatasync")) { errno = EIO; return -1; }
    return real(fd);
}
