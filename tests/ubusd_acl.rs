//! Cross-broker ACL parity pins.
//!
//! These cases run upstream, rs, a fresh-broker parity arm, and a golden replay
//! arm under a fixed synthetic identity (`testuser`/`testgroup`) supplied by
//! the nss_spoof shim. That makes canonical transcripts machine-independent.
//! Upstream matches ACL entries numerically (resolving names at load time),
//! while rs matches name strings; the shim keeps both views consistent.

#![cfg(feature = "testing")]

use std::time::{Duration, Instant};

use rstest::rstest;
use ubus::blobmsg::{lookup as blob_lookup, BlobBuilder, BlobIter, BlobMsgTable, BlobValue};
use ubus::envelope::{find, EnvBuilder, UbusAttr};
use ubus::server::UbusObject;
use ubus::transport::Transport;
use ubus::wire::{
    Frame, Header, MsgType, PROTO_VERSION, UBUS_STATUS_INVALID_ARGUMENT,
    UBUS_STATUS_INVALID_COMMAND, UBUS_STATUS_NOT_FOUND, UBUS_STATUS_OK,
    UBUS_STATUS_PERMISSION_DENIED,
};
use ubus_testkit::UnixUbusTransport;
use ubus_testkit::{run_matrix_case_with, BrokerOpts, MatrixArm};

#[allow(dead_code)]
mod common;
use common::{AclDir, OwnerThread, TEST_GROUP, TEST_UID, TEST_USER};

fn opts(acl: Option<&AclDir>, peercred: String) -> BrokerOpts {
    BrokerOpts {
        acl_dir: acl.map(|dir| dir.path().to_str().expect("utf-8 acl dir").to_owned()),
        env: vec![
            ("UBUS_TESTKIT_PEERCRED".to_owned(), peercred),
            (
                "UBUS_TESTKIT_NSS_USERS".to_owned(),
                format!("{TEST_UID}:{TEST_USER},0:root"),
            ),
            (
                "UBUS_TESTKIT_NSS_GROUPS".to_owned(),
                format!("{TEST_UID}:{TEST_GROUP},0:root"),
            ),
        ],
    }
}

fn nonroot_opts(acl: Option<&AclDir>) -> BrokerOpts {
    opts(acl, format!("{TEST_UID}:{TEST_UID}:1"))
}

fn run_nonroot(test: &str, arm: MatrixArm, acl: Option<&AclDir>, body: &dyn Fn(&str)) {
    run_matrix_case_with(test, arm, &nonroot_opts(acl), body);
}

fn connect_and_wait_hello(path: &str) -> UnixUbusTransport {
    let mut transport = UnixUbusTransport::connect(path).expect("connect");
    recv_until(&mut transport, Duration::from_secs(5), |frame| {
        frame.hdr.ty == MsgType::Hello
    })
    .expect("HELLO");
    transport
}

fn signature_with_ping_method() -> Vec<u8> {
    let mut sig = BlobBuilder::new();
    sig.open_table(Some("ping"));
    sig.close_table();
    sig.finish()
}

fn add_named(transport: &mut UnixUbusTransport, seq: u16, path: &str) -> u32 {
    let mut env = EnvBuilder::new();
    env.push_str(UbusAttr::ObjPath, path);
    env.push_signature(&signature_with_ping_method());
    transport
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::AddObject,
                seq,
                peer: 0,
            },
            payload: env.finish(),
        })
        .expect("send named ADD_OBJECT");
    expect_data_then_status_ok(transport, seq, "named ADD_OBJECT")
}

fn add_anonymous(transport: &mut UnixUbusTransport, seq: u16) -> u32 {
    transport
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::AddObject,
                seq,
                peer: 0,
            },
            payload: EnvBuilder::new().finish(),
        })
        .expect("send anonymous ADD_OBJECT");
    expect_data_then_status_ok(transport, seq, "anonymous ADD_OBJECT")
}

fn add_named_expect(transport: &mut UnixUbusTransport, seq: u16, path: &str, expected: i32) {
    let mut env = EnvBuilder::new();
    env.push_str(UbusAttr::ObjPath, path);
    env.push_signature(&signature_with_ping_method());
    transport
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::AddObject,
                seq,
                peer: 0,
            },
            payload: env.finish(),
        })
        .expect("send named ADD_OBJECT");
    if expected == UBUS_STATUS_OK {
        let _ = expect_data_then_status_ok(transport, seq, "named ADD_OBJECT");
    } else {
        expect_status_only(transport, seq, expected, "named ADD_OBJECT");
    }
}

fn lookup(path: &str, object: &str) -> u32 {
    let mut transport = connect_and_wait_hello(path);
    let mut env = EnvBuilder::new();
    env.push_str(UbusAttr::ObjPath, object);
    transport
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Lookup,
                seq: 20,
                peer: 0,
            },
            payload: env.finish(),
        })
        .expect("send LOOKUP");
    let data = recv_until(&mut transport, Duration::from_secs(5), |frame| {
        frame.hdr.ty == MsgType::Data && frame.hdr.seq == 20
    })
    .expect("LOOKUP DATA");
    let obj = find(&data.payload, UbusAttr::ObjId)
        .and_then(|attr| attr.as_u32())
        .expect("LOOKUP OBJID");
    expect_status_only(&mut transport, 20, UBUS_STATUS_OK, "LOOKUP");
    obj
}

fn invoke(
    transport: &mut UnixUbusTransport,
    seq: u16,
    obj: u32,
    method: &str,
    data: Option<&[u8]>,
    expected: i32,
) {
    let mut env = EnvBuilder::new();
    env.push_u32(UbusAttr::ObjId, obj);
    env.push_str(UbusAttr::Method, method);
    if let Some(data) = data {
        env.push_data(data);
    }
    transport
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Invoke,
                seq,
                peer: obj,
            },
            payload: env.finish(),
        })
        .expect("send INVOKE");
    if expected == UBUS_STATUS_OK {
        expect_data_then_status_ok_or_empty(transport, seq, "INVOKE");
    } else {
        expect_status_only(transport, seq, expected, "INVOKE");
    }
}

fn subscribe(transport: &mut UnixUbusTransport, obj: u32, target: u32, expected: i32) {
    let mut env = EnvBuilder::new();
    env.push_u32(UbusAttr::ObjId, obj);
    env.push_u32(UbusAttr::Target, target);
    transport
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Subscribe,
                seq: 30,
                peer: 0,
            },
            payload: env.finish(),
        })
        .expect("send SUBSCRIBE");
    expect_status_only(transport, 30, expected, "SUBSCRIBE");
}

fn event_data(method: &str, name: &str, listener: Option<u32>) -> BlobMsgTable {
    BlobMsgTable::build(|builder| match method {
        "send" => {
            builder.put_str(Some("id"), name);
            builder.open_table(Some("data"));
            builder.close_table();
        }
        "register" => {
            builder.put_i32(Some("object"), listener.expect("listener") as i32);
            builder.put_str(Some("pattern"), name);
        }
        _ => unreachable!(),
    })
}

fn named_add_body(path: &str, object: &str, expected: i32) {
    let mut transport = connect_and_wait_hello(path);
    add_named_expect(&mut transport, 1, object, expected);
}

fn access_body(path: &str, object: &str, method: &str, expected: i32) {
    let path = path.to_owned();
    let object_owned = object.to_owned();
    let owner = OwnerThread::spawn(path.clone(), move || {
        UbusObject::new(&object_owned).method("ping", |_| Ok(BlobMsgTable::empty()))
    });
    owner.wait_ready();
    let obj = lookup(&path, object);
    let mut caller = connect_and_wait_hello(&path);
    invoke(&mut caller, 40, obj, method, None, expected);
    owner.shutdown();
}

fn subscribe_body(path: &str, expected: i32) {
    let mut owner = connect_and_wait_hello(path);
    let target = add_named(&mut owner, 50, "pub.obj");
    let mut subscriber = connect_and_wait_hello(path);
    let mine = add_anonymous(&mut subscriber, 51);
    subscribe(&mut subscriber, mine, target, expected);
    if expected == UBUS_STATUS_OK {
        // A successful subscribe leaves broker-minted teardown pending: the
        // owner gets Active=1 now, and on disconnect the publisher's reap
        // races an Unsubscribe into the (possibly half-closed) subscriber
        // socket — the two brokers order the close-vs-reap differently, so a
        // tapped parity capture flaps. Tear down explicitly instead: read
        // Active=1, UNSUBSCRIBE + ack, read Active=0, leaving nothing to race.
        read_active_notify(&mut owner, target, 1);
        unsubscribe(&mut subscriber, mine, target);
        read_active_notify(&mut owner, target, 0);
    }
}

fn read_active_notify(transport: &mut UnixUbusTransport, obj: u32, active: u8) {
    let frame = recv_until(transport, Duration::from_secs(5), |f| {
        f.hdr.ty == MsgType::Notify
            && find(&f.payload, UbusAttr::ObjId).and_then(|a| a.as_u32()) == Some(obj)
            && find(&f.payload, UbusAttr::Active).map(|a| a.payload) == Some(&[active][..])
    })
    .unwrap_or_else(|e| panic!("Active={active} notify: {e}"));
    let _ = frame;
}

fn unsubscribe(transport: &mut UnixUbusTransport, obj: u32, target: u32) {
    let mut env = EnvBuilder::new();
    env.push_u32(UbusAttr::ObjId, obj);
    env.push_u32(UbusAttr::Target, target);
    transport
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Unsubscribe,
                seq: 31,
                peer: 0,
            },
            payload: env.finish(),
        })
        .expect("send UNSUBSCRIBE");
    expect_status_only(transport, 31, UBUS_STATUS_OK, "UNSUBSCRIBE");
}

fn event_body(path: &str, method: &str, name: &str, expected: i32) {
    let mut transport = connect_and_wait_hello(path);
    let listener = (method == "register").then(|| add_anonymous(&mut transport, 60));
    let data = event_data(method, name, listener);
    let mut env = EnvBuilder::new();
    env.push_u32(UbusAttr::ObjId, 1);
    env.push_str(UbusAttr::Method, method);
    env.push_data(data.contents());
    transport
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Invoke,
                seq: 61,
                peer: 1,
            },
            payload: env.finish(),
        })
        .expect("send event INVOKE");
    expect_status_only(&mut transport, 61, expected, "event INVOKE");
}

fn refused_before_hello_body(path: &str) {
    use std::io::Read;

    let mut stream = std::os::unix::net::UnixStream::connect(path).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).expect("refused client read");
    assert_eq!(n, 0, "broker must close before HELLO (EOF with zero bytes)");
}

macro_rules! acl_case {
    ($name:ident, $arm:ident, $body:block) => {
        #[rstest]
        #[case::upstream(MatrixArm::Upstream)]
        #[case::rs(MatrixArm::Rs)]
        #[case::parity(MatrixArm::Parity)]
        #[case::golden(MatrixArm::Golden)]
        fn $name(#[case] $arm: MatrixArm) $body
    };
}

acl_case!(named_add_object_denied_no_acl, arm, {
    let test = "ubusd_acl named_add_object_denied_no_acl";
    run_nonroot(test, arm, None, &|path| {
        named_add_body(path, "example.acl.denied", UBUS_STATUS_INVALID_ARGUMENT)
    });
});
acl_case!(pathless_add_object_allowed_no_acl, arm, {
    let test = "ubusd_acl pathless_add_object_allowed_no_acl";
    run_nonroot(test, arm, None, &|path| {
        let mut t = connect_and_wait_hello(path);
        assert_ne!(add_anonymous(&mut t, 2), 0);
    });
});
acl_case!(lookup_all_allowed_no_acl, arm, {
    let test = "ubusd_acl lookup_all_allowed_no_acl";
    run_nonroot(test, arm, None, &|path| {
        let mut t = connect_and_wait_hello(path);
        t.send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Lookup,
                seq: 3,
                peer: 0,
            },
            payload: EnvBuilder::new().finish(),
        })
        .expect("send list-all LOOKUP");
        expect_status_allowing_data(&mut t, 3, UBUS_STATUS_OK, "list-all LOOKUP");
    });
});
acl_case!(named_add_object_allowed_via_acl, arm, {
    let test = "ubusd_acl named_add_object_allowed_via_acl";
    let acl = AclDir::with_grant(&format!(
        r#"{{"user":"{}","publish":["example.*"]}}"#,
        TEST_USER
    ));
    run_nonroot(test, arm, Some(&acl), &|path| {
        named_add_body(path, "example.acl.allowed", UBUS_STATUS_OK)
    });
});
acl_case!(named_add_object_denied_nonmatching_acl, arm, {
    let test = "ubusd_acl named_add_object_denied_nonmatching_acl";
    let acl = AclDir::with_grant(r#"{"user":"example-no-such-user","publish":["example.*"]}"#);
    run_nonroot(test, arm, Some(&acl), &|path| {
        named_add_body(path, "example.acl.denied", UBUS_STATUS_INVALID_ARGUMENT)
    });
});
acl_case!(publish_exact_match_allows, arm, {
    let test = "ubusd_acl publish_exact_match_allows";
    let acl = AclDir::with_grant(&format!(
        r#"{{"user":"{}","publish":["exact.one"]}}"#,
        TEST_USER
    ));
    run_nonroot(test, arm, Some(&acl), &|path| {
        named_add_body(path, "exact.one", UBUS_STATUS_OK)
    });
});
acl_case!(named_add_object_exact_no_prefix_denied, arm, {
    let test = "ubusd_acl named_add_object_exact_no_prefix_denied";
    let acl = AclDir::with_grant(&format!(
        r#"{{"user":"{}","publish":["exact.one"]}}"#,
        TEST_USER
    ));
    run_nonroot(test, arm, Some(&acl), &|path| {
        named_add_body(path, "exact.one.child", UBUS_STATUS_INVALID_ARGUMENT)
    });
});
acl_case!(invoke_allowed_via_access_grant, arm, {
    let test = "ubusd_acl invoke_allowed_via_access_grant";
    let acl = AclDir::with_grant(&format!(
        r#"{{"user":"{}","publish":["svc.*"],"access":{{"svc.a":{{"methods":["ping"]}}}}}}"#,
        TEST_USER
    ));
    run_nonroot(test, arm, Some(&acl), &|path| {
        access_body(path, "svc.a", "ping", UBUS_STATUS_OK)
    });
});
acl_case!(invoke_denied_for_ungranted_method, arm, {
    let test = "ubusd_acl invoke_denied_for_ungranted_method";
    let acl = AclDir::with_grant(&format!(
        r#"{{"user":"{}","publish":["svc.*"],"access":{{"svc.a":{{"methods":["ping"]}}}}}}"#,
        TEST_USER
    ));
    run_nonroot(test, arm, Some(&acl), &|path| {
        access_body(path, "svc.a", "other", UBUS_STATUS_PERMISSION_DENIED)
    });
});
acl_case!(invoke_allowed_via_star_methods, arm, {
    let test = "ubusd_acl invoke_allowed_via_star_methods";
    let acl = AclDir::with_grant(&format!(
        r#"{{"user":"{}","publish":["svc.*"],"access":{{"svc.b":{{"methods":["*"]}}}}}}"#,
        TEST_USER
    ));
    run_nonroot(test, arm, Some(&acl), &|path| {
        access_body(path, "svc.b", "ping", UBUS_STATUS_OK)
    });
});
acl_case!(subscribe_denied_without_grant, arm, {
    let test = "ubusd_acl subscribe_denied_without_grant";
    let acl = AclDir::with_grant(&format!(
        r#"{{"user":"{}","publish":["pub.obj"]}}"#,
        TEST_USER
    ));
    run_nonroot(test, arm, Some(&acl), &|path| {
        // Probed: upstream masks an un-subscribable (but existing) target
        // as NOT_FOUND rather than PERMISSION_DENIED — unlike the
        // access/listen/send denials, which surface PERMISSION_DENIED.
        subscribe_body(path, UBUS_STATUS_NOT_FOUND)
    });
});
acl_case!(subscribe_allowed_with_grant, arm, {
    let test = "ubusd_acl subscribe_allowed_with_grant";
    let acl = AclDir::with_grant(&format!(
        r#"{{"user":"{}","publish":["pub.obj"],"subscribe":["pub.obj"]}}"#,
        TEST_USER
    ));
    run_nonroot(test, arm, Some(&acl), &|path| {
        subscribe_body(path, UBUS_STATUS_OK)
    });
});
acl_case!(event_send_denied_without_grant, arm, {
    let test = "ubusd_acl event_send_denied_without_grant";
    let acl = AclDir::with_grant(&format!(r#"{{"user":"{}"}}"#, TEST_USER));
    run_nonroot(test, arm, Some(&acl), &|path| {
        event_body(path, "send", "ev.one", UBUS_STATUS_PERMISSION_DENIED)
    });
});
acl_case!(event_send_allowed_with_grant, arm, {
    let test = "ubusd_acl event_send_allowed_with_grant";
    let acl = AclDir::with_grant(&format!(r#"{{"user":"{}","send":["ev.*"]}}"#, TEST_USER));
    run_nonroot(test, arm, Some(&acl), &|path| {
        event_body(path, "send", "ev.one", UBUS_STATUS_OK)
    });
});
acl_case!(event_listen_denied_without_grant, arm, {
    let test = "ubusd_acl event_listen_denied_without_grant";
    let acl = AclDir::with_grant(&format!(r#"{{"user":"{}"}}"#, TEST_USER));
    run_nonroot(test, arm, Some(&acl), &|path| {
        event_body(path, "register", "ev.one", UBUS_STATUS_PERMISSION_DENIED)
    });
});
acl_case!(event_listen_allowed_with_grant, arm, {
    let test = "ubusd_acl event_listen_allowed_with_grant";
    let acl = AclDir::with_grant(&format!(r#"{{"user":"{}","listen":["ev.*"]}}"#, TEST_USER));
    run_nonroot(test, arm, Some(&acl), &|path| {
        event_body(path, "register", "ev.one", UBUS_STATUS_OK)
    });
});
acl_case!(group_grant_allows, arm, {
    let test = "ubusd_acl group_grant_allows";
    let acl = AclDir::with_grant(&format!(
        r#"{{"group":"{}","publish":["grp.*"]}}"#,
        TEST_GROUP
    ));
    run_nonroot(test, arm, Some(&acl), &|path| {
        named_add_body(path, "grp.one", UBUS_STATUS_OK)
    });
});
acl_case!(unowned_acl_file_ignored, arm, {
    let test = "ubusd_acl unowned_acl_file_ignored";
    let acl = AclDir::with_grant(&format!(
        r#"{{"user":"{}","publish":["example.*"]}}"#,
        TEST_USER
    ));
    let mut options = nonroot_opts(Some(&acl));
    options.env.push((
        "UBUS_TESTKIT_STAT_ROOT_PREFIX".to_owned(),
        "/definitely/not/the/acl/dir".to_owned(),
    ));
    run_matrix_case_with(test, arm, &options, &|path| {
        named_add_body(path, "example.acl.denied", UBUS_STATUS_INVALID_ARGUMENT)
    });
});
acl_case!(malformed_acl_file_skipped_sibling_loads, arm, {
    let test = "ubusd_acl malformed_acl_file_skipped_sibling_loads";
    let good = format!(r#"{{"user":"{}","publish":["example.*"]}}"#, TEST_USER);
    let acl = AclDir::with_files(&[("broken.json", "not json"), ("good.json", &good)]);
    run_nonroot(test, arm, Some(&acl), &|path| {
        named_add_body(path, "example.acl.good", UBUS_STATUS_OK)
    });
});
acl_case!(acl_object_invoke_reports_invalid_command, arm, {
    let test = "ubusd_acl acl_object_invoke_reports_invalid_command";
    run_nonroot(test, arm, None, &|path| {
        let mut t = connect_and_wait_hello(path);
        invoke(&mut t, 70, 2, "get", None, UBUS_STATUS_INVALID_COMMAND);
    });
});
acl_case!(acl_query_reports_seq_and_matching_entries, arm, {
    let test = "ubusd_acl acl_query_reports_seq_and_matching_entries";
    let acl = AclDir::with_grant(&format!(
        r#"{{"user":"{}","publish":["svc.*"],"access":{{"svc.*":{{"methods":["ping"],"acl":{{"custom":"tag","n":7}}}}}}}}"#,
        TEST_USER
    ));
    run_nonroot(test, arm, Some(&acl), &|path| {
        let mut transport = connect_and_wait_hello(path);
        add_named(&mut transport, 70, "svc.a");
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, 2);
        env.push_str(UbusAttr::Method, "query");
        transport
            .send(&Frame {
                hdr: Header {
                    version: PROTO_VERSION,
                    ty: MsgType::Invoke,
                    seq: 71,
                    peer: 2,
                },
                payload: env.finish(),
            })
            .expect("send ACL query");
        let data = recv_until(&mut transport, Duration::from_secs(5), |frame| {
            frame.hdr.ty == MsgType::Data && frame.hdr.seq == 71
        })
        .expect("ACL query DATA");
        assert_eq!(data.hdr.peer, 2);
        assert_eq!(
            find(&data.payload, UbusAttr::ObjId).and_then(|attr| attr.as_u32()),
            Some(2)
        );
        let query = find(&data.payload, UbusAttr::Data)
            .expect("query data")
            .payload;
        assert_eq!(
            blob_lookup(query, "seq").and_then(|attr| attr.value().ok()),
            Some(BlobValue::Int32(1))
        );
        let acl_array = blob_lookup(query, "acl").expect("acl array").payload;
        let element = BlobIter::new(acl_array).next().expect("one acl element");
        assert!(BlobIter::new(acl_array).nth(1).is_none(), "one acl element");
        let BlobValue::Table(entry) = element.value().expect("acl table") else {
            panic!("acl element table")
        };
        assert_eq!(
            blob_lookup(entry, "obj").and_then(|attr| attr.as_str()),
            Some("svc.a")
        );
        assert_eq!(
            blob_lookup(entry, "user").and_then(|attr| attr.as_str()),
            Some(TEST_USER)
        );
        let privs = blob_lookup(entry, "acl").expect("priv table").payload;
        assert_eq!(
            blob_lookup(privs, "custom").and_then(|attr| attr.as_str()),
            Some("tag")
        );
        assert_eq!(
            blob_lookup(privs, "n").and_then(|attr| attr.value().ok()),
            Some(BlobValue::Int32(7))
        );
        expect_status_only(&mut transport, 71, UBUS_STATUS_OK, "ACL query");
    });
});
acl_case!(acl_query_with_no_owned_objects_reports_empty_acl, arm, {
    let test = "ubusd_acl acl_query_with_no_owned_objects_reports_empty_acl";
    let acl = AclDir::with_grant(&format!(
        r#"{{"user":"{}","publish":["svc.*"],"access":{{"svc.*":{{"methods":["ping"],"acl":{{"custom":"tag","n":7}}}}}}}}"#,
        TEST_USER
    ));
    run_nonroot(test, arm, Some(&acl), &|path| {
        let mut transport = connect_and_wait_hello(path);
        let mut env = EnvBuilder::new();
        env.push_u32(UbusAttr::ObjId, 2);
        env.push_str(UbusAttr::Method, "query");
        transport
            .send(&Frame {
                hdr: Header {
                    version: PROTO_VERSION,
                    ty: MsgType::Invoke,
                    seq: 72,
                    peer: 2,
                },
                payload: env.finish(),
            })
            .expect("send ACL query");
        let data = recv_until(&mut transport, Duration::from_secs(5), |frame| {
            frame.hdr.ty == MsgType::Data && frame.hdr.seq == 72
        })
        .expect("ACL query DATA");
        let query = find(&data.payload, UbusAttr::Data)
            .expect("query data")
            .payload;
        assert_eq!(
            blob_lookup(query, "seq").and_then(|attr| attr.value().ok()),
            Some(BlobValue::Int32(1))
        );
        assert!(
            BlobIter::new(blob_lookup(query, "acl").expect("acl array").payload)
                .next()
                .is_none()
        );
        expect_status_only(&mut transport, 72, UBUS_STATUS_OK, "ACL query");
    });
});
acl_case!(subscribe_to_acl_object_reports_not_found, arm, {
    let test = "ubusd_acl subscribe_to_acl_object_reports_not_found";
    run_nonroot(test, arm, None, &|path| {
        let mut transport = connect_and_wait_hello(path);
        let listener = add_anonymous(&mut transport, 73);
        subscribe(&mut transport, listener, 2, UBUS_STATUS_NOT_FOUND);
    });
});
acl_case!(root_bypasses_acl, arm, {
    let test = "ubusd_acl root_bypasses_acl";
    let acl = AclDir::with_grant(r#"{"user":"example-no-such-user","publish":["example.*"]}"#);
    run_matrix_case_with(test, arm, &opts(Some(&acl), "0:0:1".to_owned()), &|path| {
        named_add_body(path, "example.acl.root", UBUS_STATUS_OK)
    });
});
acl_case!(unresolvable_uid_client_refused_before_hello, arm, {
    let test = "ubusd_acl unresolvable_uid_client_refused_before_hello";
    let mut options = nonroot_opts(None);
    options.env.push((
        "UBUS_TESTKIT_NSS_FAIL_UIDS".to_owned(),
        TEST_UID.to_string(),
    ));
    run_matrix_case_with(test, arm, &options, &refused_before_hello_body);
});
acl_case!(unresolvable_gid_client_refused_before_hello, arm, {
    let test = "ubusd_acl unresolvable_gid_client_refused_before_hello";
    let mut options = nonroot_opts(None);
    options.env.push((
        "UBUS_TESTKIT_NSS_FAIL_GIDS".to_owned(),
        TEST_UID.to_string(),
    ));
    run_matrix_case_with(test, arm, &options, &refused_before_hello_body);
});
acl_case!(peercred_read_failure_client_refused, arm, {
    let test = "ubusd_acl peercred_read_failure_client_refused";
    let mut options = opts(None, "1000:1000:1".to_owned());
    options
        .env
        .push(("UBUS_TESTKIT_PEERCRED_FAIL".to_owned(), "1".to_owned()));
    run_matrix_case_with(test, arm, &options, &refused_before_hello_body);
});

fn expect_data_then_status_ok(transport: &mut UnixUbusTransport, seq: u16, what: &str) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut obj = None;
    while Instant::now() < deadline {
        match transport.recv() {
            Ok(Some(frame)) if frame.hdr.seq == seq && frame.hdr.ty == MsgType::Data => {
                obj = find(&frame.payload, UbusAttr::ObjId).and_then(|attr| attr.as_u32())
            }
            Ok(Some(frame)) if frame.hdr.seq == seq && frame.hdr.ty == MsgType::Status => {
                assert_eq!(status_value(&frame), Some(UBUS_STATUS_OK), "{what}");
                return obj.expect("ADD_OBJECT DATA OBJID");
            }
            Ok(Some(_)) => {}
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(error) => panic!("{what} recv: {error:?}"),
        }
    }
    panic!("{what}: DATA-then-STATUS timed out")
}

fn expect_data_then_status_ok_or_empty(transport: &mut UnixUbusTransport, seq: u16, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut data = false;
    while Instant::now() < deadline {
        match transport.recv() {
            Ok(Some(frame)) if frame.hdr.seq == seq && frame.hdr.ty == MsgType::Data => data = true,
            Ok(Some(frame)) if frame.hdr.seq == seq && frame.hdr.ty == MsgType::Status => {
                assert_eq!(status_value(&frame), Some(UBUS_STATUS_OK), "{what}");
                assert!(data, "{what} must return DATA");
                return;
            }
            Ok(Some(_)) => {}
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(error) => panic!("{what} recv: {error:?}"),
        }
    }
    panic!("{what}: DATA-then-STATUS timed out")
}

fn expect_status_only(transport: &mut UnixUbusTransport, seq: u16, expected: i32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match transport.recv() {
            Ok(Some(frame)) if frame.hdr.seq == seq && frame.hdr.ty == MsgType::Data => {
                panic!("{what} must not return DATA")
            }
            Ok(Some(frame)) if frame.hdr.seq == seq && frame.hdr.ty == MsgType::Status => {
                assert_eq!(status_value(&frame), Some(expected), "{what}");
                return;
            }
            Ok(Some(_)) => {}
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(error) => panic!("{what} recv: {error:?}"),
        }
    }
    panic!("{what}: STATUS timed out")
}

fn expect_status_allowing_data(
    transport: &mut UnixUbusTransport,
    seq: u16,
    expected: i32,
    what: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match transport.recv() {
            Ok(Some(frame)) if frame.hdr.seq == seq && frame.hdr.ty == MsgType::Status => {
                assert_eq!(status_value(&frame), Some(expected), "{what}");
                return;
            }
            Ok(Some(_)) => {}
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(error) => panic!("{what} recv: {error:?}"),
        }
    }
    panic!("{what}: STATUS timed out")
}

fn status_value(frame: &Frame) -> Option<i32> {
    find(&frame.payload, UbusAttr::Status)
        .and_then(|attr| attr.as_u32())
        .map(|value| value as i32)
}

fn recv_until(
    transport: &mut UnixUbusTransport,
    timeout: Duration,
    pred: impl Fn(&Frame) -> bool,
) -> Result<Frame, String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match transport.recv() {
            Ok(Some(frame)) if pred(&frame) => return Ok(frame),
            Ok(Some(_)) => {}
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(error) => return Err(format!("recv: {error:?}")),
        }
    }
    Err("recv_until: timeout".to_owned())
}
