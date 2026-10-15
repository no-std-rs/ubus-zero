/*
 * LD_PRELOAD stat-family owner spoofer for upstream `ubusd -A <dir>`.
 *
 * Upstream `ubusd` only trusts ACL JSON that is owned by root: it stats
 * each `<dir>/*.json` it globs and rejects any file whose `st_uid` is
 * not 0, so a non-root user's ACL grants never load. Userland tests
 * cannot create root-owned files, which is why the ACL *allow* path was
 * deferred (docs §P4.4) — the deny paths only need the peercred shim.
 *
 * This shim closes that gap. Preloaded into the ubusd child, it rewrites
 * the `st_uid`/`st_gid` any stat-family call reports to 0 (and clears the
 * group/other write bits, which some hardened builds also require) for
 * paths under `UBUS_TESTKIT_STAT_ROOT_PREFIX`. ubusd then treats the
 * test's ACL file as root-owned, loads it, and its grants take effect.
 *
 * Only paths under the configured prefix are touched; every other stat
 * passes through untouched, so the broker's own file accesses are
 * unaffected. Disabled entirely when the env var is unset. Tests may set
 * UBUS_TESTKIT_STAT_UID/GID (decimal) or UBUS_TESTKIT_STAT_MODE (octal
 * permissions) to exercise rejected ACL ownership and modes. File type is
 * retained and these overrides apply only inside the configured prefix.
 *
 * The pinned `.dep/bin/ubusd` imports `__xstat@GLIBC_2.2.5` (the old
 * glibc ABI), so that is the primary interposer; the newer `stat`/`statx`
 * and the `l`/`64`/`at` variants are covered defensively so the shim
 * stays correct if the broker binary is rebuilt against a newer glibc.
 */

#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>

static int path_under_prefix(const char *path) {
    if (!path) {
        return 0;
    }
    const char *prefix = getenv("UBUS_TESTKIT_STAT_ROOT_PREFIX");
    if (!prefix || !*prefix) {
        return 0;
    }
    size_t n = strlen(prefix);
    return strncmp(path, prefix, n) == 0;
}

static unsigned long configured(const char *key, unsigned long fallback, int base) {
    const char *value = getenv(key);
    if (!value || !*value)
        return fallback;
    char *end;
    errno = 0;
    unsigned long parsed = strtoul(value, &end, base);
    return errno || end == value || *end ? fallback : parsed;
}

static mode_t configured_mode(mode_t mode) {
    return (mode & S_IFMT) |
        configured("UBUS_TESTKIT_STAT_MODE", mode & 07777 & ~(S_IWGRP | S_IWOTH), 8);
}

static void spoof_stat(struct stat *buf) {
    if (!buf) {
        return;
    }
    buf->st_uid = configured("UBUS_TESTKIT_STAT_UID", 0, 10);
    buf->st_gid = configured("UBUS_TESTKIT_STAT_GID", 0, 10);
    /* Some hardened ACL loaders also reject group/other-writable files. */
    buf->st_mode = configured_mode(buf->st_mode);
}

static void spoof_stat64(struct stat64 *buf) {
    if (!buf) {
        return;
    }
    buf->st_uid = configured("UBUS_TESTKIT_STAT_UID", 0, 10);
    buf->st_gid = configured("UBUS_TESTKIT_STAT_GID", 0, 10);
    buf->st_mode = configured_mode(buf->st_mode);
}

/* ---- old glibc ABI (__xstat family): what the pinned ubusd imports. -- */

typedef int (*real_xstat_t)(int ver, const char *path, struct stat *buf);
typedef int (*real_xstat64_t)(int ver, const char *path, struct stat64 *buf);
typedef int (*real_fxstatat_t)(int ver, int dirfd, const char *path,
                               struct stat *buf, int flags);
typedef int (*real_fxstatat64_t)(int ver, int dirfd, const char *path,
                                 struct stat64 *buf, int flags);

int __xstat(int ver, const char *path, struct stat *buf) {
    static real_xstat_t real;
    if (!real) {
        real = (real_xstat_t)dlsym(RTLD_NEXT, "__xstat");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    int rc = real(ver, path, buf);
    if (rc == 0 && path_under_prefix(path)) {
        spoof_stat(buf);
    }
    return rc;
}

int __xstat64(int ver, const char *path, struct stat64 *buf) {
    static real_xstat64_t real;
    if (!real) {
        real = (real_xstat64_t)dlsym(RTLD_NEXT, "__xstat64");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    int rc = real(ver, path, buf);
    if (rc == 0 && path_under_prefix(path)) {
        spoof_stat64(buf);
    }
    return rc;
}

int __lxstat(int ver, const char *path, struct stat *buf) {
    static real_xstat_t real;
    if (!real) {
        real = (real_xstat_t)dlsym(RTLD_NEXT, "__lxstat");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    int rc = real(ver, path, buf);
    if (rc == 0 && path_under_prefix(path)) {
        spoof_stat(buf);
    }
    return rc;
}

int __lxstat64(int ver, const char *path, struct stat64 *buf) {
    static real_xstat64_t real;
    if (!real) {
        real = (real_xstat64_t)dlsym(RTLD_NEXT, "__lxstat64");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    int rc = real(ver, path, buf);
    if (rc == 0 && path_under_prefix(path)) {
        spoof_stat64(buf);
    }
    return rc;
}

int __fxstatat(int ver, int dirfd, const char *path, struct stat *buf,
               int flags) {
    static real_fxstatat_t real;
    if (!real) {
        real = (real_fxstatat_t)dlsym(RTLD_NEXT, "__fxstatat");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    int rc = real(ver, dirfd, path, buf, flags);
    if (rc == 0 && path_under_prefix(path)) {
        spoof_stat(buf);
    }
    return rc;
}

int __fxstatat64(int ver, int dirfd, const char *path, struct stat64 *buf,
                 int flags) {
    static real_fxstatat64_t real;
    if (!real) {
        real = (real_fxstatat64_t)dlsym(RTLD_NEXT, "__fxstatat64");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    int rc = real(ver, dirfd, path, buf, flags);
    if (rc == 0 && path_under_prefix(path)) {
        spoof_stat64(buf);
    }
    return rc;
}

/* ---- newer glibc ABI (direct stat/lstat/fstatat/statx symbols). ------ */

typedef int (*real_stat_t)(const char *path, struct stat *buf);
typedef int (*real_statx_t)(int dirfd, const char *path, int flags,
                            unsigned int mask, struct statx *buf);

int stat(const char *path, struct stat *buf) {
    static real_stat_t real;
    if (!real) {
        real = (real_stat_t)dlsym(RTLD_NEXT, "stat");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    int rc = real(path, buf);
    if (rc == 0 && path_under_prefix(path)) {
        spoof_stat(buf);
    }
    return rc;
}

int lstat(const char *path, struct stat *buf) {
    static real_stat_t real;
    if (!real) {
        real = (real_stat_t)dlsym(RTLD_NEXT, "lstat");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    int rc = real(path, buf);
    if (rc == 0 && path_under_prefix(path)) {
        spoof_stat(buf);
    }
    return rc;
}

int statx(int dirfd, const char *path, int flags, unsigned int mask,
          struct statx *buf) {
    static real_statx_t real;
    if (!real) {
        real = (real_statx_t)dlsym(RTLD_NEXT, "statx");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    int rc = real(dirfd, path, flags, mask, buf);
    if (rc == 0 && buf && path_under_prefix(path)) {
        buf->stx_uid = configured("UBUS_TESTKIT_STAT_UID", 0, 10);
        buf->stx_gid = configured("UBUS_TESTKIT_STAT_GID", 0, 10);
        buf->stx_mode = configured_mode(buf->stx_mode);
    }
    return rc;
}
