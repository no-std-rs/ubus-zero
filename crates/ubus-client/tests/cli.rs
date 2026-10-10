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
use ubus_testkit::{skip, TapDirection, UbusdHarness, WireTap};

fn brokers(test: &str, body: impl Fn(&str)) {
    for rust in [false, true] {
        let harness = if rust {
            UbusdHarness::start_rust()
        } else {
            UbusdHarness::start_upstream()
        }
        .unwrap();
        let Some(harness) = harness else {
            skip::or_skip(test, "broker unavailable");
            continue;
        };
        body(harness.socket_path().to_str().unwrap());
    }
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
