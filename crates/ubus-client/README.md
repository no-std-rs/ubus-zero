# ubus-client

A Linux CLI for OpenWrt ubus, implemented in Rust using `ubus-zero`. This crate
contains only the executable. Applications use `ubus-zero` for protocol APIs,
JSON conversion and the optional Linux Unix-socket adapter.

```sh
cargo install ubus-client
ubus-client -s /var/run/ubus/ubus.sock list
ubus-client call system board
printf '{"interface":"wan"}\n' | ubus-client call network.interface status -
ubus-client -t 10 wait_for network.interface
ubus-client listen 'network.*' | jq --unbuffered '.data'
ubus-client -n 1 subscribe example.status
ubus-client send example.changed '{"value":42}'
ubus-client monitor
```

The socket defaults to `UBUS_SOCKET` or `/var/run/ubus/ubus.sock`. Set `-s` when
using a daemon at `/var/run/ubus.sock` or a custom socket. Options precede the
command; `--help` lists every command. `list` returns an array with object IDs,
paths and method signatures. `call` prints each DATA reply as a JSON line;
status-only success prints no reply. `listen`, `subscribe` and `monitor` flush
each JSON line immediately. Monitor records include direction, client, peer,
sequence, type and the raw envelope in `payload_hex`. Monitoring requires root.

Finite requests time out after five seconds by default. `-t SECONDS` sets the
deadline and bounds event streams when explicitly given. `-n COUNT` stops a
stream after the requested number of messages. `-p` formats finite results.

## Persistent scripting

`batch` keeps a single connection open and processes one JSON request per line:

```sh
printf '%s\n' \
  '{"id":1,"command":"list","pattern":"network.*"}' \
  '{"id":2,"command":"call","object":"system","method":"board","args":{}}' \
  '{"id":3,"command":"send","event":"example.changed","args":{"value":42}}' \
  | ubus-client batch
```

Each response contains `id`, `ok`, and a `result` array, or `error` and `status`.
Supported batch commands are `list`, `call`, `send`, `ping`, and `wait_for` (with
an `objects` array). Request failures produce a response and processing continues;
the batch exits 1 if any request failed.

`serve` registers methods backed by a program. Each invocation starts the program,
sends the request JSON to its stdin, and converts its JSON object on stdout into
the reply. `UBUS_OBJECT` and `UBUS_METHOD` identify the invocation. Program stderr
goes to the CLI's stderr; arguments are passed directly without a shell:

```sh
ubus-client serve example.echo echo -- cat
ubus-client call example.echo echo '{"message":"hello"}'
```

Each handler has the configured request timeout. Failed handlers return an ubus
error. The provider runs until stopped, or `-n COUNT` requests have completed.

## Exit codes and data types

Success exits 0; positive ubus statuses are preserved (for example, 4 for missing
objects and 6 for denied access); timeouts exit 7, input/usage errors 64, and
other failures 1. Diagnostics use stderr, leaving stdout available to scripts.

Arguments must be JSON objects. Nested tables/arrays, strings, booleans, null,
signed integers and finite doubles are supported. Integer encoding follows
libblobmsg_json's signed 64-bit representation; unsigned values above `i64::MAX`
are clamped to `i64::MAX`. Strings and attribute names cannot contain NUL.
