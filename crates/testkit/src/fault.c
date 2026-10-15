/*
 * LD_PRELOAD fault shim for broker-side socket I/O paths.
 *
 * Controls:
 *   UBUS_TESTKIT_MAX_WRITE=N
 *       Cap each intercepted write/send/sendmsg/writev call on an accepted
 *       client socket to at most N bytes, forcing partial-write handling.
 *   UBUS_TESTKIT_EINTR_EVERY=K
 *       Fail every K-th intercepted client-socket call with EINTR before
 *       writing. Files, logs, self-pipes and unmarked sockets pass through.
 *   UBUS_TESTKIT_RX_EINTR_EVERY=K
 *       Fail every K-th intercepted read/recv/recvmsg call on an
 *       accepted client socket with EINTR before consuming bytes.
 *   UBUS_TESTKIT_TX_STALL_CONN=C
 *       Target the C-th accepted client (0-based accept order). See
 *       below for how the fd is associated with an accept index.
 *   UBUS_TESTKIT_TX_STALL_AFTER_BYTES=M
 *       Let M bytes flow to the targeted fd (enough for the broker's
 *       HELLO + subscribe handshake), then wedge: every subsequent
 *       write/send/sendmsg/writev to that fd returns EAGAIN forever.
 *
 * TX stall — deterministic broker backpressure without kernel-buffer
 * or scheduling dependence. Instead of shrinking SO_RCVBUF on the
 * client and hoping the broker's tx path fills at a predictable point,
 * the shim pins the exact byte at which the broker can no longer make
 * progress to a stuck subscriber. Upstream `ubusd` then exercises its
 * real shed path (drop fan-out, keep the connection) deterministically.
 *
 * Accept-index targeting: the shim never interposes `accept`. Upstream
 * `ubusd` queries `getsockopt(SOL_SOCKET, SO_PEERCRED)` exactly once
 * per accepted client, right after accept and before any write — the
 * same mechanism `uid_spoof.c` uses for PEERCRED_SEQ. Counting those
 * queries yields the accept ordinal, and the fd passed to that
 * getsockopt call is the same fd the broker later writes to, so the
 * shim records fd -> "this is accept N" at query time. It also marks
 * that fd as a client socket for TX and RX fault injection. Injection
 * is deliberately restricted to those marked fds: ACL and /proc files,
 * signal self-pipes, logs and the listener must never be poisoned. `close()` is
 * interposed to clear per-fd state so a recycled fd never inherits a
 * stale stall or client marker.
 *
 * LD_PRELOAD ordering: both this shim and uid_spoof.c interpose
 * `getsockopt`. Whichever appears first in LD_PRELOAD is called by the
 * broker; RTLD_NEXT chains to the other and then to libc. This shim's
 * interposer is a strict pass-through — it forwards to the next
 * `getsockopt`, tracks successful SO_PEERCRED queries as a side effect,
 * and never touches `optval`. uid_spoof.c owns the credential rewrite.
 * Order is therefore irrelevant as long as both forward via RTLD_NEXT.
 *
 * Uloop caveat: once the shim wedges a writable fd, upstream's uloop
 * may busy-poll it (POLLOUT keeps firing because the kernel buffer
 * really does have room; only the shim refuses). That is a CPU cost,
 * not a correctness problem — the broker still services every other
 * fd — and it is bounded by the millisecond-scale flood the stall
 * tests run. Do not leave a stall engaged across a long-lived broker.
 *
 * RX EINTR caveat: upstream's edge-triggered client callback handles a
 * negative recvmsg/read by `goto out`. If injected EINTR consumes the
 * final pending edge, that connection can stall until new bytes or EOF
 * arrive. This mirrors upstream handling and makes the control suitable
 * only for targeted use.
 *
 * This is intentionally useful for upstream `ubusd` only. `ubusd-rs`
 * sends socket bytes through rustix raw syscalls, which bypass libc
 * interposers such as this one. That gap is by design: `ubusd-rs`
 * covers the same shed point through its `testing`-gated write
 * governor seam (`Config::tx_governor`), exercised by
 * `slow_subscriber_evicted_when_tx_buf_caps` in crates/ubusd/tests/smoke.rs.
 * Cross-broker client-to-broker fragmentation is covered separately by
 * WireTap, which sits on the actual socket stream and affects both.
 */

#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <stdatomic.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

typedef ssize_t (*real_write_t)(int fd, const void *buf, size_t count);
typedef ssize_t (*real_send_t)(int sockfd, const void *buf, size_t len,
                               int flags);
typedef ssize_t (*real_sendmsg_t)(int sockfd, const struct msghdr *msg,
                                  int flags);
typedef ssize_t (*real_writev_t)(int fd, const struct iovec *iov,
                                 int iovcnt);
typedef ssize_t (*real_read_t)(int fd, void *buf, size_t count);
typedef ssize_t (*real_recv_t)(int sockfd, void *buf, size_t len, int flags);
typedef ssize_t (*real_recvmsg_t)(int sockfd, struct msghdr *msg, int flags);
typedef int (*real_getsockopt_t)(int sockfd, int level, int optname,
                                  void *optval, socklen_t *optlen);
typedef int (*real_close_t)(int fd);

static atomic_ulong call_count = 0;
static atomic_ulong rx_call_count = 0;

static size_t env_usize(const char *name) {
    const char *raw = getenv(name);
    if (!raw || !*raw) {
        return 0;
    }
    errno = 0;
    char *end = NULL;
    unsigned long value = strtoul(raw, &end, 10);
    if (errno || end == raw || *end != '\0') {
        return 0;
    }
    return (size_t)value;
}

static int should_eintr(void) {
    size_t every = env_usize("UBUS_TESTKIT_EINTR_EVERY");
    if (every == 0) {
        return 0;
    }
    unsigned long n =
        atomic_fetch_add_explicit(&call_count, 1, memory_order_relaxed) + 1;
    return n % every == 0;
}

static size_t cap_len(size_t len) {
    size_t max = env_usize("UBUS_TESTKIT_MAX_WRITE");
    if (max == 0 || len <= max) {
        return len;
    }
    return max;
}

static int cap_iov(const struct iovec *iov, int iovcnt, size_t max,
                   struct iovec *out, int *outcnt) {
    if (!iov || iovcnt < 0 || !out || !outcnt) {
        errno = EINVAL;
        return -1;
    }
    if (iovcnt == 0 || max == 0) {
        *outcnt = 0;
        return 0;
    }
    if (iovcnt > 1024) {
        errno = EINVAL;
        return -1;
    }

    size_t remaining = max;
    int n = 0;
    for (int i = 0; i < iovcnt && remaining > 0; i++) {
        out[n] = iov[i];
        if (out[n].iov_len > remaining) {
            out[n].iov_len = remaining;
        }
        remaining -= out[n].iov_len;
        n++;
    }
    *outcnt = n;
    return 0;
}

/*
 * TX-stall bookkeeping. The broker is single-threaded (uloop), so plain
 * loads/stores of the per-fd table are race-free in practice; the accept
 * counter is atomic to match uid_spoof.c and to stay correct if a shim
 * consumer ever drives the broker from more than one thread.
 */
#define STALL_MAX_FD 4096

struct fd_stall {
    int targeted;
    size_t remaining;
};

static struct fd_stall fd_stall_table[STALL_MAX_FD];
static unsigned char fd_is_client[STALL_MAX_FD];
static atomic_ulong stall_accept_count = 0;

static int is_client_socket(int fd) {
    return fd >= 0 && fd < STALL_MAX_FD && fd_is_client[fd];
}

static atomic_int stall_config_ready = 0;
static int stall_enabled = 0;
static long stall_conn_index = 0;
static size_t stall_after_bytes = 0;

static void stall_init(void) {
    if (atomic_load_explicit(&stall_config_ready, memory_order_acquire)) {
        return;
    }
    /* A racing double-init is harmless: both writers compute the same
     * values from the immutable environment. */
    const char *conn = getenv("UBUS_TESTKIT_TX_STALL_CONN");
    if (conn && *conn) {
        errno = 0;
        char *end = NULL;
        long idx = strtol(conn, &end, 10);
        if (!errno && end != conn && *end == '\0' && idx >= 0) {
            stall_conn_index = idx;
            stall_after_bytes = env_usize("UBUS_TESTKIT_TX_STALL_AFTER_BYTES");
            stall_enabled = 1;
        }
    }
    atomic_store_explicit(&stall_config_ready, 1, memory_order_release);
}

/* Decision for a single write to `fd`:
 *   0 -> not a stall target; caller proceeds normally.
 *   1 -> budget exhausted; caller returns EAGAIN.
 *   2 -> partially allowed; *cap set to the max bytes to hand to real(). */
enum { STALL_PASS = 0, STALL_BLOCK = 1, STALL_CAP = 2 };

static int stall_decide(int fd, size_t *cap) {
    if (fd < 0 || fd >= STALL_MAX_FD || !fd_stall_table[fd].targeted) {
        return STALL_PASS;
    }
    size_t rem = fd_stall_table[fd].remaining;
    if (rem == 0) {
        return STALL_BLOCK;
    }
    *cap = rem;
    return STALL_CAP;
}

static void stall_consumed(int fd, ssize_t written) {
    if (fd < 0 || fd >= STALL_MAX_FD || !fd_stall_table[fd].targeted ||
        written <= 0) {
        return;
    }
    size_t rem = fd_stall_table[fd].remaining;
    fd_stall_table[fd].remaining =
        (size_t)written >= rem ? 0 : rem - (size_t)written;
}

static int should_rx_eintr(int fd) {
    if (!is_client_socket(fd)) {
        return 0;
    }
    size_t every = env_usize("UBUS_TESTKIT_RX_EINTR_EVERY");
    if (every == 0) {
        return 0;
    }
    unsigned long n = atomic_fetch_add_explicit(&rx_call_count, 1,
                                                  memory_order_relaxed) +
                      1;
    return n % every == 0;
}

ssize_t read(int fd, void *buf, size_t count) {
    static real_read_t real;
    if (!real) {
        real = (real_read_t)dlsym(RTLD_NEXT, "read");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    if (should_rx_eintr(fd)) {
        errno = EINTR;
        return -1;
    }
    return real(fd, buf, count);
}

ssize_t recv(int sockfd, void *buf, size_t len, int flags) {
    static real_recv_t real;
    if (!real) {
        real = (real_recv_t)dlsym(RTLD_NEXT, "recv");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    if (should_rx_eintr(sockfd)) {
        errno = EINTR;
        return -1;
    }
    return real(sockfd, buf, len, flags);
}

ssize_t recvmsg(int sockfd, struct msghdr *msg, int flags) {
    static real_recvmsg_t real;
    if (!real) {
        real = (real_recvmsg_t)dlsym(RTLD_NEXT, "recvmsg");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    if (should_rx_eintr(sockfd)) {
        errno = EINTR;
        return -1;
    }
    return real(sockfd, msg, flags);
}

ssize_t write(int fd, const void *buf, size_t count) {
    static real_write_t real;
    if (!real) {
        real = (real_write_t)dlsym(RTLD_NEXT, "write");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    if (!is_client_socket(fd)) {
        return real(fd, buf, count);
    }
    size_t cap = 0;
    switch (stall_decide(fd, &cap)) {
        case STALL_BLOCK:
            errno = EAGAIN;
            return -1;
        case STALL_CAP: {
            if (should_eintr()) {
                errno = EINTR;
                return -1;
            }
            size_t n = count < cap ? count : cap;
            ssize_t r = real(fd, buf, cap_len(n));
            stall_consumed(fd, r);
            return r;
        }
        default:
            break;
    }
    if (should_eintr()) {
        errno = EINTR;
        return -1;
    }
    return real(fd, buf, cap_len(count));
}

ssize_t send(int sockfd, const void *buf, size_t len, int flags) {
    static real_send_t real;
    if (!real) {
        real = (real_send_t)dlsym(RTLD_NEXT, "send");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    if (!is_client_socket(sockfd)) {
        return real(sockfd, buf, len, flags);
    }
    size_t cap = 0;
    switch (stall_decide(sockfd, &cap)) {
        case STALL_BLOCK:
            errno = EAGAIN;
            return -1;
        case STALL_CAP: {
            if (should_eintr()) {
                errno = EINTR;
                return -1;
            }
            size_t n = len < cap ? len : cap;
            ssize_t r = real(sockfd, buf, cap_len(n), flags);
            stall_consumed(sockfd, r);
            return r;
        }
        default:
            break;
    }
    if (should_eintr()) {
        errno = EINTR;
        return -1;
    }
    return real(sockfd, buf, cap_len(len), flags);
}

ssize_t writev(int fd, const struct iovec *iov, int iovcnt) {
    static real_writev_t real;
    if (!real) {
        real = (real_writev_t)dlsym(RTLD_NEXT, "writev");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }

    if (!is_client_socket(fd)) {
        return real(fd, iov, iovcnt);
    }

    size_t stall_cap = 0;
    int decision = stall_decide(fd, &stall_cap);
    if (decision == STALL_BLOCK) {
        errno = EAGAIN;
        return -1;
    }

    size_t max = env_usize("UBUS_TESTKIT_MAX_WRITE");
    if (decision == STALL_CAP) {
        if (should_eintr()) {
            errno = EINTR;
            return -1;
        }
        size_t cap = stall_cap;
        if (max != 0 && max < cap) {
            cap = max;
        }
        struct iovec capped[1024];
        int capped_cnt = 0;
        if (cap_iov(iov, iovcnt, cap, capped, &capped_cnt) != 0) {
            return -1;
        }
        ssize_t r = real(fd, capped, capped_cnt);
        stall_consumed(fd, r);
        return r;
    }

    if (should_eintr()) {
        errno = EINTR;
        return -1;
    }
    if (max == 0) {
        return real(fd, iov, iovcnt);
    }
    struct iovec capped[1024];
    int capped_cnt = 0;
    if (cap_iov(iov, iovcnt, max, capped, &capped_cnt) != 0) {
        return -1;
    }
    return real(fd, capped, capped_cnt);
}

ssize_t sendmsg(int sockfd, const struct msghdr *msg, int flags) {
    static real_sendmsg_t real;
    if (!real) {
        real = (real_sendmsg_t)dlsym(RTLD_NEXT, "sendmsg");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }

    if (!is_client_socket(sockfd)) {
        return real(sockfd, msg, flags);
    }

    size_t stall_cap = 0;
    int decision = stall_decide(sockfd, &stall_cap);
    if (decision == STALL_BLOCK) {
        errno = EAGAIN;
        return -1;
    }

    size_t max = env_usize("UBUS_TESTKIT_MAX_WRITE");
    if (decision == STALL_CAP) {
        if (should_eintr()) {
            errno = EINTR;
            return -1;
        }
        if (!msg) {
            return real(sockfd, msg, flags);
        }
        size_t cap = stall_cap;
        if (max != 0 && max < cap) {
            cap = max;
        }
        struct iovec capped[1024];
        int capped_cnt = 0;
        if (cap_iov(msg->msg_iov, (int)msg->msg_iovlen, cap, capped,
                    &capped_cnt) != 0) {
            return -1;
        }
        struct msghdr capped_msg;
        memcpy(&capped_msg, msg, sizeof(capped_msg));
        capped_msg.msg_iov = capped;
        capped_msg.msg_iovlen = (size_t)capped_cnt;
        ssize_t r = real(sockfd, &capped_msg, flags);
        stall_consumed(sockfd, r);
        return r;
    }

    if (should_eintr()) {
        errno = EINTR;
        return -1;
    }
    if (max == 0 || !msg) {
        return real(sockfd, msg, flags);
    }
    struct iovec capped[1024];
    int capped_cnt = 0;
    if (cap_iov(msg->msg_iov, (int)msg->msg_iovlen, max, capped,
                &capped_cnt) != 0) {
        return -1;
    }
    struct msghdr capped_msg;
    memcpy(&capped_msg, msg, sizeof(capped_msg));
    capped_msg.msg_iov = capped;
    capped_msg.msg_iovlen = (size_t)capped_cnt;
    return real(sockfd, &capped_msg, flags);
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
    /* Strict pass-through: forward first, never touch optval (uid_spoof.c
     * owns the SO_PEERCRED rewrite), then track it as a side effect. */
    int rc = real(sockfd, level, optname, optval, optlen);
    stall_init();
    if (rc == 0 && level == SOL_SOCKET && optname == SO_PEERCRED) {
        if (sockfd >= 0 && sockfd < STALL_MAX_FD) {
            fd_is_client[sockfd] = 1;
        }
        if (stall_enabled) {
            unsigned long idx = atomic_fetch_add_explicit(
                &stall_accept_count, 1, memory_order_relaxed);
            if ((long)idx == stall_conn_index && sockfd >= 0 &&
                sockfd < STALL_MAX_FD) {
                fd_stall_table[sockfd].targeted = 1;
                fd_stall_table[sockfd].remaining = stall_after_bytes;
            }
        }
    }
    return rc;
}

int close(int fd) {
    static real_close_t real;
    if (!real) {
        real = (real_close_t)dlsym(RTLD_NEXT, "close");
        if (!real) {
            errno = ENOSYS;
            return -1;
        }
    }
    /* Clear per-fd state before the fd can be recycled by a later accept. */
    if (fd >= 0 && fd < STALL_MAX_FD) {
        fd_stall_table[fd].targeted = 0;
        fd_stall_table[fd].remaining = 0;
        fd_is_client[fd] = 0;
    }
    return real(fd);
}
