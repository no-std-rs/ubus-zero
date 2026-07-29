/*
 * Configurable SO_PEERCRED uid/gid/pid spoofer for `ubusd` test runs.
 *
 * Upstream `ubusd_acl_check` (ubus.git, ubusd_acl.c) denies any
 * `ADD_OBJECT(OBJPATH=…)` from a non-root client when no matching
 * ACL file is loaded — and ACL files must be root-owned, which
 * userland tests can't produce. The standard sidesteps are
 * `unshare -r` (creates a user namespace, but introduces SO_PEERCRED
 * translation quirks that leave reads parked in poll without bytes
 * ever surfacing) or installing the test as root (invasive).
 *
 * This shim is a third option: LD_PRELOAD'd into the ubusd child, it
 * overrides `getsockopt` so `SO_PEERCRED` reads return a configured
 * identity. By default that is uid=0/gid=0/pid=1, matching the original
 * root-spoof behavior: ubusd sees every client as root, the root-uid
 * ACL fast-path triggers, and `ADD_OBJECT` succeeds. pid=1 keeps
 * `ubusd_acl_load_extra_gids -> /proc/<pid>/status` coherent (init's
 * empty supplementary set is benign).
 *
 * Controls:
 *   UBUS_TESTKIT_PEERCRED=uid:gid:pid
 *       Spoof every peer as this identity. Defaults to 0:0:1.
 *   UBUS_TESTKIT_PEERCRED_SEQ=uid:gid:pid,uid:gid:pid,...
 *       Spoof identities in SO_PEERCRED query order. ubusd queries once
 *       per accepted client fd, so this maps to accept order in the
 *       harness. The last entry repeats. SEQ takes precedence over
 *       UBUS_TESTKIT_PEERCRED when both are set.
 *   UBUS_TESTKIT_PEERCRED_FAIL=1
 *       Make successful SO_PEERCRED reads fail with EINVAL after the
 *       real syscall. This pins the upstream and Rust brokers' refusal
 *       paths when peer credentials cannot be read.
 *
 * Current ubus master also calls `mkdir_sockdir()` unconditionally:
 * it tries to create the compiled-in default socket dir
 * (`/var/run/ubus`) even when `-s <path>` points the broker at a
 * test-owned socket. As a non-root user that mkdir fails with EACCES,
 * main exits 255, and stderr stays empty. Spoofing mkdir success is
 * safe for this harness because the default dir is never used after
 * the explicit `-s` override.
 *
 * Scope: only `getsockopt(_, SOL_SOCKET, SO_PEERCRED, …)` is
 * rewritten, and `mkdir` failures other than EEXIST are reported as
 * success. All other syscalls pass through. ubusd's own `getuid()` is
 * irrelevant — the broker doesn't gate on its own uid, only on
 * connecting client uids surfaced via SO_PEERCRED.
 */

#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>

typedef int (*real_getsockopt_t)(int sockfd, int level, int optname,
                                 void *optval, socklen_t *optlen);
typedef int (*real_mkdir_t)(const char *path, mode_t mode);

struct spoof_cred {
    uid_t uid;
    gid_t gid;
    pid_t pid;
};

static atomic_ulong peercred_query_count = 0;

static int parse_triplet(const char *raw, struct spoof_cred *out) {
    if (!raw || !*raw || !out) {
        return 0;
    }

    errno = 0;
    char *end = NULL;
    unsigned long uid = strtoul(raw, &end, 10);
    if (errno || end == raw || *end != ':') {
        return 0;
    }
    raw = end + 1;

    errno = 0;
    unsigned long gid = strtoul(raw, &end, 10);
    if (errno || end == raw || *end != ':') {
        return 0;
    }
    raw = end + 1;

    errno = 0;
    long pid = strtol(raw, &end, 10);
    if (errno || end == raw || *end != '\0') {
        return 0;
    }

    out->uid = (uid_t)uid;
    out->gid = (gid_t)gid;
    out->pid = (pid_t)pid;
    return 1;
}

static int parse_seq_entry(const char *start, size_t len,
                           struct spoof_cred *out) {
    if (!start || len == 0 || !out) {
        return 0;
    }
    char buf[128];
    if (len >= sizeof(buf)) {
        return 0;
    }
    memcpy(buf, start, len);
    buf[len] = '\0';
    return parse_triplet(buf, out);
}

static struct spoof_cred selected_cred(void) {
    struct spoof_cred cred = {0, 0, 1};
    const char *seq = getenv("UBUS_TESTKIT_PEERCRED_SEQ");
    if (seq && *seq) {
        unsigned long idx = atomic_fetch_add_explicit(
            &peercred_query_count, 1, memory_order_relaxed);
        unsigned long cur = 0;
        const char *entry = seq;
        struct spoof_cred last = cred;
        int have_last = 0;

        for (const char *p = seq;; p++) {
            if (*p != ',' && *p != '\0') {
                continue;
            }
            struct spoof_cred parsed;
            if (parse_seq_entry(entry, (size_t)(p - entry), &parsed)) {
                last = parsed;
                have_last = 1;
                if (cur == idx) {
                    return parsed;
                }
                cur++;
            }
            if (*p == '\0') {
                return have_last ? last : cred;
            }
            entry = p + 1;
        }
    }

    const char *single = getenv("UBUS_TESTKIT_PEERCRED");
    if (single && *single) {
        (void)parse_triplet(single, &cred);
    }
    return cred;
}

int getsockopt(int sockfd, int level, int optname, void *optval,
               socklen_t *optlen) {
    static real_getsockopt_t real;
    if (!real) {
        real = (real_getsockopt_t)dlsym(RTLD_NEXT, "getsockopt");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }

    int rc = real(sockfd, level, optname, optval, optlen);
    if (rc != 0) {
        return rc;
    }
    const char *fail = getenv("UBUS_TESTKIT_PEERCRED_FAIL");
    if (level == SOL_SOCKET && optname == SO_PEERCRED && fail && *fail) {
        errno = EINVAL;
        return -1;
    }
    if (level == SOL_SOCKET && optname == SO_PEERCRED && optval &&
        optlen && *optlen >= sizeof(struct ucred)) {
        struct ucred *uc = (struct ucred *)optval;
        struct spoof_cred cred = selected_cred();
        uc->uid = cred.uid;
        uc->gid = cred.gid;
        uc->pid = cred.pid;
    }
    return rc;
}

int mkdir(const char *path, mode_t mode) {
    static real_mkdir_t real;
    if (!real) {
        real = (real_mkdir_t)dlsym(RTLD_NEXT, "mkdir");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }

    int rc = real(path, mode);
    if (rc == 0 || errno == EEXIST) {
        return rc;
    }
    return 0;
}
