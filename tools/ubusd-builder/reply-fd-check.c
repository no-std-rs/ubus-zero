/* Reuse a reply fd immediately after libubus closes it. A second close must
 * never invalidate this new owner, even when the fd number is identical. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <libubus.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>

static int watched_fd = -1;
static int replacement_fd = -1;
static int watched_closes;

#define CHECK(condition, message) do { \
    if (!(condition)) { fprintf(stderr, "%s\n", message); exit(1); } \
} while (0)

/* Interpose only this probe's libubus calls; syscall avoids recursive close. */
int close(int fd)
{
    int result = syscall(SYS_close, fd);
    int saved_errno = errno;
    if (fd == watched_fd && ++watched_closes == 1) {
        CHECK(syscall(SYS_dup3, replacement_fd, fd, O_CLOEXEC) == fd,
              "could not install the replacement fd");
    }
    errno = saved_errno;
    return result;
}

int main(void)
{
    int sockets[2], payload[2], replacement[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) == 0, "socketpair");
    CHECK(pipe(payload) == 0 && pipe(replacement) == 0, "pipes");
    struct timeval timeout = { .tv_sec = 2 };
    CHECK(setsockopt(sockets[1], SOL_SOCKET, SO_RCVTIMEO,
                     &timeout, sizeof(timeout)) == 0, "receive timeout");

    struct ubus_context context = {0};
    struct ubus_request_data request = {0};
    context.sock.fd = sockets[0];
    request.fd = watched_fd = payload[1];
    replacement_fd = replacement[1];
    ubus_complete_deferred_request(&context, &request, UBUS_STATUS_OK);
    CHECK(fcntl(watched_fd, F_GETFD) >= 0,
          "libubus closed the replacement owner of its reply fd number");
    CHECK(watched_closes == 1, "reply fd must be closed exactly once");

    char bytes[512];
    struct iovec iov = { .iov_base = bytes, .iov_len = sizeof(bytes) };
    union {
        struct cmsghdr alignment;
        char bytes[CMSG_SPACE(sizeof(int))];
    } control;
    struct msghdr message = {
        .msg_iov = &iov, .msg_iovlen = 1,
        .msg_control = control.bytes, .msg_controllen = sizeof(control.bytes),
    };
    CHECK(recvmsg(sockets[1], &message, MSG_CMSG_CLOEXEC) > 0, "reply recvmsg");
    struct cmsghdr *rights = CMSG_FIRSTHDR(&message);
    CHECK(!(message.msg_flags & MSG_CTRUNC) && rights
          && rights->cmsg_level == SOL_SOCKET && rights->cmsg_type == SCM_RIGHTS
          && rights->cmsg_len == CMSG_LEN(sizeof(int)), "reply SCM_RIGHTS");
    int received;
    memcpy(&received, CMSG_DATA(rights), sizeof(received));
    char marker;
    CHECK(write(received, "p", 1) == 1 && read(payload[0], &marker, 1) == 1
          && marker == 'p', "transferred reply fd must remain usable");
    CHECK(write(watched_fd, "r", 1) == 1 && read(replacement[0], &marker, 1) == 1
          && marker == 'r', "replacement fd must remain usable");
    puts("Reply fd transferred once; reused descriptor remains valid.");
    return 0;
}
