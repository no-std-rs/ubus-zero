/*
 * Order pinned upstream rpcd file.exec pipe EOF after child reaping.
 *
 * file.c's stream callbacks schedule a successful deferred reply as soon as
 * stdout/stderr reach EOF. Its SIGCHLD callback separately stores the child's
 * wait status. Under concurrency EOF can win and publish the zero-initialized
 * status instead of the real exit code. Keep the real SIGCHLD callback and
 * status intact: defer only FIFO EOF until that callback reaps the child.
 *
 * This shim is appended only to rpcd's child environment. It is disabled
 * unless UBUS_TESTKIT_RPCD_EXEC_WAIT_EOF=1, never consumes a wait status, and
 * leaves socket/file reads and all non-EOF pipe reads unchanged. Track each
 * pipe's child so unrelated concurrent execs do not hold back its EOF. libubox's
 * process callback polls the streams again after waitpid, so returning EAGAIN
 * before that callback cannot lose the final pipe edge.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

struct pipe_child {
    int fd;
    pid_t pid;
    struct pipe_child *next;
};

/* rpcd has a single event-loop thread. After fork the child gets its own
 * copy, and exec resets it. No state crosses test processes or daemons.
 */
static struct pipe_child *pipes;

static int enabled(void) {
    const char *value = getenv("UBUS_TESTKIT_RPCD_EXEC_WAIT_EOF");
    return value && !strcmp(value, "1");
}

static int track_pipe(int fds[2], int result) {
    if (result || !enabled()) return result;
    struct pipe_child *entry = malloc(sizeof(*entry));
    if (!entry) {
        close(fds[0]);
        close(fds[1]);
        errno = ENOMEM;
        return -1;
    }
    *entry = (struct pipe_child){ .fd = fds[0], .next = pipes };
    pipes = entry;
    return result;
}

int pipe(int fds[2]) {
    static int (*real)(int [2]);
    if (!real) real = dlsym(RTLD_NEXT, "pipe");
    if (!real) { errno = ENOSYS; return -1; }
    return track_pipe(fds, real(fds));
}

int pipe2(int fds[2], int flags) {
    static int (*real)(int [2], int);
    if (!real) real = dlsym(RTLD_NEXT, "pipe2");
    if (!real) { errno = ENOSYS; return -1; }
    return track_pipe(fds, real(fds, flags));
}

pid_t fork(void) {
    static pid_t (*real)(void);
    if (!real) real = dlsym(RTLD_NEXT, "fork");
    if (!real) { errno = ENOSYS; return -1; }
    pid_t child = real();
    if (child > 0) {
        for (struct pipe_child *entry = pipes; entry; entry = entry->next)
            if (!entry->pid) entry->pid = child;
    }
    return child;
}

int close(int fd) {
    static int (*real)(int);
    if (!real) real = dlsym(RTLD_NEXT, "close");
    if (!real) { errno = ENOSYS; return -1; }
    struct pipe_child **previous = &pipes;
    while (*previous) {
        struct pipe_child *entry = *previous;
        if (entry->fd == fd) {
            *previous = entry->next;
            free(entry);
            break;
        }
        previous = &entry->next;
    }
    return real(fd);
}

ssize_t read(int fd, void *buf, size_t count) {
    static ssize_t (*real_read)(int, void *, size_t);
    if (!real_read) real_read = dlsym(RTLD_NEXT, "read");
    if (!real_read) { errno = ENOSYS; return -1; }

    ssize_t result = real_read(fd, buf, count);
    if (result != 0 || count == 0 || !enabled())
        return result;

    int saved_errno = errno;
    pid_t owner = 0;
    for (struct pipe_child *entry = pipes; entry; entry = entry->next)
        if (entry->fd == fd) { owner = entry->pid; break; }
    struct stat st;
    if (owner > 0 && fstat(fd, &st) == 0 && S_ISFIFO(st.st_mode)) {
        siginfo_t child;
        int status;
        do {
            memset(&child, 0, sizeof(child));
            status = waitid(P_PID, owner, &child, WEXITED | WNOWAIT | WNOHANG);
        } while (status < 0 && errno == EINTR);

        /* Success with si_pid == 0 means a child is still running. A
         * nonzero si_pid means its status is ready but not yet consumed by
         * rpcd's callback. Both require deferring EOF. ECHILD means the
         * normal callback has already reaped this child; allow EOF then.
         */
        if (status == 0) {
            errno = EAGAIN;
            return -1;
        }
    }
    errno = saved_errno;
    return result;
}
