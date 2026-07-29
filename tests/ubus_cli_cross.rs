//! End-to-end cross-check: drive the upstream `ubus` CLI binary (the
//! reference libubus + blobmsg_json client operators actually run) as
//! a peer against both broker implementations. The wire-level
//! `ubusd_handshake` matrix drives our own client; this file pins the
//! JSON → blobmsg args → reply → JSON round-trip that only the CLI
//! exercises. Upstream `ubus` reports a visible skip when its binary isn't
//! reachable (fresh clone before `tools/ubusd-builder/build.sh`).

#![cfg(feature = "testing")]

use std::path::Path;
use std::time::Duration;

use ubus::blobmsg::{
    lookup as blobmsg_lookup, BlobBuilder, BlobIter, BlobMsgTable, BlobType, BlobValue,
};
use ubus::envelope::{find, EnvBuilder, UbusAttr};
use ubus::server::UbusObject;
use ubus::transport::Transport;
use ubus::wire::{Frame, Header, MsgType, PROTO_VERSION, UBUS_STATUS_OK};
use ubus_testkit::UnixUbusTransport;
use ubus_testkit::{run_matrix_case, skip, MatrixArm, TapDirection, UbusCli, WireTap};

#[allow(dead_code)]
mod common;
use common::{raw_add_object_with_signature, recv_until, signature_with_ping_method, OwnerThread};

fn locate_cli_or_skip(test: &str) -> Option<UbusCli> {
    let cli = UbusCli::locate();
    if cli.is_none() {
        skip::or_skip(test, "ubus CLI binary not reachable");
    }
    cli
}

use rstest::rstest;

fn cli_list_shows_registered_object_body(path: &str, cli: &UbusCli) {
    let socket = Path::new(path);
    let path = path.to_owned();

    let owner = OwnerThread::spawn(path, || {
        UbusObject::new("example.test").method("ping", |_| Ok(BlobMsgTable::build(|_| {})))
    });
    owner.wait_ready();

    // Bare `list`: every registered path, one per line.
    let out = cli.run(socket, &["list"]).expect("run ubus list");
    assert!(
        out.status.success(),
        "ubus list exited {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.lines().any(|l| l == "example.test"),
        "ubus list missing `example.test` line; stdout: {stdout:?}",
    );

    // Filtered `list <pattern>`: same object, addressed directly.
    let out = cli
        .run(socket, &["list", "example.test"])
        .expect("run ubus list pattern");
    assert!(
        out.status.success(),
        "ubus list <pattern> exited {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.lines().any(|l| l == "example.test"),
        "ubus list example.test missing `example.test` line; stdout: {stdout:?}",
    );

    owner.shutdown();
}

#[rstest]
#[case::upstream(MatrixArm::Upstream)]
#[case::rs(MatrixArm::Rs)]
fn cli_list_shows_registered_object(#[case] arm: MatrixArm) {
    let Some(cli) = locate_cli_or_skip("ubus_cli_cross cli_list_shows_registered_object") else {
        return;
    };
    run_matrix_case(
        "ubus_cli_cross cli_list_shows_registered_object",
        arm,
        &|path| cli_list_shows_registered_object_body(path, &cli),
    );
}

fn cli_list_verbose_shows_signature_body(path: &str, cli: &UbusCli) {
    let socket = Path::new(path);
    let path = path.to_owned();

    let owner = OwnerThread::spawn(path, || {
        UbusObject::new("example.verbose").method("ping", |_| Ok(BlobMsgTable::build(|_| {})))
    });
    owner.wait_ready();

    let out = cli.run(socket, &["list", "-v"]).expect("run ubus list -v");
    assert!(
        out.status.success(),
        "ubus list -v exited {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("example.verbose"),
        "ubus list -v missing object path; stdout: {stdout:?}",
    );
    assert!(
        stdout.contains("ping"),
        "ubus list -v missing method name; stdout: {stdout:?}",
    );

    owner.shutdown();
}

#[rstest]
#[case::upstream(MatrixArm::Upstream)]
#[case::rs(MatrixArm::Rs)]
fn cli_list_verbose_shows_signature(#[case] arm: MatrixArm) {
    let Some(cli) = locate_cli_or_skip("ubus_cli_cross cli_list_verbose_shows_signature") else {
        return;
    };
    run_matrix_case(
        "ubus_cli_cross cli_list_verbose_shows_signature",
        arm,
        &|path| cli_list_verbose_shows_signature_body(path, &cli),
    );
}

fn cli_list_pattern_filters_body(path: &str, cli: &UbusCli) {
    let socket = Path::new(path);
    let path = path.to_owned();

    let alpha_path = path.clone();
    let alpha = OwnerThread::spawn(alpha_path, || {
        UbusObject::new("example.alpha").method("ping", |_| Ok(BlobMsgTable::build(|_| {})))
    });
    alpha.wait_ready();
    let beta = OwnerThread::spawn(path, || {
        UbusObject::new("example.beta").method("ping", |_| Ok(BlobMsgTable::build(|_| {})))
    });
    beta.wait_ready();

    let out = cli
        .run(socket, &["list", "example.a*"])
        .expect("run ubus list pattern");
    assert!(
        out.status.success(),
        "ubus list pattern exited {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.lines().any(|l| l == "example.alpha"),
        "ubus list example.a* missing alpha; stdout: {stdout:?}",
    );
    assert!(
        !stdout.lines().any(|l| l == "example.beta"),
        "ubus list example.a* unexpectedly showed beta; stdout: {stdout:?}",
    );

    alpha.shutdown();
    beta.shutdown();
}

#[rstest]
#[case::upstream(MatrixArm::Upstream)]
#[case::rs(MatrixArm::Rs)]
fn cli_list_pattern_filters(#[case] arm: MatrixArm) {
    let Some(cli) = locate_cli_or_skip("ubus_cli_cross cli_list_pattern_filters") else {
        return;
    };
    run_matrix_case("ubus_cli_cross cli_list_pattern_filters", arm, &|path| {
        cli_list_pattern_filters_body(path, &cli)
    });
}

fn cli_call_round_trips_json_args_body(path: &str, cli: &UbusCli) {
    let socket = Path::new(path);
    let path = path.to_owned();

    // `echo` reads the string arg `key` (the `__missing__` marker makes
    // a dropped arg fail the assertion loudly rather than echo an empty
    // string) and returns it under the same key.
    let owner = OwnerThread::spawn(path, || {
        UbusObject::new("example.test").method("echo", |args| {
            let echoed = blobmsg_lookup(args, "key")
                .and_then(|a| a.as_str())
                .unwrap_or("__missing__")
                .to_owned();
            Ok(BlobMsgTable::build(|bb| {
                bb.put_str(Some("key"), &echoed);
            }))
        })
    });
    owner.wait_ready();

    // `-t 3` bounds the call so a routing bug fails loud instead of
    // hanging the suite. The JSON is a single argv element — no shell
    // quoting, `Command` passes it verbatim.
    let out = cli
        .run(
            socket,
            &[
                "-t",
                "3",
                "call",
                "example.test",
                "echo",
                r#"{"key":"val"}"#,
            ],
        )
        .expect("run ubus call");
    assert!(
        out.status.success(),
        "ubus call exited {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    // Upstream's pretty-printer emits `\t"key": "val"`; match the
    // substring without the leading tab.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"key\": \"val\""),
        "ubus call reply missing echoed arg; stdout: {stdout:?}",
    );

    owner.shutdown();
}

#[rstest]
#[case::upstream(MatrixArm::Upstream)]
#[case::rs(MatrixArm::Rs)]
fn cli_call_round_trips_json_args(#[case] arm: MatrixArm) {
    let Some(cli) = locate_cli_or_skip("ubus_cli_cross cli_call_round_trips_json_args") else {
        return;
    };
    run_matrix_case(
        "ubus_cli_cross cli_call_round_trips_json_args",
        arm,
        &|path| cli_call_round_trips_json_args_body(path, &cli),
    );
}

/// Rebuild a blobmsg stream while preserving every type that json-c can
/// produce for `ubus call`. Int16 and Raw have no exact BlobBuilder writer and
/// cannot arrive through that path, so they are deliberately omitted.
fn reencode_blobmsg(bb: &mut BlobBuilder, bytes: &[u8]) {
    for attr in BlobIter::new(bytes) {
        let name = attr.name;
        match attr.value().expect("decode CLI blobmsg argument") {
            BlobValue::String(value) => bb.put_str(name, value),
            BlobValue::Int8(value) => bb.put_bool(name, value != 0),
            BlobValue::Int16(_) | BlobValue::Raw(_) => {
                // json-c does not emit these, and BlobBuilder has no exact
                // Int16/Raw writer to preserve an arbitrary incoming value.
            }
            BlobValue::Int32(value) => bb.put_i32(name, value),
            BlobValue::Int64(value) => bb.put_i64(name, value),
            BlobValue::Double(value) => bb.put_double(name, value),
            BlobValue::Table(contents) => {
                bb.open_table(name);
                reencode_blobmsg(bb, contents);
                bb.close_table();
            }
            BlobValue::Array(contents) => {
                bb.open_array(name);
                reencode_blobmsg(bb, contents);
                bb.close_array();
            }
        }
    }
}

fn cli_blob_type_name(attr_type: BlobType, payload: &[u8]) -> &'static str {
    match attr_type {
        BlobType::Int32 => "int32",
        BlobType::Int64 => "int64",
        BlobType::Int8 if payload.len() == 1 => "bool",
        BlobType::Double => "double",
        BlobType::String => "string",
        BlobType::Table => "table",
        BlobType::Array => "array",
        other => panic!("unexpected CLI JSON blobmsg type {other:?}"),
    }
}

fn cli_call_type_sweep_body(path: &str, cli: &UbusCli) {
    let socket = Path::new(path);
    let path = path.to_owned();
    let longstr = "a".repeat(256);
    let args = format!(
        r#"{{"neg":-1,"i32min":-2147483648,"i32max":2147483647,"beyond32":2147483648,"negbeyond":-2147483649,"i64big":9223372036854775807,"t":true,"f":false,"d":2.5,"dneg":-0.5,"uni":"üñî","longstr":"{longstr}","empty_obj":{{}},"empty_arr":[],"nested":{{"x":[1,2]}}}}"#
    );

    let owner = OwnerThread::spawn(path, || {
        UbusObject::new("example.sweep").method("echo", |args| {
            Ok(BlobMsgTable::build(|bb| {
                bb.open_table(Some("echo"));
                reencode_blobmsg(bb, args);
                bb.close_table();

                bb.open_table(Some("types"));
                for attr in BlobIter::new(args) {
                    let name = attr.name.expect("top-level CLI JSON argument has a name");
                    bb.put_str(Some(name), cli_blob_type_name(attr.ty, attr.payload));
                }
                bb.close_table();
            }))
        })
    });
    owner.wait_ready();

    let out = cli
        .run(socket, &["call", "example.sweep", "echo", &args])
        .expect("run ubus type-sweep call");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let input: serde_json::Value = serde_json::from_str(&args).expect("parse input JSON");
    let stdout = String::from_utf8(out.stdout).expect("ubus stdout is UTF-8");
    let reply: serde_json::Value = serde_json::from_str(&stdout).expect("parse ubus reply JSON");
    let expected_types = serde_json::json!({
        "neg": "int32",
        "i32min": "int32",
        "i32max": "int32",
        "beyond32": "int64",
        "negbeyond": "int64",
        "i64big": "int64",
        "t": "bool",
        "f": "bool",
        "d": "double",
        "dneg": "double",
        "uni": "string",
        "longstr": "string",
        "empty_obj": "table",
        "empty_arr": "array",
        "nested": "table",
    });
    assert_eq!(reply["types"], expected_types, "stdout: {stdout:?}");
    assert_eq!(reply["echo"], input, "stdout: {stdout:?}");

    owner.shutdown();
}

#[rstest]
#[case::upstream(MatrixArm::Upstream)]
#[case::rs(MatrixArm::Rs)]
fn cli_call_type_sweep(#[case] arm: MatrixArm) {
    let Some(cli) = locate_cli_or_skip("ubus_cli_cross cli_call_type_sweep") else {
        return;
    };
    run_matrix_case("ubus_cli_cross cli_call_type_sweep", arm, &|path| {
        cli_call_type_sweep_body(path, &cli)
    });
}

fn cli_call_simplified_output_body(path: &str, cli: &UbusCli) {
    let socket = Path::new(path);
    let path = path.to_owned();
    let owner = OwnerThread::spawn(path, || {
        UbusObject::new("example.simple").method("reply", |_| {
            Ok(BlobMsgTable::build(|bb| {
                bb.put_i32(Some("i32"), 42);
                bb.put_i64(Some("big"), 4_294_967_296);
                bb.put_double(Some("d"), 2.5);
                bb.put_bool(Some("t"), true);
                bb.put_str(Some("s"), "x");
            }))
        })
    });
    owner.wait_ready();

    let compact = cli
        .run(socket, &["-S", "call", "example.simple", "reply", "{}"])
        .expect("run simplified ubus call");
    assert_eq!(
        compact.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&compact.stderr)
    );
    assert_eq!(
        compact.stdout,
        b"{\"i32\":42,\"big\":4294967296,\"d\":2.5,\"t\":true,\"s\":\"x\"}\n"
    );

    let plain = cli
        .run(socket, &["call", "example.simple", "reply", "{}"])
        .expect("run plain ubus call");
    assert_eq!(
        plain.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&plain.stderr)
    );
    let compact_json: serde_json::Value =
        serde_json::from_slice(&compact.stdout).expect("parse compact reply JSON");
    let plain_json: serde_json::Value =
        serde_json::from_slice(&plain.stdout).expect("parse plain reply JSON");
    assert_eq!(plain_json, compact_json);
    assert!(
        plain.stdout.contains(&b'\t'),
        "plain ubus call must pretty-print with tabs; stdout: {:?}",
        String::from_utf8_lossy(&plain.stdout),
    );

    let simplified_list = cli
        .run(socket, &["-S", "list", "-v", "example.simple"])
        .expect("run simplified ubus list -v");
    assert_eq!(
        simplified_list.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&simplified_list.stderr)
    );
    assert_eq!(simplified_list.stdout, b"example.simple\n");

    owner.shutdown();
}

#[rstest]
#[case::upstream(MatrixArm::Upstream)]
#[case::rs(MatrixArm::Rs)]
fn cli_call_simplified_output(#[case] arm: MatrixArm) {
    let Some(cli) = locate_cli_or_skip("ubus_cli_cross cli_call_simplified_output") else {
        return;
    };
    run_matrix_case("ubus_cli_cross cli_call_simplified_output", arm, &|path| {
        cli_call_simplified_output_body(path, &cli)
    });
}

fn cli_call_unknown_object_reports_not_found_body(path: &str, cli: &UbusCli) {
    let socket = Path::new(path);

    // No object registered: the CLI must decode the broker's
    // UBUS_STATUS_NOT_FOUND to the reference message. This is the
    // end-user-visible pin for NOT_FOUND parity across both brokers.
    let out = cli
        .run(socket, &["-t", "3", "call", "no.such.object", "x"])
        .expect("run ubus call unknown");
    assert!(
        !out.status.success(),
        "ubus call on a missing object must exit non-zero; stdout: {}",
        String::from_utf8_lossy(&out.stdout),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Command failed: Not found"),
        "ubus call must report `Command failed: Not found`; stderr: {stderr:?}",
    );
}

#[rstest]
#[case::upstream(MatrixArm::Upstream)]
#[case::rs(MatrixArm::Rs)]
fn cli_call_unknown_object_reports_not_found(#[case] arm: MatrixArm) {
    let Some(cli) = locate_cli_or_skip("ubus_cli_cross cli_call_unknown_object_reports_not_found")
    else {
        return;
    };
    run_matrix_case(
        "ubus_cli_cross cli_call_unknown_object_reports_not_found",
        arm,
        &|path| cli_call_unknown_object_reports_not_found_body(path, &cli),
    );
}

fn cli_listen_receives_sent_event_body(path: &str, cli: &UbusCli) {
    use std::io::{BufRead, BufReader};
    use std::time::Instant;
    use ubus::envelope::{find, UbusAttr};
    use ubus::wire::MsgType;

    // Front the broker(-tap) socket with a tap we own so the listener's
    // `register` is observable — a readiness barrier, not a blind sleep.
    // The event object is `hdr.peer == 1` (UBUS_SYSTEM_OBJECT_EVENT).
    const EVENT_OBJECT_ID: u32 = 1;
    let inner = WireTap::spawn(Path::new(path)).expect("spawn inner listen tap");
    let inner_path = inner
        .socket_path()
        .to_str()
        .expect("utf-8 inner tap path")
        .to_owned();

    // Long-running `ubus listen example.*` pointed at the inner tap: it
    // does HELLO + an anonymous ADD_OBJECT + a `register` INVOKE on the
    // event object, then blocks reading fan-out until killed.
    let mut listener = cli
        .spawn(Path::new(&inner_path), &["listen", "example.*"])
        .expect("spawn ubus listen");

    // Barrier: wait until the tap has seen the `register` INVOKE on the
    // event object AND the broker's STATUS reply echoing its seq — the
    // listener is then provably registered, so the send below cannot race
    // registration. Deadline loop, not a sleep.
    let reg_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let register_seq = inner
            .frames_lenient(TapDirection::ClientToBroker)
            .into_iter()
            .find(|f| {
                f.hdr.ty == MsgType::Invoke
                    && f.hdr.peer == EVENT_OBJECT_ID
                    && find(&f.payload, UbusAttr::Method).and_then(|a| a.as_str())
                        == Some("register")
            })
            .map(|f| f.hdr.seq);
        let registered = register_seq.is_some_and(|seq| {
            inner
                .frames_lenient(TapDirection::BrokerToClient)
                .iter()
                .any(|f| f.hdr.ty == MsgType::Status && f.hdr.seq == seq)
        });
        if registered {
            break;
        }
        if Instant::now() >= reg_deadline {
            let _ = listener.kill();
            let _ = listener.wait();
            panic!("ubus listen did not register (register INVOKE + STATUS ack) within 5s");
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    // One-shot `ubus send` of a matching event, straight at the broker(-tap).
    let out = cli
        .run(
            Path::new(path),
            &["send", "example.probe.event", r#"{"value":7}"#],
        )
        .expect("run ubus send");
    assert!(
        out.status.success(),
        "ubus send exited {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );

    // Read the listener's stdout on a worker thread so the harvest has a
    // real deadline (a blocking pipe read can't be polled): the thread
    // returns the moment the expected event line appears, and `kill`
    // below unblocks it via EOF if the event never lands.
    let stdout = listener.stdout.take().expect("listener stdout piped");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut buf = BufReader::new(stdout);
        let mut captured = String::new();
        loop {
            let mut line = String::new();
            match buf.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    captured.push_str(&line);
                    if captured.contains("example.probe.event")
                        && captured.contains("value")
                        && captured.contains('7')
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(captured);
    });
    let matched = rx.recv_timeout(Duration::from_secs(5)).ok();

    // Kill by saved PID (`Child::kill`) — never `pkill -f`, which would
    // match and kill the test process itself.
    listener.kill().expect("kill ubus listen");
    let _ = listener.wait();
    let _ = reader.join();
    // On a timeout the reader was still blocked; after the kill it flushes
    // whatever it captured — pick that up for the diagnostic.
    let captured = matched.or_else(|| rx.try_recv().ok()).unwrap_or_default();
    drop(inner);

    assert!(
        captured.contains("example.probe.event"),
        "ubus listen missing event name; stdout: {captured:?}",
    );
    assert!(
        captured.contains("value") && captured.contains('7'),
        "ubus listen missing event payload; stdout: {captured:?}",
    );
}

#[rstest]
#[case::upstream(MatrixArm::Upstream)]
#[case::rs(MatrixArm::Rs)]
fn cli_listen_receives_sent_event(#[case] arm: MatrixArm) {
    let Some(cli) = locate_cli_or_skip("ubus_cli_cross cli_listen_receives_sent_event") else {
        return;
    };
    run_matrix_case(
        "ubus_cli_cross cli_listen_receives_sent_event",
        arm,
        &|path| cli_listen_receives_sent_event_body(path, &cli),
    );
}

fn cli_monitor_streams_broker_traffic_body(path: &str, cli: &UbusCli) {
    use std::io::{BufRead, BufReader};
    use std::time::Instant;

    const MONITOR_OBJECT_ID: u32 = 3;
    let inner = WireTap::spawn(Path::new(path)).expect("spawn inner monitor tap");
    let inner_path = inner
        .socket_path()
        .to_str()
        .expect("utf-8 inner tap path")
        .to_owned();
    let mut monitor = cli
        .spawn(Path::new(&inner_path), &["monitor"])
        .expect("spawn ubus monitor");

    // Barrier: the monitor is active only after its object-3 `add` INVOKE
    // and the matching STATUS acknowledgement have crossed the tap.
    let reg_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let add_seq = inner
            .frames_lenient(TapDirection::ClientToBroker)
            .into_iter()
            .find(|f| {
                f.hdr.ty == MsgType::Invoke
                    && find(&f.payload, UbusAttr::ObjId).and_then(|a| a.as_u32())
                        == Some(MONITOR_OBJECT_ID)
                    && find(&f.payload, UbusAttr::Method).and_then(|a| a.as_str()) == Some("add")
            })
            .map(|f| f.hdr.seq);
        let registered = add_seq.is_some_and(|seq| {
            inner
                .frames_lenient(TapDirection::BrokerToClient)
                .iter()
                .any(|f| f.hdr.ty == MsgType::Status && f.hdr.seq == seq)
        });
        if registered {
            break;
        }
        if Instant::now() >= reg_deadline {
            let _ = monitor.kill();
            let _ = monitor.wait();
            panic!("ubus monitor did not register (add INVOKE + STATUS ack) within 5s");
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    // Register and call directly against the broker socket so the monitor
    // observes ordinary broker traffic, rather than traffic generated via its tap.
    let owner = OwnerThread::spawn(path.to_owned(), || {
        UbusObject::new("example.moncli").method("ping", |_| Ok(BlobMsgTable::build(|_| {})))
    });
    owner.wait_ready();
    let out = cli
        .run(
            Path::new(path),
            &["-t", "3", "call", "example.moncli", "ping", "{}"],
        )
        .expect("run ubus monitor probe call");
    assert!(
        out.status.success(),
        "ubus call exited {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );

    // Read stdout on a worker thread so the harvest has a real deadline;
    // killing the child below unblocks a still-pending read through EOF.
    let stdout = monitor.stdout.take().expect("monitor stdout piped");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut buf = BufReader::new(stdout);
        let mut captured = String::new();
        loop {
            let mut line = String::new();
            match buf.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    captured.push_str(&line);
                    if captured.contains("add_object")
                        && captured.contains("example.moncli")
                        && captured.contains("invoke")
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(captured);
    });
    let matched = rx.recv_timeout(Duration::from_secs(5)).ok();

    // Kill by the saved child PID; EOF releases the stdout worker on timeout.
    monitor.kill().expect("kill ubus monitor");
    let _ = monitor.wait();
    let _ = reader.join();
    let captured = matched.or_else(|| rx.try_recv().ok()).unwrap_or_default();

    owner.shutdown();
    drop(inner);

    assert!(
        captured.contains("add_object"),
        "ubus monitor missing add_object frame; stdout: {captured:?}",
    );
    assert!(
        captured.contains("example.moncli"),
        "ubus monitor missing registered object path; stdout: {captured:?}",
    );
    assert!(
        captured.contains("invoke"),
        "ubus monitor missing INVOKE frame; stdout: {captured:?}",
    );
    assert!(
        captured.contains("-> ") && captured.contains("<- "),
        "ubus monitor missing direction markers; stdout: {captured:?}",
    );
}

#[rstest]
#[case::upstream(MatrixArm::Upstream)]
#[case::rs(MatrixArm::Rs)]
fn cli_monitor_streams_broker_traffic(#[case] arm: MatrixArm) {
    let Some(cli) = locate_cli_or_skip("ubus_cli_cross cli_monitor_streams_broker_traffic") else {
        return;
    };
    run_matrix_case(
        "ubus_cli_cross cli_monitor_streams_broker_traffic",
        arm,
        &|path| cli_monitor_streams_broker_traffic_body(path, &cli),
    );
}

fn cli_monitor_probe_output(path: &str, cli: &UbusCli, monitor_args: &[&str]) -> String {
    use std::io::Read;
    use std::time::Instant;

    const MONITOR_OBJECT_ID: u32 = 3;
    let inner = WireTap::spawn(Path::new(path)).expect("spawn inner monitor tap");
    let inner_path = inner
        .socket_path()
        .to_str()
        .expect("utf-8 inner tap path")
        .to_owned();
    let mut monitor = cli
        .spawn(Path::new(&inner_path), monitor_args)
        .expect("spawn ubus monitor");

    // Match the established monitor test's barrier: the command is ready only
    // after its object-3 `add` request has received a STATUS acknowledgement.
    let reg_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let add_seq = inner
            .frames_lenient(TapDirection::ClientToBroker)
            .into_iter()
            .find(|f| {
                f.hdr.ty == MsgType::Invoke
                    && find(&f.payload, UbusAttr::ObjId).and_then(|a| a.as_u32())
                        == Some(MONITOR_OBJECT_ID)
                    && find(&f.payload, UbusAttr::Method).and_then(|a| a.as_str()) == Some("add")
            })
            .map(|f| f.hdr.seq);
        let registered = add_seq.is_some_and(|seq| {
            inner
                .frames_lenient(TapDirection::BrokerToClient)
                .iter()
                .any(|f| f.hdr.ty == MsgType::Status && f.hdr.seq == seq)
        });
        if registered {
            break;
        }
        if Instant::now() >= reg_deadline {
            let _ = monitor.kill();
            let _ = monitor.wait();
            panic!("ubus monitor did not register (add INVOKE + STATUS ack) within 5s");
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    std::thread::sleep(Duration::from_millis(300));
    let owner = OwnerThread::spawn(path.to_owned(), || {
        UbusObject::new("example.mon").method("ping", |_| Ok(BlobMsgTable::build(|_| {})))
    });
    owner.wait_ready();
    let out = cli
        .run(Path::new(path), &["call", "example.mon", "ping", "{}"])
        .expect("run ubus monitor probe call");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::thread::sleep(Duration::from_millis(300));

    // Kill by the saved PID, then collect its now-closed stdout pipe.
    monitor.kill().expect("kill ubus monitor");
    let _ = monitor.wait();
    let mut captured = String::new();
    monitor
        .stdout
        .take()
        .expect("monitor stdout piped")
        .read_to_string(&mut captured)
        .expect("read monitor stdout");

    owner.shutdown();
    drop(inner);
    captured
}

fn cli_monitor_msg_type_filter_body(path: &str, cli: &UbusCli) {
    let captured = cli_monitor_probe_output(path, cli, &["monitor", "-m", "invoke"]);
    let lines: Vec<_> = captured.lines().filter(|line| !line.is_empty()).collect();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("invoke: ") && line.contains(r#""method":"ping""#)),
        "invoke-filtered monitor missing ping INVOKE; stdout: {captured:?}",
    );
    for forbidden in [" data: ", " status: ", " lookup: "] {
        assert!(
            !lines.iter().any(|line| line.contains(forbidden)),
            "invoke-filtered monitor unexpectedly contained {forbidden:?}; stdout: {captured:?}",
        );
    }
}

#[rstest]
#[case::upstream(MatrixArm::Upstream)]
#[case::rs(MatrixArm::Rs)]
fn cli_monitor_msg_type_filter(#[case] arm: MatrixArm) {
    let Some(cli) = locate_cli_or_skip("ubus_cli_cross cli_monitor_msg_type_filter") else {
        return;
    };
    run_matrix_case("ubus_cli_cross cli_monitor_msg_type_filter", arm, &|path| {
        cli_monitor_msg_type_filter_body(path, &cli)
    });
}

fn cli_monitor_direction_filter_body(path: &str, cli: &UbusCli) {
    let received = cli_monitor_probe_output(path, cli, &["monitor", "-M", "r"]);
    let received_lines: Vec<_> = received.lines().filter(|line| !line.is_empty()).collect();
    assert!(
        received_lines.iter().all(|line| line.starts_with("<- ")),
        "receive-filtered monitor emitted a transmit line; stdout: {received:?}",
    );
    assert!(
        received_lines
            .iter()
            .any(|line| line.contains("invoke: ") && line.contains(r#""method":"ping""#)),
        "receive-filtered monitor missing caller ping INVOKE; stdout: {received:?}",
    );

    let transmitted = cli_monitor_probe_output(path, cli, &["monitor", "-M", "t"]);
    let transmitted_lines: Vec<_> = transmitted
        .lines()
        .filter(|line| !line.is_empty())
        .collect();
    assert!(
        transmitted_lines.iter().all(|line| line.starts_with("-> ")),
        "transmit-filtered monitor emitted a receive line; stdout: {transmitted:?}",
    );
    assert!(
        transmitted_lines
            .iter()
            .any(|line| line.contains("invoke: ") || line.contains("data: ")),
        "transmit-filtered monitor missing forwarded INVOKE or DATA; stdout: {transmitted:?}",
    );
}

#[rstest]
#[case::upstream(MatrixArm::Upstream)]
#[case::rs(MatrixArm::Rs)]
fn cli_monitor_direction_filter(#[case] arm: MatrixArm) {
    let Some(cli) = locate_cli_or_skip("ubus_cli_cross cli_monitor_direction_filter") else {
        return;
    };
    run_matrix_case(
        "ubus_cli_cross cli_monitor_direction_filter",
        arm,
        &|path| cli_monitor_direction_filter_body(path, &cli),
    );
}

fn cli_wait_for_blocks_until_object_appears_body(path: &str, cli: &UbusCli) {
    use std::time::Instant;

    const EVENT_OBJECT_ID: u32 = 1;
    let inner = WireTap::spawn(Path::new(path)).expect("spawn inner wait_for tap");
    let inner_path = inner
        .socket_path()
        .to_str()
        .expect("utf-8 inner tap path")
        .to_owned();
    let mut child = cli
        .spawn(
            Path::new(&inner_path),
            &["-t", "5", "wait_for", "example.wf.appears"],
        )
        .expect("spawn ubus wait_for");

    let reg_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let register_seq = inner
            .frames_lenient(TapDirection::ClientToBroker)
            .into_iter()
            .find(|f| {
                f.hdr.ty == MsgType::Invoke
                    && f.hdr.peer == EVENT_OBJECT_ID
                    && find(&f.payload, UbusAttr::Method).and_then(|a| a.as_str())
                        == Some("register")
            })
            .map(|f| f.hdr.seq);
        let registered = register_seq.is_some_and(|seq| {
            inner
                .frames_lenient(TapDirection::BrokerToClient)
                .iter()
                .any(|f| f.hdr.ty == MsgType::Status && f.hdr.seq == seq)
        });
        if registered {
            break;
        }
        if Instant::now() >= reg_deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("ubus wait_for did not register (register INVOKE + STATUS ack) within 5s");
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let owner = OwnerThread::spawn(path.to_owned(), || {
        UbusObject::new("example.wf.appears").method("ping", |_| Ok(BlobMsgTable::build(|_| {})))
    });
    owner.wait_ready();

    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _ = tx.send(child.wait());
    });
    let status = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("ubus wait_for did not exit within 10s")
        .expect("wait for ubus wait_for");
    waiter.join().expect("wait_for wait thread");
    assert!(status.success(), "ubus wait_for exited {:?}", status.code(),);

    owner.shutdown();
    drop(inner);
}

#[rstest]
#[case::upstream(MatrixArm::Upstream)]
#[case::rs(MatrixArm::Rs)]
fn cli_wait_for_blocks_until_object_appears(#[case] arm: MatrixArm) {
    let Some(cli) = locate_cli_or_skip("ubus_cli_cross cli_wait_for_blocks_until_object_appears")
    else {
        return;
    };
    run_matrix_case(
        "ubus_cli_cross cli_wait_for_blocks_until_object_appears",
        arm,
        &|path| cli_wait_for_blocks_until_object_appears_body(path, &cli),
    );
}

fn cli_wait_for_preregistered_object_returns_immediately_body(path: &str, cli: &UbusCli) {
    let owner = OwnerThread::spawn(path.to_owned(), || {
        UbusObject::new("example.wf.ready").method("ping", |_| Ok(BlobMsgTable::build(|_| {})))
    });
    owner.wait_ready();

    let out = cli
        .run(
            Path::new(path),
            &["-t", "3", "wait_for", "example.wf.ready"],
        )
        .expect("run ubus wait_for registered object");
    assert!(
        out.status.success(),
        "ubus wait_for exited {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );

    owner.shutdown();
}

#[rstest]
#[case::upstream(MatrixArm::Upstream)]
#[case::rs(MatrixArm::Rs)]
fn cli_wait_for_preregistered_object_returns_immediately(#[case] arm: MatrixArm) {
    let Some(cli) =
        locate_cli_or_skip("ubus_cli_cross cli_wait_for_preregistered_object_returns_immediately")
    else {
        return;
    };
    run_matrix_case(
        "ubus_cli_cross cli_wait_for_preregistered_object_returns_immediately",
        arm,
        &|path| cli_wait_for_preregistered_object_returns_immediately_body(path, &cli),
    );
}

fn cli_wait_for_missing_object_times_out_nonzero_body(path: &str, cli: &UbusCli) {
    let out = cli
        .run(
            Path::new(path),
            &["-t", "1", "wait_for", "example.wf.never"],
        )
        .expect("run ubus wait_for missing object");
    assert!(
        !out.status.success(),
        "ubus wait_for on a missing object must exit non-zero",
    );
}

#[rstest]
#[case::upstream(MatrixArm::Upstream)]
#[case::rs(MatrixArm::Rs)]
fn cli_wait_for_missing_object_times_out_nonzero(#[case] arm: MatrixArm) {
    let Some(cli) =
        locate_cli_or_skip("ubus_cli_cross cli_wait_for_missing_object_times_out_nonzero")
    else {
        return;
    };
    run_matrix_case(
        "ubus_cli_cross cli_wait_for_missing_object_times_out_nonzero",
        arm,
        &|path| cli_wait_for_missing_object_times_out_nonzero_body(path, &cli),
    );
}

fn cli_subscribe_receives_notification_body(path: &str, cli: &UbusCli) {
    use std::io::{BufRead, BufReader};

    let mut publisher = UnixUbusTransport::connect(path).expect("publisher connect");
    recv_until(&mut publisher, Duration::from_secs(5), |f| {
        f.hdr.ty == MsgType::Hello
    })
    .expect("publisher HELLO");
    let (publisher_obj, _) = raw_add_object_with_signature(
        &mut publisher,
        70,
        "example.subcli",
        &signature_with_ping_method(),
    );

    let mut subscriber = cli
        .spawn(Path::new(path), &["subscribe", "example.subcli"])
        .expect("spawn ubus subscribe");
    recv_until(&mut publisher, Duration::from_secs(5), |f| {
        f.hdr.ty == MsgType::Notify
            && find(&f.payload, UbusAttr::Active).map(|a| a.payload) == Some(&[1][..])
    })
    .expect("publisher Active=1");

    let data = BlobMsgTable::build(|bb| {
        bb.put_i32(Some("value"), 7);
    });
    let mut notify = EnvBuilder::new();
    notify.push_u32(UbusAttr::ObjId, publisher_obj);
    notify.push_str(UbusAttr::Method, "example.notify");
    notify.push_data(data.contents());
    publisher
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Notify,
                seq: 71,
                peer: publisher_obj,
            },
            payload: notify.finish(),
        })
        .expect("send NOTIFY");
    let ack = recv_until(&mut publisher, Duration::from_secs(5), |f| {
        f.hdr.ty == MsgType::Status && f.hdr.seq == 71
    })
    .expect("NOTIFY ack");
    assert_eq!(
        find(&ack.payload, UbusAttr::Status)
            .and_then(|a| a.as_u32())
            .map(|u| u as i32),
        Some(UBUS_STATUS_OK),
        "NOTIFY status",
    );

    let stdout = subscriber.stdout.take().expect("subscriber stdout piped");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut buf = BufReader::new(stdout);
        let mut captured = String::new();
        loop {
            let mut line = String::new();
            match buf.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    captured.push_str(&line);
                    if captured.contains("example.notify") && captured.contains('7') {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(captured);
    });
    let matched = rx.recv_timeout(Duration::from_secs(5)).ok();

    subscriber.kill().expect("kill ubus subscribe");
    let _ = subscriber.wait();
    let _ = reader.join();
    let captured = matched.or_else(|| rx.try_recv().ok()).unwrap_or_default();

    recv_until(&mut publisher, Duration::from_secs(5), |f| {
        f.hdr.ty == MsgType::Notify
            && find(&f.payload, UbusAttr::Active).map(|a| a.payload) == Some(&[0][..])
    })
    .expect("publisher Active=0");

    assert!(
        captured.contains("example.notify"),
        "ubus subscribe missing notification name; stdout: {captured:?}",
    );
    assert!(
        captured.contains('7'),
        "ubus subscribe missing notification payload; stdout: {captured:?}",
    );
}

#[rstest]
#[case::upstream(MatrixArm::Upstream)]
#[case::rs(MatrixArm::Rs)]
fn cli_subscribe_receives_notification(#[case] arm: MatrixArm) {
    let Some(cli) = locate_cli_or_skip("ubus_cli_cross cli_subscribe_receives_notification") else {
        return;
    };
    run_matrix_case(
        "ubus_cli_cross cli_subscribe_receives_notification",
        arm,
        &|path| cli_subscribe_receives_notification_body(path, &cli),
    );
}
