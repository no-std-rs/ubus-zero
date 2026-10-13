# ubusd

A pure-Rust implementation of the OpenWrt ubus broker, from the
[ubus-zero workspace](https://github.com/no-std-rs/ubus-zero). Provides an
installable Linux daemon and an embeddable `Broker` library. Protocol support
includes object registration, discovery, invocation, events, subscriptions,
monitoring, ACLs, and file-descriptor forwarding. The implementation is pre-1.0.

```sh
cargo install --git https://github.com/no-std-rs/ubus-zero ubusd
ubusd --socket /tmp/ubus.sock
```

The daemon stays in the foreground. To load ACLs, start it with
`--acl-dir /etc/ubus/acl.d` in addition to the socket option.

The installed executable is named `ubusd`, as is the OpenWrt C daemon. Choose the
executable's full path when both are installed. The socket defaults to
`/var/run/ubus.sock`; pass `--socket` to match your clients' socket path.

## Access control

Non-root clients need ACL grants to register named objects, call their methods,
subscribe, or send/listen for events. Omitting `--acl-dir` leaves those operations
denied for non-root clients. Root clients bypass ACLs; monitoring requires root.

Pass `--acl-dir /etc/ubus/acl.d` to load `*.json` ACL files. Files must be owned
by root and must not be writable by group or others. For example,
`/etc/ubus/acl.d/example.json` can allow the local user `alice` to provide and call
the CLI's echo service:

```json
{
  "user": "alice",
  "publish": ["example.echo"],
  "access": {
    "example.echo": { "methods": ["echo"] }
  }
}
```

Replace `alice` with the provider/caller's local username, then start the daemon:

```sh
ubusd --socket /tmp/ubus.sock --acl-dir /etc/ubus/acl.d
```

## Embed the broker

```rust,no_run
use ubusd::{Broker, Config};

let mut broker = Broker::bind(Config {
    socket_path: "/tmp/ubus.sock".into(),
    ..Config::default()
})?;
broker.poll_once(0)?;
# Ok::<(), ubusd::BrokerError>(())
```

The binary supports Linux. SIGTERM/SIGINT cleanly remove its socket; SIGHUP
reloads ACLs. See the workspace's compatibility documentation for its tested
wire contract. The `testing` feature exposes deterministic backpressure controls
for compatibility tests.
