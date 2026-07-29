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
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

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
    return (flags & O_CREAT) || (flags & O_TMPFILE);
}

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
    const char *rewritten = path && path[0] == '/'
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
    const char *rewritten = path && path[0] == '/'
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
