#![cfg(target_os = "linux")]

use serde_json::{json, Value};
use std::{
    io::Write,
    process::{Child, Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use ubus::{
    client::UbusClient,
    envelope::{find, EnvBuilder, UbusAttr},
    json::{blobmsg_to_json, json_to_blobmsg},
    server::UbusConnection,
    unix::UnixUbusTransport,
    wire::MsgType,
    BlobMsgTable, Transport, UbusObject,
};
use ubus_testkit::{
    skip, RpcdHarness, TapDirection, UbusCli, UbusdHarness, WireTap, RPCD_LOGIN_READONLY,
    RPCD_TEST_PASSWORD,
};

fn brokers(test: &str, body: impl Fn(&str) + Sync) {
    // Both fixtures own independent sockets and child environments. Running
    // them together lets the normal test pool cover the full consumer matrix
    // without serializing the Rust and upstream halves of every scenario.
    std::thread::scope(|scope| {
        for rust in [false, true] {
            let body = &body;
            scope.spawn(move || {
                let harness = if rust {
                    UbusdHarness::start_rust()
                } else {
                    UbusdHarness::start_upstream()
                }
                .unwrap();
                let Some(harness) = harness else {
                    skip::or_skip(test, "broker unavailable");
                    return;
                };
                body(harness.socket_path().to_str().unwrap());
            });
        }
    });
}

fn cli(socket: &str, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ubus-client"));
    command
        .args(["-s", socket])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn wait(child: Child) -> Output {
    let mut child = child;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("CLI timed out: {}", String::from_utf8_lossy(&output.stderr));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn success(socket: &str, args: &[&str]) -> Value {
    let output = wait(cli(socket, args).spawn().unwrap());
    assert!(
        output.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

struct Owner {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Owner {
    fn start(socket: &str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let socket = socket.to_owned();
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let object = UbusObject::new("example.echo")
                .method("echo", |bytes| json_to_blobmsg(&blobmsg_to_json(bytes)?));
            let transport = UnixUbusTransport::connect(&socket).unwrap();
            let mut connection = UbusConnection::connect_and_register(transport, object).unwrap();
            tx.send(()).unwrap();
            while !done.load(Ordering::Relaxed) {
                connection.poll_one().unwrap();
                connection.transport_mut().wait_recv();
            }
            connection.unregister().unwrap();
        });
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn calls_stdin_discovery_batch_and_error_codes() {
    brokers("cli finite", |socket| {
        let _owner = Owner::start(socket);
        let list = success(socket, &["list", "example.*"]);
        assert_eq!(list[0]["path"], "example.echo");
        assert_eq!(list[0]["methods"]["echo"], json!({}));
        let value = json!({"nested":{"array":[null,true,-42,1.25,"hello"]}});
        assert_eq!(
            success(
                socket,
                &["call", "example.echo", "echo", &value.to_string()]
            ),
            value
        );
        let mut child = cli(socket, &["call", "example.echo", "echo", "-"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(value.to_string().as_bytes())
            .unwrap();
        let output = wait(child);
        assert!(output.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            value
        );
        assert_eq!(success(socket, &["ping"]), json!({"ok": true}));

        let missing = wait(cli(socket, &["call", "absent", "echo"]).spawn().unwrap());
        assert_eq!(missing.status.code(), Some(4));
        assert!(missing.stdout.is_empty());
        let invalid = wait(
            cli(socket, &["call", "example.echo", "echo", "[]"])
                .spawn()
                .unwrap(),
        );
        assert_eq!(invalid.status.code(), Some(64));

        let mut child = cli(socket, &["batch"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(
            b"{\"id\":1,\"command\":\"call\",\"object\":\"absent\",\"method\":\"echo\"}\n{\"id\":2,\"command\":\"ping\"}\n"
        ).unwrap();
        let output = wait(child);
        assert_eq!(output.status.code(), Some(1));
        let responses: Vec<Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(responses[0]["status"], 4);
        assert_eq!(
            responses[1],
            json!({"id":2,"ok":true,"result":[{"ok":true}]})
        );
    });
}

fn registered(tap: &WireTap, object: u32, method: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let seq = tap
            .frames_lenient(TapDirection::ClientToBroker)
            .iter()
            .find(|frame| {
                frame.hdr.ty == MsgType::Invoke
                    && find(&frame.payload, UbusAttr::ObjId).and_then(|a| a.as_u32())
                        == Some(object)
                    && find(&frame.payload, UbusAttr::Method).and_then(|a| a.as_str())
                        == Some(method)
            })
            .map(|frame| frame.hdr.seq);
        if seq.is_some_and(|seq| {
            tap.frames_lenient(TapDirection::BrokerToClient)
                .iter()
                .any(|frame| frame.hdr.ty == MsgType::Status && frame.hdr.seq == seq)
        }) {
            return;
        }
        assert!(Instant::now() < deadline, "stream registration timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn event_stream_and_monitor_emit_flushed_json_lines() {
    brokers("cli streams", |socket| {
        let tap = WireTap::spawn(std::path::Path::new(socket)).unwrap();
        let path = tap.socket_path().to_str().unwrap();
        let listener = cli(path, &["-n", "1", "-t", "3", "listen", "example.*"])
            .spawn()
            .unwrap();
        registered(&tap, 1, "register");
        success(socket, &["send", "example.changed", "{\"value\":42}"]);
        let output = wait(listener);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let event: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(event["event"], "example.changed");
        assert_eq!(event["data"], json!({"value":42}));
        drop(tap);

        let tap = WireTap::spawn(std::path::Path::new(socket)).unwrap();
        let monitor = cli(
            tap.socket_path().to_str().unwrap(),
            &["-n", "1", "-t", "3", "monitor"],
        )
        .spawn()
        .unwrap();
        registered(&tap, 3, "add");
        success(socket, &["ping"]);
        let output = wait(monitor);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let record: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(record["send"].is_boolean());
        assert!(record["type"].is_number());
        assert!(record["payload_hex"].is_string());
    });
}

#[test]
fn subscriptions_deliver_notifications() {
    brokers("cli subscribe", |socket| {
        let mut publisher =
            UbusClient::connect(UnixUbusTransport::connect(socket).unwrap()).unwrap();
        let mut envelope = EnvBuilder::new();
        envelope.push_str(UbusAttr::ObjPath, "example.publisher");
        envelope.push_signature(&[]);
        let mut id = 0;
        publisher
            .request(MsgType::AddObject, 0, envelope, |frame| {
                id = find(&frame.payload, UbusAttr::ObjId)
                    .and_then(|a| a.as_u32())
                    .unwrap();
                Ok(())
            })
            .unwrap();
        let subscriber = cli(
            socket,
            &["-n", "1", "-t", "3", "subscribe", "example.publisher"],
        )
        .spawn()
        .unwrap();
        publisher
            .transport_mut()
            .set_deadline(Some(Duration::from_secs(3)));
        loop {
            if let Some(frame) = publisher.recv_frame().unwrap() {
                if frame.hdr.ty == MsgType::Notify
                    && find(&frame.payload, UbusAttr::Active).is_some_and(|a| a.as_bool_int8())
                {
                    break;
                }
            } else {
                publisher.transport_mut().wait_recv();
            }
        }
        let data = BlobMsgTable::build(|b| b.put_i32(Some("value"), 7));
        let mut envelope = EnvBuilder::new();
        envelope.push_u32(UbusAttr::ObjId, id);
        envelope.push_str(UbusAttr::Method, "example.notify");
        envelope.push_data(data.contents());
        publisher
            .request(MsgType::Notify, id, envelope, |_| Ok(()))
            .unwrap();
        let output = wait(subscriber);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let event: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(event["event"], "example.notify");
        assert_eq!(event["data"]["value"], 7);
    });
}

#[test]
fn waiting_and_shell_provider_work_with_both_brokers() {
    brokers("cli serve", |socket| {
        // Churn short-lived providers: their final reply must survive removal
        // and disconnect even when the broker sees all three at once.
        for _ in 0..8 {
            let provider = cli(
                socket,
                &["-n", "1", "serve", "example.shell", "echo", "--", "cat"],
            )
            .spawn()
            .unwrap();
            success(socket, &["-t", "3", "wait_for", "example.shell"]);
            assert_eq!(
                success(socket, &["call", "example.shell", "echo", "{\"value\":7}"]),
                json!({"value":7})
            );
            assert!(wait(provider).status.success());
        }
        let missing = wait(
            cli(socket, &["-t", "0.1", "wait_for", "absent"])
                .spawn()
                .unwrap(),
        );
        assert_eq!(missing.status.code(), Some(7));
    });
}

#[test]
fn upstream_cli_calls_the_shipped_shell_provider() {
    brokers("cli upstream consumer", |socket| {
        let Some(upstream) = UbusCli::locate() else {
            skip::or_skip("cli upstream consumer", "upstream ubus CLI unavailable");
            return;
        };
        let provider = cli(
            socket,
            &["-n", "2", "serve", "example.cross", "echo", "--", "cat"],
        )
        .spawn()
        .unwrap();
        success(socket, &["-t", "3", "wait_for", "example.cross"]);
        let value = json!({"unicode":"Zażółć 🦀", "nested":{"flags":[true,false], "values":[-42,2147483648i64,1.25]}, "empty":{}});
        let output = upstream
            .run(
                std::path::Path::new(socket),
                &[
                    "-t",
                    "3",
                    "call",
                    "example.cross",
                    "echo",
                    &value.to_string(),
                ],
            )
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            value
        );
        assert_eq!(
            success(
                socket,
                &["call", "example.cross", "echo", "{\"caller\":\"rust\"}"]
            ),
            json!({"caller":"rust"})
        );
        assert!(wait(provider).status.success());
        let absent = wait(cli(socket, &["list", "example.cross"]).spawn().unwrap());
        assert_eq!(absent.status.code(), Some(4));
        assert!(absent.stdout.is_empty());
    });
}

#[test]
fn encoded_oversize_handler_reply_keeps_the_provider_running() {
    brokers("cli encoded reply limit", |socket| {
        let program = r#"import json,sys; value=json.load(sys.stdin); print(json.dumps({"values":[0]*70000} if value.get("large") else value, separators=(",",":")))"#;
        let provider = cli(
            socket,
            &[
                "-n",
                "3",
                "serve",
                "example.reply-limit",
                "echo",
                "--",
                "python3",
                "-c",
                program,
            ],
        )
        .spawn()
        .unwrap();
        success(socket, &["-t", "3", "wait_for", "example.reply-limit"]);
        assert_eq!(
            success(
                socket,
                &["call", "example.reply-limit", "echo", r#"{"value":1}"#]
            ),
            json!({"value":1})
        );
        // 140,012 bytes of JSON are within the stdout budget, but 70,000
        // encoded integers exceed the complete DATA frame's wire limit.
        let oversized = wait(
            cli(
                socket,
                &[
                    "-t",
                    "3",
                    "call",
                    "example.reply-limit",
                    "echo",
                    r#"{"large":true}"#,
                ],
            )
            .spawn()
            .unwrap(),
        );
        assert_eq!(
            oversized.status.code(),
            Some(9),
            "{}",
            String::from_utf8_lossy(&oversized.stderr)
        );
        assert!(oversized.stdout.is_empty());
        assert_eq!(
            success(
                socket,
                &["call", "example.reply-limit", "echo", r#"{"value":2}"#]
            ),
            json!({"value":2})
        );
        let provider = wait(provider);
        assert!(
            provider.status.success(),
            "{}",
            String::from_utf8_lossy(&provider.stderr)
        );
        assert!(String::from_utf8_lossy(&provider.stderr)
            .contains("handler reply exceeds maximum message length"));
    });
}

#[test]
fn shell_provider_round_trips_empty_json_keys() {
    brokers("cli empty JSON keys", |socket| {
        let provider = cli(
            socket,
            &[
                "-n",
                "3",
                "serve",
                "example.empty-keys",
                "echo",
                "--",
                "cat",
            ],
        )
        .spawn()
        .unwrap();
        success(socket, &["-t", "3", "wait_for", "example.empty-keys"]);
        for value in [
            json!({"": 1}),
            json!({"outer": {"": [null, true, 7, 1.25, "text"]}}),
            json!({"items": [{"": {"": false}}, 2]}),
        ] {
            assert_eq!(
                success(
                    socket,
                    &["call", "example.empty-keys", "echo", &value.to_string()]
                ),
                value
            );
        }
        assert!(wait(provider).status.success());
    });
}

#[test]
fn real_rpcd_sessions_and_error_statuses() {
    brokers("cli rpcd", |socket| {
        let Some(rpcd) = RpcdHarness::start(std::path::Path::new(socket), &[]).unwrap() else {
            skip::or_skip("cli rpcd", "upstream rpcd unavailable");
            return;
        };
        let login = success(
            socket,
            &[
                "call",
                "session",
                "login",
                &json!({"username":RPCD_LOGIN_READONLY, "password":RPCD_TEST_PASSWORD}).to_string(),
            ],
        );
        let sid = login["ubus_rpc_session"].as_str().unwrap();
        assert_eq!(sid.len(), 32);
        assert_eq!(login["data"]["username"], RPCD_LOGIN_READONLY);
        let values =
            json!({"nested":{"unicode":"üñî", "array":[true,7,1.25]}, "text":"line\nnext"});
        let args = json!({"ubus_rpc_session":sid, "values":values});
        let output = wait(
            cli(socket, &["call", "session", "set", &args.to_string()])
                .spawn()
                .unwrap(),
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
        let args = json!({"ubus_rpc_session":sid, "keys":["nested", "text"]});
        assert_eq!(
            success(socket, &["call", "session", "get", &args.to_string()]),
            json!({"values":values})
        );
        let denied = wait(
            cli(
                socket,
                &[
                    "call",
                    "session",
                    "login",
                    "{\"username\":\"ro\",\"password\":\"wrong\"}",
                ],
            )
            .spawn()
            .unwrap(),
        );
        assert_eq!(denied.status.code(), Some(6));
        assert!(denied.stdout.is_empty());
        assert!(String::from_utf8_lossy(&denied.stderr).contains('6'));

        let mut child = cli(socket, &["batch"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(
            b"{\"id\":\"bad-method\",\"command\":\"call\",\"object\":\"session\",\"method\":\"absent\"}\n{\"id\":\"still-connected\",\"command\":\"ping\"}\n"
        ).unwrap();
        let output = wait(child);
        assert_eq!(output.status.code(), Some(1));
        let responses: Vec<Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(responses[0]["id"], "bad-method");
        assert_eq!(responses[0]["status"], 3);
        assert_eq!(
            responses[1],
            json!({"id":"still-connected", "ok":true, "result":[{"ok":true}]})
        );
        drop(rpcd);
    });
}

#[test]
fn concurrent_large_cli_calls_keep_replies_correlated() {
    brokers("cli concurrent large calls", |socket| {
        let _owner = Owner::start(socket);
        std::thread::scope(|scope| {
            for worker in 0..8 {
                scope.spawn(move || {
                    // Larger than socket/read scratch buffers but below the
                    // per-argument exec limit on Linux.
                    let payload = json!({"worker":worker, "text":"üx".repeat(16_384), "nested":{"array":[true,-1,2147483648i64]}});
                    for request in 0..3 {
                        let mut args = payload.clone();
                        args["request"] = json!(request);
                        assert_eq!(success(socket, &["-t", "5", "call", "example.echo", "echo", &args.to_string()]), args);
                    }
                });
            }
        });
    });
}

#[test]
fn help_and_usage_do_not_need_a_broker() {
    assert!(Command::new(env!("CARGO_BIN_EXE_ubus-client"))
        .arg("--help")
        .output()
        .unwrap()
        .status
        .success());
    let output = Command::new(env!("CARGO_BIN_EXE_ubus-client"))
        .args(["--timeout", "nan", "ping"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(64));
}
