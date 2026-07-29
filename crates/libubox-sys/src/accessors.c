/*
 * Tiny C shim that re-exports libubox's inline helpers and opaque
 * field accessors as real linkable symbols. `blob_data`, `blob_len`,
 * and `blobmsg_add_string` are inline-only in the upstream headers,
 * so a Rust `extern "C"` reference to them resolves to nothing. Keep
 * this file minimal — only the helpers our FFI currently needs.
 *
 * Compiled by libubox-sys/build.rs into an object that gets linked
 * into the consumer crate via `cargo:rustc-link-arg=`.
 */

#include <libubox/blob.h>
#include <libubox/blobmsg.h>

void *ubus_zero_blob_data(const struct blob_attr *attr) {
    return blob_data(attr);
}

unsigned int ubus_zero_blob_len(const struct blob_attr *attr) {
    return (unsigned int) blob_len(attr);
}

struct blob_attr *ubus_zero_blob_buf_head(const struct blob_buf *buf) {
    return buf->head;
}

int ubus_zero_blobmsg_add_string(struct blob_buf *buf, const char *name, const char *string) {
    return blobmsg_add_string(buf, name, string);
}

#ifdef UBUS_ZERO_HAVE_BLOBMSG_JSON
#include <stdlib.h>
#include <libubox/blobmsg_json.h>

/* `blobmsg_format_json` is a static inline over blobmsg_format_json_with_cb. */
char *ubus_zero_blobmsg_format_json(struct blob_attr *attr, bool list) {
    return blobmsg_format_json(attr, list);
}

/* json-c/libubox return malloc'd strings; Rust must free them with the same
 * allocator that produced them. */
void ubus_zero_free(void *ptr) {
    free(ptr);
}
#endif
