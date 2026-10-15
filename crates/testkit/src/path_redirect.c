/*
 * LD_PRELOAD path redirector for the pinned upstream `rpcd` daemon.
 *
 * rpcd has its ACL glob, plugin directories, and session state paths baked
 * in as absolute paths such as `/usr/local/share/rpcd/acl.d`,
 * `/usr/local/lib/rpcd`, and `/var/run/rpcd`. Those system directories are
 * unavailable (or root-owned) in userland tests, even though the test has
 * suitable fixture directories.
 *
 * This shim remaps those paths before rpcd opens, globs, or stats them.
 * Set UBUS_TESTKIT_PATH_MAP to comma-separated prefix pairs:
 *
 *     UBUS_TESTKIT_PATH_MAP=/from=/to,/other=/elsewhere
 *
 * The first matching pair wins. Only paths matching a configured prefix are
 * redirected; every other operation passes through unchanged. The shim is
 * therefore disabled entirely when the variable is unset.
 *
 * The pinned `.dep/bin/rpcd` may use the old glibc `__xstat` ABI, while
 * rebuilt versions may use direct `stat`, `statx`, or `*at` calls. Cover
 * both forms so the daemon runs unchanged against the test fixtures.
 */

#define _GNU_SOURCE
#include <dlfcn.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <glob.h>
#include <limits.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

/* Return PATH unchanged unless the first configured prefix matches it. */
static const char *rewrite(const char *path, char *buf, size_t buflen) {
    const char *map;
    const char *pair;

    if (!path || !buf || buflen == 0) {
        return path;
    }
    map = getenv("UBUS_TESTKIT_PATH_MAP");
    if (!map || !*map) {
        return path;
    }

    pair = map;
    while (*pair) {
        const char *end = strchr(pair, ',');
        const char *equals;
        size_t pair_len;
        size_t from_len;
        size_t to_len;
        size_t rest_len;

        if (!end) {
            end = pair + strlen(pair);
        }
        pair_len = (size_t)(end - pair);
        equals = memchr(pair, '=', pair_len);
        if (equals) {
            from_len = (size_t)(equals - pair);
            to_len = pair_len - from_len - 1;
            if (from_len != 0 && strncmp(path, pair, from_len) == 0 &&
                (path[from_len] == '\0' || path[from_len] == '/' ||
                 pair[from_len - 1] == '/')) {
                rest_len = strlen(path + from_len);
                if (to_len + rest_len + 1 <= buflen) {
                    memcpy(buf, equals + 1, to_len);
                    memcpy(buf + to_len, path + from_len, rest_len + 1);
                    return buf;
                }
                return path;
            }
        }
        if (*end == '\0') {
            break;
        }
        pair = end + 1;
    }
    return path;
}

static int open_needs_mode(int flags) {
    return (flags & O_CREAT) || (flags & O_TMPFILE) == O_TMPFILE;
}

/* rpcd file.write already fsyncs its opened file, then invokes the process-
 * global sync(). The latter waits on every unrelated filesystem on the CI
 * host, defeating fixture isolation and occasionally exceeding the RPC
 * deadline. Opt in only for rpcd's private root: preserve the real fsync and
 * its result, and omit a subsequent global flush only after that fsync
 * succeeded on a descriptor inside the fixture. Other sync calls pass through.
 */
static int private_file_synced;

int fsync(int fd) {
    static int (*real)(int);
    if (!real) real = dlsym(RTLD_NEXT, "fsync");
    if (!real) { errno = ENOSYS; return -1; }
    int result = real(fd);
    int saved_errno = errno;
    private_file_synced = 0;
    const char *root = getenv("UBUS_TESTKIT_RPCD_SYNC_ROOT");
    if (result == 0 && root && *root) {
        char proc[64], path[PATH_MAX];
        snprintf(proc, sizeof(proc), "/proc/self/fd/%d", fd);
        ssize_t size = readlink(proc, path, sizeof(path) - 1);
        if (size >= 0 && (size_t)size < sizeof(path) - 1) {
            path[size] = '\0';
            size_t length = strlen(root);
            private_file_synced = !strncmp(path, root, length) &&
                (path[length] == '/' || path[length] == '\0');
        }
    }
    errno = saved_errno;
    return result;
}

void sync(void) {
    static void (*real)(void);
    const char *root = getenv("UBUS_TESTKIT_RPCD_SYNC_ROOT");
    if (private_file_synced && root && *root) {
        private_file_synced = 0;
        return;
    }
    private_file_synced = 0;
    if (!real) real = dlsym(RTLD_NEXT, "sync");
    if (real) real();
}

/* ---- stdio, mutations, and canonicalization used by libuci/rpcd. ----- */

#define RESOLVE_REAL(name) \
    if (!real) { \
        real = (__typeof__(real))dlsym(RTLD_NEXT, #name); \
        if (!real) { errno = ENOSYS; return -1; } \
    }

FILE *fopen(const char *path, const char *mode) {
    static FILE *(*real)(const char *, const char *);
    char buf[PATH_MAX];
    if (!real) real = dlsym(RTLD_NEXT, "fopen");
    if (!real) { errno = ENOSYS; return NULL; }
    return real(rewrite(path, buf, sizeof(buf)), mode);
}

FILE *fopen64(const char *path, const char *mode) {
    static FILE *(*real)(const char *, const char *);
    char buf[PATH_MAX];
    if (!real) real = dlsym(RTLD_NEXT, "fopen64");
    if (!real) { errno = ENOSYS; return NULL; }
    return real(rewrite(path, buf, sizeof(buf)), mode);
}

void *dlopen(const char *path, int flags) {
    static void *(*real)(const char *, int);
    char buf[PATH_MAX];
    if (!real) real = dlsym(RTLD_NEXT, "dlopen");
    if (!real) return NULL;
    return real(rewrite(path, buf, sizeof(buf)), flags);
}

int mkdir(const char *path, mode_t mode) {
    static int (*real)(const char *, mode_t);
    char buf[PATH_MAX];
    RESOLVE_REAL(mkdir);
    return real(rewrite(path, buf, sizeof(buf)), mode);
}

int mkdirat(int dirfd, const char *path, mode_t mode) {
    static int (*real)(int, const char *, mode_t);
    char buf[PATH_MAX];
    RESOLVE_REAL(mkdirat);
    return real(dirfd, rewrite(path, buf, sizeof(buf)), mode);
}

int chmod(const char *path, mode_t mode) {
    static int (*real)(const char *, mode_t);
    char buf[PATH_MAX];
    RESOLVE_REAL(chmod);
    return real(rewrite(path, buf, sizeof(buf)), mode);
}

int fchmodat(int dirfd, const char *path, mode_t mode, int flags) {
    static int (*real)(int, const char *, mode_t, int);
    char buf[PATH_MAX];
    RESOLVE_REAL(fchmodat);
    return real(dirfd, rewrite(path, buf, sizeof(buf)), mode, flags);
}

#define ONE_PATH_MUTATION(name) \
int name(const char *path) { \
    static int (*real)(const char *); \
    char buf[PATH_MAX]; \
    RESOLVE_REAL(name); \
    return real(rewrite(path, buf, sizeof(buf))); \
}

ONE_PATH_MUTATION(unlink)
ONE_PATH_MUTATION(rmdir)

int unlinkat(int dirfd, const char *path, int flags) {
    static int (*real)(int, const char *, int);
    char buf[PATH_MAX];
    RESOLVE_REAL(unlinkat);
    return real(dirfd, rewrite(path, buf, sizeof(buf)), flags);
}

int rename(const char *from, const char *to) {
    static int (*real)(const char *, const char *);
    char from_buf[PATH_MAX], to_buf[PATH_MAX];
    RESOLVE_REAL(rename);
    return real(rewrite(from, from_buf, sizeof(from_buf)),
                rewrite(to, to_buf, sizeof(to_buf)));
}

int renameat(int from_dirfd, const char *from, int to_dirfd, const char *to) {
    static int (*real)(int, const char *, int, const char *);
    char from_buf[PATH_MAX], to_buf[PATH_MAX];
    RESOLVE_REAL(renameat);
    return real(from_dirfd, rewrite(from, from_buf, sizeof(from_buf)),
                to_dirfd, rewrite(to, to_buf, sizeof(to_buf)));
}

ssize_t readlink(const char *path, char *target, size_t size) {
    static ssize_t (*real)(const char *, char *, size_t);
    char buf[PATH_MAX];
    RESOLVE_REAL(readlink);
    return real(rewrite(path, buf, sizeof(buf)), target, size);
}

ssize_t readlinkat(int dirfd, const char *path, char *target, size_t size) {
    static ssize_t (*real)(int, const char *, char *, size_t);
    char buf[PATH_MAX];
    RESOLVE_REAL(readlinkat);
    return real(dirfd, rewrite(path, buf, sizeof(buf)), target, size);
}

char *realpath(const char *path, char *resolved) {
    static char *(*real)(const char *, char *);
    char buf[PATH_MAX];
    if (!real) real = dlsym(RTLD_NEXT, "realpath");
    if (!real) { errno = ENOSYS; return NULL; }
    return real(rewrite(path, buf, sizeof(buf)), resolved);
}

/* libuci creates a temporary config beside the original and renames it.
 * Keep the caller's logical prefix (its buffer may be much shorter than
 * the redirected path), copying back only the six randomized characters.
 */
static void copy_random_suffix(char *logical, const char *actual, int suffix) {
    size_t logical_len = strlen(logical), actual_len = strlen(actual);
    size_t tail = (size_t)suffix + 6;
    if (suffix >= 0 && logical_len >= tail && actual_len >= tail)
        memcpy(logical + logical_len - tail, actual + actual_len - tail, 6);
}

#define MKSTEMP_WRAPPER(name) \
int name(char *template) { \
    static int (*real)(char *); \
    char buf[PATH_MAX]; \
    const char *mapped = rewrite(template, buf, sizeof(buf)); \
    RESOLVE_REAL(name); \
    int fd = real(mapped == template ? template : buf); \
    if (fd >= 0 && mapped != template) copy_random_suffix(template, buf, 0); \
    return fd; \
}

MKSTEMP_WRAPPER(mkstemp)
MKSTEMP_WRAPPER(mkstemp64)

#define MKSTEMPS_WRAPPER(name) \
int name(char *template, int suffix) { \
    static int (*real)(char *, int); \
    char buf[PATH_MAX]; \
    const char *mapped = rewrite(template, buf, sizeof(buf)); \
    RESOLVE_REAL(name); \
    int fd = real(mapped == template ? template : buf, suffix); \
    if (fd >= 0 && mapped != template) copy_random_suffix(template, buf, suffix); \
    return fd; \
}

MKSTEMPS_WRAPPER(mkstemps)
MKSTEMPS_WRAPPER(mkstemps64)

int mkostemp(char *template, int flags) {
    static int (*real)(char *, int);
    char buf[PATH_MAX];
    const char *mapped = rewrite(template, buf, sizeof(buf));
    RESOLVE_REAL(mkostemp);
    int fd = real(mapped == template ? template : buf, flags);
    if (fd >= 0 && mapped != template) copy_random_suffix(template, buf, 0);
    return fd;
}

#undef RESOLVE_REAL

/* ---- opens and directory discovery. ---------------------------------- */

typedef int (*real_open_t)(const char *path, int flags, ...);
typedef int (*real_openat_t)(int dirfd, const char *path, int flags, ...);
typedef DIR *(*real_opendir_t)(const char *name);
typedef int (*real_glob_t)(const char *pattern, int flags,
                           int (*errfunc)(const char *epath, int eerrno),
                           glob_t *pglob);
typedef int (*real_glob64_t)(const char *pattern, int flags,
                             int (*errfunc)(const char *epath, int eerrno),
                             glob64_t *pglob);

int open(const char *path, int flags, ...) {
    static real_open_t real;
    char buf[PATH_MAX];
    const char *rewritten = rewrite(path, buf, sizeof(buf));

    if (!real) {
        real = (real_open_t)dlsym(RTLD_NEXT, "open");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    if (open_needs_mode(flags)) {
        va_list ap;
        mode_t mode;
        va_start(ap, flags);
        mode = va_arg(ap, mode_t);
        va_end(ap);
        return real(rewritten, flags, mode);
    }
    return real(rewritten, flags);
}

int open64(const char *path, int flags, ...) {
    static real_open_t real;
    char buf[PATH_MAX];
    const char *rewritten = rewrite(path, buf, sizeof(buf));

    if (!real) {
        real = (real_open_t)dlsym(RTLD_NEXT, "open64");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    if (open_needs_mode(flags)) {
        va_list ap;
        mode_t mode;
        va_start(ap, flags);
        mode = va_arg(ap, mode_t);
        va_end(ap);
        return real(rewritten, flags, mode);
    }
    return real(rewritten, flags);
}

int openat(int dirfd, const char *path, int flags, ...) {
    static real_openat_t real;
    char buf[PATH_MAX];
    const char *rewritten = path[0] == '/'
                               ? rewrite(path, buf, sizeof(buf))
                               : path;

    if (!real) {
        real = (real_openat_t)dlsym(RTLD_NEXT, "openat");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    if (open_needs_mode(flags)) {
        va_list ap;
        mode_t mode;
        va_start(ap, flags);
        mode = va_arg(ap, mode_t);
        va_end(ap);
        return real(dirfd, rewritten, flags, mode);
    }
    return real(dirfd, rewritten, flags);
}

int openat64(int dirfd, const char *path, int flags, ...) {
    static real_openat_t real;
    char buf[PATH_MAX];
    const char *rewritten = path[0] == '/'
                               ? rewrite(path, buf, sizeof(buf))
                               : path;

    if (!real) {
        real = (real_openat_t)dlsym(RTLD_NEXT, "openat64");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    if (open_needs_mode(flags)) {
        va_list ap;
        mode_t mode;
        va_start(ap, flags);
        mode = va_arg(ap, mode_t);
        va_end(ap);
        return real(dirfd, rewritten, flags, mode);
    }
    return real(dirfd, rewritten, flags);
}

DIR *opendir(const char *name) {
    static real_opendir_t real;
    char buf[PATH_MAX];

    if (!real) {
        real = (real_opendir_t)dlsym(RTLD_NEXT, "opendir");
        if (!real) {
            errno = ENOSYS;
            return NULL;
        }
    }
    return real(rewrite(name, buf, sizeof(buf)));
}

int glob(const char *pattern, int flags,
         int (*errfunc)(const char *epath, int eerrno), glob_t *pglob) {
    static real_glob_t real;
    char buf[PATH_MAX];

    if (!real) {
        real = (real_glob_t)dlsym(RTLD_NEXT, "glob");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    return real(rewrite(pattern, buf, sizeof(buf)), flags, errfunc, pglob);
}

int glob64(const char *pattern, int flags,
           int (*errfunc)(const char *epath, int eerrno), glob64_t *pglob) {
    static real_glob64_t real;
    char buf[PATH_MAX];

    if (!real) {
        real = (real_glob64_t)dlsym(RTLD_NEXT, "glob64");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    return real(rewrite(pattern, buf, sizeof(buf)), flags, errfunc, pglob);
}

/* ---- old glibc ABI (__xstat family). --------------------------------- */

typedef int (*real_xstat_t)(int ver, const char *path, struct stat *buf);
typedef int (*real_xstat64_t)(int ver, const char *path, struct stat64 *buf);
typedef int (*real_fxstatat_t)(int ver, int dirfd, const char *path,
                               struct stat *buf, int flags);
typedef int (*real_fxstatat64_t)(int ver, int dirfd, const char *path,
                                 struct stat64 *buf, int flags);

int __xstat(int ver, const char *path, struct stat *buf) {
    static real_xstat_t real;
    char redirected[PATH_MAX];

    if (!real) {
        real = (real_xstat_t)dlsym(RTLD_NEXT, "__xstat");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    return real(ver, rewrite(path, redirected, sizeof(redirected)), buf);
}

int __lxstat(int ver, const char *path, struct stat *buf) {
    static real_xstat_t real;
    char redirected[PATH_MAX];

    if (!real) {
        real = (real_xstat_t)dlsym(RTLD_NEXT, "__lxstat");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    return real(ver, rewrite(path, redirected, sizeof(redirected)), buf);
}

int __xstat64(int ver, const char *path, struct stat64 *buf) {
    static real_xstat64_t real;
    char redirected[PATH_MAX];

    if (!real) {
        real = (real_xstat64_t)dlsym(RTLD_NEXT, "__xstat64");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    return real(ver, rewrite(path, redirected, sizeof(redirected)), buf);
}

int __lxstat64(int ver, const char *path, struct stat64 *buf) {
    static real_xstat64_t real;
    char redirected[PATH_MAX];

    if (!real) {
        real = (real_xstat64_t)dlsym(RTLD_NEXT, "__lxstat64");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    return real(ver, rewrite(path, redirected, sizeof(redirected)), buf);
}

int __fxstatat(int ver, int dirfd, const char *path, struct stat *buf,
               int flags) {
    static real_fxstatat_t real;
    char redirected[PATH_MAX];
    const char *rewritten = path && path[0] == '/'
                               ? rewrite(path, redirected, sizeof(redirected))
                               : path;

    if (!real) {
        real = (real_fxstatat_t)dlsym(RTLD_NEXT, "__fxstatat");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    return real(ver, dirfd, rewritten, buf, flags);
}

int __fxstatat64(int ver, int dirfd, const char *path, struct stat64 *buf,
                 int flags) {
    static real_fxstatat64_t real;
    char redirected[PATH_MAX];
    const char *rewritten = path && path[0] == '/'
                               ? rewrite(path, redirected, sizeof(redirected))
                               : path;

    if (!real) {
        real = (real_fxstatat64_t)dlsym(RTLD_NEXT, "__fxstatat64");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    return real(ver, dirfd, rewritten, buf, flags);
}

/* ---- newer glibc ABI (direct stat/lstat/fstatat/statx symbols). ------ */

typedef int (*real_stat_t)(const char *path, struct stat *buf);
typedef int (*real_stat64_t)(const char *path, struct stat64 *buf);
typedef int (*real_fstatat_t)(int dirfd, const char *path, struct stat *buf,
                              int flags);
typedef int (*real_fstatat64_t)(int dirfd, const char *path,
                                struct stat64 *buf, int flags);
typedef int (*real_statx_t)(int dirfd, const char *path, int flags,
                            unsigned int mask, struct statx *buf);
typedef int (*real_access_t)(const char *path, int mode);

int stat(const char *path, struct stat *buf) {
    static real_stat_t real;
    char redirected[PATH_MAX];

    if (!real) {
        real = (real_stat_t)dlsym(RTLD_NEXT, "stat");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    return real(rewrite(path, redirected, sizeof(redirected)), buf);
}

int lstat(const char *path, struct stat *buf) {
    static real_stat_t real;
    char redirected[PATH_MAX];

    if (!real) {
        real = (real_stat_t)dlsym(RTLD_NEXT, "lstat");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    return real(rewrite(path, redirected, sizeof(redirected)), buf);
}

int stat64(const char *path, struct stat64 *buf) {
    static real_stat64_t real;
    char redirected[PATH_MAX];

    if (!real) {
        real = (real_stat64_t)dlsym(RTLD_NEXT, "stat64");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    return real(rewrite(path, redirected, sizeof(redirected)), buf);
}

int lstat64(const char *path, struct stat64 *buf) {
    static real_stat64_t real;
    char redirected[PATH_MAX];

    if (!real) {
        real = (real_stat64_t)dlsym(RTLD_NEXT, "lstat64");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    return real(rewrite(path, redirected, sizeof(redirected)), buf);
}

int fstatat(int dirfd, const char *path, struct stat *buf, int flags) {
    static real_fstatat_t real;
    char redirected[PATH_MAX];
    const char *rewritten = path && path[0] == '/'
                               ? rewrite(path, redirected, sizeof(redirected))
                               : path;

    if (!real) {
        real = (real_fstatat_t)dlsym(RTLD_NEXT, "fstatat");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    return real(dirfd, rewritten, buf, flags);
}

int fstatat64(int dirfd, const char *path, struct stat64 *buf, int flags) {
    static real_fstatat64_t real;
    char redirected[PATH_MAX];
    const char *rewritten = path && path[0] == '/'
                               ? rewrite(path, redirected, sizeof(redirected))
                               : path;

    if (!real) {
        real = (real_fstatat64_t)dlsym(RTLD_NEXT, "fstatat64");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    return real(dirfd, rewritten, buf, flags);
}

int statx(int dirfd, const char *path, int flags, unsigned int mask,
          struct statx *buf) {
    static real_statx_t real;
    char redirected[PATH_MAX];
    const char *rewritten = path && path[0] == '/'
                               ? rewrite(path, redirected, sizeof(redirected))
                               : path;

    if (!real) {
        real = (real_statx_t)dlsym(RTLD_NEXT, "statx");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    return real(dirfd, rewritten, flags, mask, buf);
}

int access(const char *path, int mode) {
    static real_access_t real;
    char redirected[PATH_MAX];

    if (!real) {
        real = (real_access_t)dlsym(RTLD_NEXT, "access");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    return real(rewrite(path, redirected, sizeof(redirected)), mode);
}
