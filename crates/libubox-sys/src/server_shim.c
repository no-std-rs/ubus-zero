/*
 * C-owned libubus server-role objects and inline helper re-exports.
 *
 * `ubus_object`, `ubus_subscriber`, and `ubus_event_handler` embed AVL/list
 * nodes, so Rust deliberately treats them as opaque.  These wrappers keep
 * their allocation and field initialization on the C side while forwarding
 * callbacks into Rust through one context pointer per role.
 */

#include <stdlib.h>
#include <string.h>

#include <libubus.h>

typedef int (*ubus_zero_testkit_method_cb)(void *rust_ctx, const char *method,
		struct blob_attr *msg, struct ubus_request_data *req);

/* object host: n_methods method names sharing one Rust dispatch fn */
struct ubus_zero_testkit_object {
	struct ubus_object object;
	ubus_zero_testkit_method_cb cb;
	void *rust_ctx;
	struct ubus_object_type *type;
	struct ubus_method *methods;
};

static int ubus_zero_testkit_object_handler(struct ubus_context *ctx,
		struct ubus_object *object, struct ubus_request_data *req,
		const char *method, struct blob_attr *msg)
{
	struct ubus_zero_testkit_object *wrapper =
		(struct ubus_zero_testkit_object *)object;

	(void)ctx;
	if (!wrapper->cb)
		return UBUS_STATUS_UNKNOWN_ERROR;
	return wrapper->cb(wrapper->rust_ctx, method, msg, req);
}

struct ubus_object *ubus_zero_testkit_object_create(const char *name,
		const char *const *method_names, int n_methods,
		ubus_zero_testkit_method_cb cb, void *rust_ctx)
{
	struct ubus_zero_testkit_object *wrapper;
	int i;

	if (!name || n_methods < 0 || (n_methods && !method_names))
		return NULL;

	wrapper = calloc(1, sizeof(*wrapper));
	if (!wrapper)
		return NULL;

	wrapper->object.name = strdup(name);
	if (!wrapper->object.name)
		goto fail_wrapper;

	wrapper->type = calloc(1, sizeof(*wrapper->type));
	if (!wrapper->type)
		goto fail_name;
	wrapper->type->name = wrapper->object.name;

	if (n_methods) {
		wrapper->methods = calloc((size_t)n_methods, sizeof(*wrapper->methods));
		if (!wrapper->methods)
			goto fail_type;
		for (i = 0; i < n_methods; i++) {
			if (!method_names[i])
				goto fail_methods;
			wrapper->methods[i].name = strdup(method_names[i]);
			if (!wrapper->methods[i].name)
				goto fail_methods;
			wrapper->methods[i].handler = ubus_zero_testkit_object_handler;
		}
	}

	wrapper->cb = cb;
	wrapper->rust_ctx = rust_ctx;
	wrapper->type->methods = wrapper->methods;
	wrapper->type->n_methods = n_methods;
	wrapper->object.type = wrapper->type;
	wrapper->object.methods = wrapper->methods;
	wrapper->object.n_methods = n_methods;
	return &wrapper->object;

fail_methods:
	while (i-- > 0)
		free((void *)wrapper->methods[i].name);
	free(wrapper->methods);
fail_type:
	free(wrapper->type);
fail_name:
	free((void *)wrapper->object.name);
fail_wrapper:
	free(wrapper);
	return NULL;
}

void ubus_zero_testkit_object_free(struct ubus_object *object)
{
	struct ubus_zero_testkit_object *wrapper =
		(struct ubus_zero_testkit_object *)object;
	int i;

	if (!wrapper)
		return;
	for (i = 0; i < wrapper->object.n_methods; i++)
		free((void *)wrapper->methods[i].name);
	free(wrapper->methods);
	free(wrapper->type);
	free((void *)wrapper->object.name);
	free(wrapper);
}

/* subscriber */
typedef int (*ubus_zero_testkit_notify_cb)(void *rust_ctx, const char *method,
		struct blob_attr *msg);
typedef void (*ubus_zero_testkit_remove_cb)(void *rust_ctx, uint32_t id);

struct ubus_zero_testkit_subscriber {
	struct ubus_subscriber subscriber;
	ubus_zero_testkit_notify_cb notify_cb;
	ubus_zero_testkit_remove_cb remove_cb;
	void *rust_ctx;
};

static int ubus_zero_testkit_subscriber_handler(struct ubus_context *ctx,
		struct ubus_object *object, struct ubus_request_data *req,
		const char *method, struct blob_attr *msg)
{
	struct ubus_subscriber *subscriber =
		container_of(object, struct ubus_subscriber, obj);
	struct ubus_zero_testkit_subscriber *wrapper =
		(struct ubus_zero_testkit_subscriber *)subscriber;

	(void)ctx;
	(void)req;
	if (!wrapper->notify_cb)
		return UBUS_STATUS_UNKNOWN_ERROR;
	return wrapper->notify_cb(wrapper->rust_ctx, method, msg);
}

static void ubus_zero_testkit_subscriber_removed(struct ubus_context *ctx,
		struct ubus_subscriber *subscriber, uint32_t id)
{
	struct ubus_zero_testkit_subscriber *wrapper =
		(struct ubus_zero_testkit_subscriber *)subscriber;

	(void)ctx;
	if (wrapper->remove_cb)
		wrapper->remove_cb(wrapper->rust_ctx, id);
}

struct ubus_subscriber *ubus_zero_testkit_subscriber_create(
		ubus_zero_testkit_notify_cb cb, ubus_zero_testkit_remove_cb remove_cb,
		void *rust_ctx)
{
	struct ubus_zero_testkit_subscriber *wrapper = calloc(1, sizeof(*wrapper));

	if (!wrapper)
		return NULL;
	wrapper->notify_cb = cb;
	wrapper->remove_cb = remove_cb;
	wrapper->rust_ctx = rust_ctx;
	INIT_LIST_HEAD(&wrapper->subscriber.list);
	wrapper->subscriber.cb = ubus_zero_testkit_subscriber_handler;
	wrapper->subscriber.remove_cb = ubus_zero_testkit_subscriber_removed;
	return &wrapper->subscriber;
}

void ubus_zero_testkit_subscriber_free(struct ubus_subscriber *subscriber)
{
	free(subscriber);
}

/* event handler */
typedef void (*ubus_zero_testkit_event_cb)(void *rust_ctx, const char *type,
		struct blob_attr *msg);

struct ubus_zero_testkit_event_handler {
	struct ubus_event_handler event_handler;
	ubus_zero_testkit_event_cb cb;
	void *rust_ctx;
};

static void ubus_zero_testkit_event_handler(struct ubus_context *ctx,
		struct ubus_event_handler *event_handler, const char *type,
		struct blob_attr *msg)
{
	struct ubus_zero_testkit_event_handler *wrapper =
		(struct ubus_zero_testkit_event_handler *)event_handler;

	(void)ctx;
	if (wrapper->cb)
		wrapper->cb(wrapper->rust_ctx, type, msg);
}

struct ubus_event_handler *ubus_zero_testkit_event_handler_create(
		ubus_zero_testkit_event_cb cb, void *rust_ctx)
{
	struct ubus_zero_testkit_event_handler *wrapper = calloc(1, sizeof(*wrapper));

	if (!wrapper)
		return NULL;
	wrapper->cb = cb;
	wrapper->rust_ctx = rust_ctx;
	wrapper->event_handler.cb = ubus_zero_testkit_event_handler;
	return &wrapper->event_handler;
}

void ubus_zero_testkit_event_handler_free(struct ubus_event_handler *event_handler)
{
	free(event_handler);
}

/* libubus header inlines and opaque context accessors. */
void ubus_zero_ubus_handle_event(struct ubus_context *ctx)
{
	ubus_handle_event(ctx);
}

int ubus_zero_ubus_ctx_fd(const struct ubus_context *ctx)
{
	return ctx->sock.fd;
}

int ubus_zero_ubus_unregister_event_handler(struct ubus_context *ctx,
		struct ubus_event_handler *event_handler)
{
	return ubus_unregister_event_handler(ctx, event_handler);
}

int ubus_zero_ubus_unregister_subscriber(struct ubus_context *ctx,
		struct ubus_subscriber *subscriber)
{
	return ubus_unregister_subscriber(ctx, subscriber);
}

void ubus_zero_ubus_request_set_fd(struct ubus_context *ctx,
		struct ubus_request_data *req, int fd)
{
	ubus_request_set_fd(ctx, req, fd);
}

int ubus_zero_ubus_request_get_caller_fd(struct ubus_request_data *req)
{
	return ubus_request_get_caller_fd(req);
}

/* Deferred replies own a C-side copy because the incoming request is transient. */
struct ubus_request_data *ubus_zero_testkit_defer_request(struct ubus_context *ctx,
		struct ubus_request_data *req)
{
	struct ubus_request_data *copy = calloc(1, sizeof(*copy));

	if (!copy)
		return NULL;
	ubus_defer_request(ctx, req, copy);
	return copy;
}

void ubus_zero_testkit_complete_deferred(struct ubus_context *ctx,
		struct ubus_request_data *req, int ret)
{
	ubus_complete_deferred_request(ctx, req, ret);
	free(req);
}
