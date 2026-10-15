/*
 * Configurable NSS identity spoofer for deterministic `ubusd` ACL tests.
 *
 * Upstream ubusd resolves peer uid/gid to names with getpwuid/getgrgid
 * immediately after accept, and resolves ACL-file user/group names with
 * getpwnam/getgrnam while loading the file. ubusd-rs follows the same path
 * through libc's getpwuid_r/getgrgid_r. A fixed synthetic test identity
 * therefore needs both directions interposed consistently on both brokers.
 *
 * Controls:
 *   UBUS_TESTKIT_NSS_USERS=uid:name[,uid:name...]
 *       Synthetic passwd entries. Their primary gid mirrors the uid.
 *   UBUS_TESTKIT_NSS_GROUPS=gid:name[,gid:name...]
 *       Synthetic group entries.
 *   UBUS_TESTKIT_NSS_FAIL_UIDS=uid[,uid...]
 *   UBUS_TESTKIT_NSS_FAIL_GIDS=gid[,gid...]
 *       Lookups for these identities fail. FAIL takes precedence over a
 *       matching synthetic entry in either lookup direction.
 *   UBUS_TESTKIT_NSS_EXTRA_GIDS=gid[ gid...]
 *       Replace SO_PEERGROUPS and Groups: in /proc/<synthetic-peer-pid>/status.
 *       The proc fixture requires UBUS_TESTKIT_PEERCRED=uid:gid:pid. Group
 *       queries preserve the full configured list and real buffer limits.
 *   UBUS_TESTKIT_NSS_PROC_FAIL=1
 *       Make the synthetic status-file read/group query fail instead.
 *
 * Entries are parsed for every call, with static storage only: this keeps
 * test configuration child-local and avoids constructors or allocation. If
 * every control is unset, every interposer is a strict RTLD_NEXT pass-through.
 */

#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <grp.h>
#include <pwd.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/types.h>

#define NSS_NAME_MAX 64

typedef struct passwd *(*real_getpwuid_t)(uid_t uid);
typedef struct group *(*real_getgrgid_t)(gid_t gid);
typedef struct passwd *(*real_getpwnam_t)(const char *name);
typedef struct group *(*real_getgrnam_t)(const char *name);
typedef int (*real_getpwuid_r_t)(uid_t uid, struct passwd *pwd, char *buf,
                                 size_t buflen, struct passwd **result);
typedef int (*real_getgrgid_r_t)(gid_t gid, struct group *grp, char *buf,
                                 size_t buflen, struct group **result);

static struct passwd spoof_passwd;
static struct group spoof_group;
static char spoof_passwd_name[NSS_NAME_MAX + 1];
static char spoof_group_name[NSS_NAME_MAX + 1];
static char *nullvec[1] = {NULL};

int getsockopt(int sockfd, int level, int optname, void *optval, socklen_t *optlen) {
    static int (*real)(int, int, int, void *, socklen_t *);
    if (!real)
        real = dlsym(RTLD_NEXT, "getsockopt");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
#ifdef SO_PEERGROUPS
    const char *extra = getenv("UBUS_TESTKIT_NSS_EXTRA_GIDS");
    if (level == SOL_SOCKET && optname == SO_PEERGROUPS && extra) {
        int saved_errno = errno;
        // Validate the descriptor through the next interposer/libc instead
        // of fabricating success for a closed fd or a regular file.
        int socket_type;
        socklen_t type_len = sizeof(socket_type);
        if (real(sockfd, SOL_SOCKET, SO_TYPE, &socket_type, &type_len))
            return -1;
        if (!optlen) {
            errno = EFAULT;
            return -1;
        }
        const char *fail = getenv("UBUS_TESTKIT_NSS_PROC_FAIL");
        if (fail && strcmp(fail, "1") == 0) {
            errno = ENODATA;
            return -1;
        }
        // Match the proc fixture's strtoul prefix parsing, including stopping
        // at the first non-numeric token. Do not inherit its daemon-side
        // 64-group cap: newer upstream must observe ERANGE for longer lists.
        const char *cursor = extra;
        size_t count = 0;
        while (*cursor) {
            char *end;
            strtoul(cursor, &end, 10);
            if (end == cursor)
                break;
            count++;
            cursor = end;
        }
        socklen_t required = (socklen_t)(count * sizeof(gid_t));
        socklen_t available = *optlen;
        if (available < required) {
            *optlen = required;
            errno = ERANGE;
            return -1;
        }
        if (required && !optval) {
            errno = EFAULT;
            return -1;
        }
        cursor = extra;
        for (size_t i = 0; i < count; i++) {
            char *end;
            gid_t gid = (gid_t)strtoul(cursor, &end, 10);
            memcpy((char *)optval + i * sizeof(gid), &gid, sizeof(gid));
            cursor = end;
        }
        *optlen = required;
        errno = saved_errno;
        return 0;
    }
#endif
    return real(sockfd, level, optname, optval, optlen);
}

/* Keep supplementary-group fixtures private to each broker child. No real
 * /proc file, host account, or process-global parent environment is changed. */
FILE *fopen(const char *path, const char *mode) {
    static FILE *(*real)(const char *, const char *);
    const char *extra = getenv("UBUS_TESTKIT_NSS_EXTRA_GIDS");
    const char *fail = getenv("UBUS_TESTKIT_NSS_PROC_FAIL");
    const char *peer = getenv("UBUS_TESTKIT_PEERCRED");
    long pid;
    char expected[64];
    if ((extra || fail) && peer && sscanf(peer, "%*u:%*u:%ld", &pid) == 1) {
        snprintf(expected, sizeof(expected), "/proc/%ld/status", pid);
        if (path && strcmp(path, expected) == 0 && strcmp(mode, "r") == 0) {
            if (fail && strcmp(fail, "1") == 0) {
                errno = ENOENT;
                return NULL;
            }
            FILE *stream = tmpfile();
            if (!stream)
                return NULL;
            fprintf(stream, "Name:\tubus-test-peer\nGroups:\t%s\n", extra ? extra : "");
            rewind(stream);
            return stream;
        }
    }
    if (!real)
        real = dlsym(RTLD_NEXT, "fopen");
    if (!real) {
        errno = ENOSYS;
        return NULL;
    }
    return real(path, mode);
}

static int parse_ulong(const char *raw, unsigned long *value) {
    char *end = NULL;

    if (!raw || !*raw || !value) {
        return 0;
    }
    errno = 0;
    unsigned long parsed = strtoul(raw, &end, 10);
    if (errno || end == raw || *end != '\0') {
        return 0;
    }
    *value = parsed;
    return 1;
}

static int parse_list_entry(const char *start, size_t len, unsigned long *value) {
    char buf[64];

    if (!start || len == 0 || len >= sizeof(buf)) {
        return 0;
    }
    memcpy(buf, start, len);
    buf[len] = '\0';
    return parse_ulong(buf, value);
}

static int id_is_listed(const char *env_name, unsigned long wanted) {
    const char *list = getenv(env_name);

    if (!list || !*list) {
        return 0;
    }
    const char *entry = list;
    for (const char *p = list;; p++) {
        if (*p != ',' && *p != '\0') {
            continue;
        }
        unsigned long value;
        if (parse_list_entry(entry, (size_t)(p - entry), &value) &&
            value == wanted) {
            return 1;
        }
        if (*p == '\0') {
            return 0;
        }
        entry = p + 1;
    }
}

static int parse_mapping_entry(const char *start, size_t len,
                               unsigned long *id, char name[NSS_NAME_MAX + 1]) {
    char buf[128];
    char *end = NULL;
    size_t name_len;

    if (!start || len == 0 || len >= sizeof(buf) || !id || !name) {
        return 0;
    }
    memcpy(buf, start, len);
    buf[len] = '\0';
    errno = 0;
    unsigned long parsed = strtoul(buf, &end, 10);
    if (errno || end == buf || *end != ':') {
        return 0;
    }
    name_len = strlen(end + 1);
    if (name_len == 0 || name_len > NSS_NAME_MAX) {
        return 0;
    }
    memcpy(name, end + 1, name_len + 1);
    *id = parsed;
    return 1;
}

static int find_mapping_by_id(const char *env_name, unsigned long wanted,
                              char name[NSS_NAME_MAX + 1]) {
    const char *list = getenv(env_name);

    if (!list || !*list) {
        return 0;
    }
    const char *entry = list;
    for (const char *p = list;; p++) {
        if (*p != ',' && *p != '\0') {
            continue;
        }
        unsigned long id;
        char parsed_name[NSS_NAME_MAX + 1];
        if (parse_mapping_entry(entry, (size_t)(p - entry), &id,
                                parsed_name) &&
            id == wanted) {
            memcpy(name, parsed_name, sizeof(parsed_name));
            return 1;
        }
        if (*p == '\0') {
            return 0;
        }
        entry = p + 1;
    }
}

static int find_mapping_by_name(const char *env_name, const char *wanted,
                                unsigned long *id,
                                char name[NSS_NAME_MAX + 1]) {
    const char *list = getenv(env_name);

    if (!list || !*list || !wanted || !id || !name) {
        return 0;
    }
    const char *entry = list;
    for (const char *p = list;; p++) {
        if (*p != ',' && *p != '\0') {
            continue;
        }
        unsigned long parsed_id;
        char parsed_name[NSS_NAME_MAX + 1];
        if (parse_mapping_entry(entry, (size_t)(p - entry), &parsed_id,
                                parsed_name) &&
            strcmp(parsed_name, wanted) == 0) {
            *id = parsed_id;
            memcpy(name, parsed_name, sizeof(parsed_name));
            return 1;
        }
        if (*p == '\0') {
            return 0;
        }
        entry = p + 1;
    }
}

static struct passwd *make_passwd(uid_t uid, const char *name) {
    strcpy(spoof_passwd_name, name);
    spoof_passwd.pw_name = spoof_passwd_name;
    spoof_passwd.pw_passwd = "x";
    spoof_passwd.pw_uid = uid;
    spoof_passwd.pw_gid = (gid_t)uid;
    spoof_passwd.pw_gecos = "";
    spoof_passwd.pw_dir = "/";
    spoof_passwd.pw_shell = "/bin/false";
    return &spoof_passwd;
}

static struct group *make_group(gid_t gid, const char *name) {
    strcpy(spoof_group_name, name);
    spoof_group.gr_name = spoof_group_name;
    spoof_group.gr_passwd = "x";
    spoof_group.gr_gid = gid;
    spoof_group.gr_mem = nullvec;
    return &spoof_group;
}

struct passwd *getpwuid(uid_t uid) {
    static real_getpwuid_t real;
    char name[NSS_NAME_MAX + 1];
    int saved_errno = errno;

    if (id_is_listed("UBUS_TESTKIT_NSS_FAIL_UIDS", (unsigned long)uid)) {
        errno = ENOENT;
        return NULL;
    }
    if (find_mapping_by_id("UBUS_TESTKIT_NSS_USERS", (unsigned long)uid,
                           name)) {
        return make_passwd(uid, name);
    }
    if (!real) {
        real = (real_getpwuid_t)dlsym(RTLD_NEXT, "getpwuid");
        if (!real) {
            errno = ENOSYS;
            return NULL;
        }
    }
    errno = saved_errno;
    return real(uid);
}

struct group *getgrgid(gid_t gid) {
    static real_getgrgid_t real;
    char name[NSS_NAME_MAX + 1];
    int saved_errno = errno;

    if (id_is_listed("UBUS_TESTKIT_NSS_FAIL_GIDS", (unsigned long)gid)) {
        errno = ENOENT;
        return NULL;
    }
    if (find_mapping_by_id("UBUS_TESTKIT_NSS_GROUPS", (unsigned long)gid,
                           name)) {
        return make_group(gid, name);
    }
    if (!real) {
        real = (real_getgrgid_t)dlsym(RTLD_NEXT, "getgrgid");
        if (!real) {
            errno = ENOSYS;
            return NULL;
        }
    }
    errno = saved_errno;
    return real(gid);
}

struct passwd *getpwnam(const char *wanted) {
    static real_getpwnam_t real;
    unsigned long uid;
    char name[NSS_NAME_MAX + 1];
    int saved_errno = errno;

    if (find_mapping_by_name("UBUS_TESTKIT_NSS_USERS", wanted, &uid, name)) {
        if (id_is_listed("UBUS_TESTKIT_NSS_FAIL_UIDS", uid)) {
            errno = ENOENT;
            return NULL;
        }
        return make_passwd((uid_t)uid, name);
    }
    if (!real) {
        real = (real_getpwnam_t)dlsym(RTLD_NEXT, "getpwnam");
        if (!real) {
            errno = ENOSYS;
            return NULL;
        }
    }
    errno = saved_errno;
    return real(wanted);
}

struct group *getgrnam(const char *wanted) {
    static real_getgrnam_t real;
    unsigned long gid;
    char name[NSS_NAME_MAX + 1];
    int saved_errno = errno;

    if (find_mapping_by_name("UBUS_TESTKIT_NSS_GROUPS", wanted, &gid,
                             name)) {
        if (id_is_listed("UBUS_TESTKIT_NSS_FAIL_GIDS", gid)) {
            errno = ENOENT;
            return NULL;
        }
        return make_group((gid_t)gid, name);
    }
    if (!real) {
        real = (real_getgrnam_t)dlsym(RTLD_NEXT, "getgrnam");
        if (!real) {
            errno = ENOSYS;
            return NULL;
        }
    }
    errno = saved_errno;
    return real(wanted);
}

int getpwuid_r(uid_t uid, struct passwd *pwd, char *buf, size_t buflen,
               struct passwd **result) {
    static real_getpwuid_r_t real;
    char name[NSS_NAME_MAX + 1];
    size_t name_len;
    int saved_errno = errno;

    if (id_is_listed("UBUS_TESTKIT_NSS_FAIL_UIDS", (unsigned long)uid)) {
        *result = NULL;
        return 0;
    }
    if (find_mapping_by_id("UBUS_TESTKIT_NSS_USERS", (unsigned long)uid,
                           name)) {
        name_len = strlen(name) + 1;
        if (buflen < name_len) {
            *result = NULL;
            return ERANGE;
        }
        memcpy(buf, name, name_len);
        pwd->pw_name = buf;
        pwd->pw_passwd = "x";
        pwd->pw_uid = uid;
        pwd->pw_gid = (gid_t)uid;
        pwd->pw_gecos = "";
        pwd->pw_dir = "/";
        pwd->pw_shell = "/bin/false";
        *result = pwd;
        return 0;
    }
    if (!real) {
        real = (real_getpwuid_r_t)dlsym(RTLD_NEXT, "getpwuid_r");
        if (!real) {
            errno = ENOSYS;
            return ENOSYS;
        }
    }
    errno = saved_errno;
    return real(uid, pwd, buf, buflen, result);
}

int getgrgid_r(gid_t gid, struct group *grp, char *buf, size_t buflen,
               struct group **result) {
    static real_getgrgid_r_t real;
    char name[NSS_NAME_MAX + 1];
    size_t name_len;
    int saved_errno = errno;

    if (id_is_listed("UBUS_TESTKIT_NSS_FAIL_GIDS", (unsigned long)gid)) {
        *result = NULL;
        return 0;
    }
    if (find_mapping_by_id("UBUS_TESTKIT_NSS_GROUPS", (unsigned long)gid,
                           name)) {
        name_len = strlen(name) + 1;
        if (buflen < name_len) {
            *result = NULL;
            return ERANGE;
        }
        memcpy(buf, name, name_len);
        grp->gr_name = buf;
        grp->gr_passwd = "x";
        grp->gr_gid = gid;
        grp->gr_mem = nullvec;
        *result = grp;
        return 0;
    }
    if (!real) {
        real = (real_getgrgid_r_t)dlsym(RTLD_NEXT, "getgrgid_r");
        if (!real) {
            errno = ENOSYS;
            return ENOSYS;
        }
    }
    errno = saved_errno;
    return real(gid, grp, buf, buflen, result);
}

int getpwnam_r(const char *wanted, struct passwd *pwd, char *buf, size_t buflen,
              struct passwd **result) {
    static int (*real)(const char *, struct passwd *, char *, size_t, struct passwd **);
    unsigned long uid;
    char name[NSS_NAME_MAX + 1];
    if (find_mapping_by_name("UBUS_TESTKIT_NSS_USERS", wanted, &uid, name))
        return getpwuid_r((uid_t)uid, pwd, buf, buflen, result);
    if (!real)
        real = dlsym(RTLD_NEXT, "getpwnam_r");
    if (!real) {
        *result = NULL;
        return ENOSYS;
    }
    return real(wanted, pwd, buf, buflen, result);
}

int getgrnam_r(const char *wanted, struct group *grp, char *buf, size_t buflen,
              struct group **result) {
    static int (*real)(const char *, struct group *, char *, size_t, struct group **);
    unsigned long gid;
    char name[NSS_NAME_MAX + 1];
    if (find_mapping_by_name("UBUS_TESTKIT_NSS_GROUPS", wanted, &gid, name))
        return getgrgid_r((gid_t)gid, grp, buf, buflen, result);
    if (!real)
        real = dlsym(RTLD_NEXT, "getgrnam_r");
    if (!real) {
        *result = NULL;
        return ENOSYS;
    }
    return real(wanted, grp, buf, buflen, result);
}
