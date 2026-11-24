/* LD_PRELOAD slow-disk shim for real-path atomic-publication checks (test only).
 *
 * The first write(2) to each file whose resolved path starts with
 * $SLOW_WRITE_PREFIX (the Meeting folder) writes only half its bytes, then
 * sleeps $SLOW_WRITE_MS (default 1500) before returning; the caller's
 * write loop sends the rest. An unnamed O_TMPFILE inode resolves to
 * "<dir>/#<ino> (deleted)", so it matches too. Every delayed write is logged
 * to $SLOW_WRITE_LOG (path, bytes, times; never contents).
 *
 * With $SLOW_WRITE_NO_TMPFILE set, open(2)/open64(2)/openat(2) with O_TMPFILE
 * fail with EOPNOTSUPP (a filesystem without O_TMPFILE), to exercise the
 * direct exclusive-create fallback on the compiled binary.
 *
 * With $SLOW_WRITE_RACE set, the first linkat(2) into $SLOW_WRITE_PREFIX is
 * preceded by a "racing writer" that creates its destination name (content
 * "racer\n") just after Cantrip checked it was free, so linkat sees EEXIST
 * and Cantrip must step to the next name without touching the racer's file.
 *
 *   cc -shared -fPIC -O2 -o slow_write.so scripts/e2e/slow_write.c -ldl
 *   SLOW_WRITE_PREFIX=/notes LD_PRELOAD=$PWD/slow_write.so cantrip e2e meeting …
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#define MAX_SEEN 256
static ino_t seen[MAX_SEEN];
static int nseen;

static double now_s(void) {
    struct timespec t;
    clock_gettime(CLOCK_REALTIME, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static int first_write_of(int fd, char *path, size_t n) {
    const char *prefix = getenv("SLOW_WRITE_PREFIX");
    if (!prefix || !*prefix) return 0;
    char link[64];
    snprintf(link, sizeof link, "/proc/self/fd/%d", fd);
    ssize_t len = readlink(link, path, n - 1);
    if (len < 0) return 0;
    path[len] = 0;
    if (strncmp(path, prefix, strlen(prefix)) != 0) return 0;
    struct stat st;
    if (fstat(fd, &st) != 0 || !S_ISREG(st.st_mode)) return 0;
    for (int i = 0; i < nseen; i++)
        if (seen[i] == st.st_ino) return 0;
    if (nseen < MAX_SEEN) seen[nseen++] = st.st_ino;
    return 1;
}

ssize_t write(int fd, const void *buf, size_t count) {
    static ssize_t (*real)(int, const void *, size_t);
    if (!real) real = (ssize_t (*)(int, const void *, size_t))dlsym(RTLD_NEXT, "write");
    char path[4096];
    if (count < 2 || !first_write_of(fd, path, sizeof path)) return real(fd, buf, count);
    const char *ms = getenv("SLOW_WRITE_MS");
    long delay = ms ? atol(ms) : 1500;
    double t0 = now_s();
    ssize_t r = real(fd, buf, count / 2);
    int saved = errno;
    double t1 = now_s();
    usleep((useconds_t)delay * 1000);
    double t2 = now_s();
    const char *log = getenv("SLOW_WRITE_LOG");
    if (log) {
        FILE *f = fopen(log, "a");
        if (f) {
            fprintf(f, "slow-write\t%s\tof=%zu\twrote=%zd\tstart=%.6f\thalf_done=%.6f\tresumed=%.6f\n",
                    path, count, r, t0, t1, t2);
            fclose(f);
        }
    }
    errno = saved;
    return r;
}

static int refuse_tmpfile(int flags) {
    return getenv("SLOW_WRITE_NO_TMPFILE") && (flags & O_TMPFILE) == O_TMPFILE;
}

static void log_refused(const char *p) {
    const char *log = getenv("SLOW_WRITE_LOG");
    if (!log) return;
    FILE *f = fopen(log, "a");
    if (f) {
        fprintf(f, "refused-O_TMPFILE\t%s\n", p);
        fclose(f);
    }
}

int open(const char *p, int flags, ...) {
    static int (*real)(const char *, int, ...);
    if (!real) real = (int (*)(const char *, int, ...))dlsym(RTLD_NEXT, "open");
    va_list ap;
    va_start(ap, flags);
    mode_t mode = (flags & (O_CREAT | O_TMPFILE)) ? va_arg(ap, mode_t) : 0;
    va_end(ap);
    if (refuse_tmpfile(flags)) { log_refused(p); errno = EOPNOTSUPP; return -1; }
    return real(p, flags, mode);
}

int open64(const char *p, int flags, ...) {
    static int (*real)(const char *, int, ...);
    if (!real) real = (int (*)(const char *, int, ...))dlsym(RTLD_NEXT, "open64");
    va_list ap;
    va_start(ap, flags);
    mode_t mode = (flags & (O_CREAT | O_TMPFILE)) ? va_arg(ap, mode_t) : 0;
    va_end(ap);
    if (refuse_tmpfile(flags)) { log_refused(p); errno = EOPNOTSUPP; return -1; }
    return real(p, flags, mode);
}

int openat(int dirfd, const char *p, int flags, ...) {
    static int (*real)(int, const char *, int, ...);
    if (!real) real = (int (*)(int, const char *, int, ...))dlsym(RTLD_NEXT, "openat");
    va_list ap;
    va_start(ap, flags);
    mode_t mode = (flags & (O_CREAT | O_TMPFILE)) ? va_arg(ap, mode_t) : 0;
    va_end(ap);
    if (refuse_tmpfile(flags)) { log_refused(p); errno = EOPNOTSUPP; return -1; }
    return real(dirfd, p, flags, mode);
}

int linkat(int olddirfd, const char *oldpath, int newdirfd, const char *newpath, int flags) {
    static int (*real)(int, const char *, int, const char *, int);
    static int raced;
    if (!real) real = (int (*)(int, const char *, int, const char *, int))dlsym(RTLD_NEXT, "linkat");
    const char *prefix = getenv("SLOW_WRITE_PREFIX");
    if (getenv("SLOW_WRITE_RACE") && !raced && prefix &&
        strncmp(newpath, prefix, strlen(prefix)) == 0) {
        raced = 1;
        static int (*ropen)(const char *, int, ...);
        if (!ropen) ropen = (int (*)(const char *, int, ...))dlsym(RTLD_NEXT, "open");
        int fd = ropen(newpath, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0644);
        if (fd >= 0) {
            static ssize_t (*rwrite)(int, const void *, size_t);
            if (!rwrite) rwrite = (ssize_t (*)(int, const void *, size_t))dlsym(RTLD_NEXT, "write");
            rwrite(fd, "racer\n", 6);
            close(fd);
        }
        const char *log = getenv("SLOW_WRITE_LOG");
        if (log) {
            FILE *f = fopen(log, "a");
            if (f) { fprintf(f, "raced\t%s\n", newpath); fclose(f); }
        }
    }
    return real(olddirfd, oldpath, newdirfd, newpath, flags);
}
