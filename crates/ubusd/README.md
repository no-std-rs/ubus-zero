# ubusd

A pure-Rust implementation of the OpenWrt ubus broker, from the
[ubus-zero workspace](https://github.com/no-std-rs/ubus-zero). Provides an
installable Linux daemon and an embeddable `Broker` library. Protocol support
includes object registration, discovery, invocation, events, subscriptions,
monitoring, ACLs, and file-descriptor forwarding. The implementation is pre-1.0.

```sh
cargo install ubusd
ubusd --socket /tmp/ubus.sock
ubusd --socket /tmp/ubus.sock --acl-dir /etc/ubus/acl.d
```

The installed executable is named `ubusd`, as is the OpenWrt C daemon. Choose the
executable's full path when both are installed. The socket defaults to
`/var/run/ubus.sock`; pass `--socket` to match your clients' socket path.

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
