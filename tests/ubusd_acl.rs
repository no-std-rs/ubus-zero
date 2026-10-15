//! Cross-broker ACL parity pins.
//!
//! These cases run upstream, rs, a fresh-broker parity arm, and a golden replay
//! arm under a fixed synthetic identity (`testuser`/`testgroup`) supplied by
//! the nss_spoof shim. That makes canonical transcripts machine-independent.
//! Both brokers resolve ACL names at load time and check numeric identities,
//! including supplementary groups. Each broker child has its own NSS/proc
//! fixtures, so unrelated cases can run concurrently without host accounts.

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
            ("UBUS_TESTKIT_NSS_EXTRA_GIDS".to_owned(), String::new()),
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

// Exercise the loader through a real process as well as the pure ACL unit
// tests: ownership, NSS resolution and typed JSON policy all affect whether
// these grants ever reach the authorization checks.
macro_rules! publish_rule_case {
    ($name:ident, $grant:expr, $object:expr, $expected:expr) => {
        acl_case!($name, arm, {
            let acl = AclDir::with_grant($grant);
            run_nonroot(
                concat!("ubusd_acl ", stringify!($name)),
                arm,
                Some(&acl),
                &|path| named_add_body(path, $object, $expected),
            );
        });
    };
}

publish_rule_case!(
    publish_star_matches_any_path,
    r#"{"user":"testuser","publish":["*"]}"#,
    "any.object",
    UBUS_STATUS_OK
);
publish_rule_case!(
    publish_prefix_matches_empty_suffix,
    r#"{"user":"testuser","publish":["prefix.*"]}"#,
    "prefix.",
    UBUS_STATUS_OK
);
publish_rule_case!(
    publish_interior_star_is_literal,
    r#"{"user":"testuser","publish":["prefix.*.tail"]}"#,
    "prefix.any.tail",
    UBUS_STATUS_INVALID_ARGUMENT
);
publish_rule_case!(
    publish_question_mark_is_literal,
    r#"{"user":"testuser","publish":["prefix.?ne"]}"#,
    "prefix.one",
    UBUS_STATUS_INVALID_ARGUMENT
);
publish_rule_case!(
    publish_without_identity_is_denied,
    r#"{"publish":["*"]}"#,
    "identity.required",
    UBUS_STATUS_INVALID_ARGUMENT
);
publish_rule_case!(
    publish_user_takes_precedence_over_group,
    r#"{"user":"no-such-ubus-test-user","group":"testgroup","publish":["*"]}"#,
    "identity.precedence",
    UBUS_STATUS_INVALID_ARGUMENT
);
publish_rule_case!(
    publish_user_with_unmatched_group_is_allowed,
    r#"{"user":"testuser","group":"no-such-ubus-test-group","publish":["*"]}"#,
    "identity.user",
    UBUS_STATUS_OK
);
publish_rule_case!(
    publish_mixed_type_array_ignores_non_strings,
    r#"{"user":"testuser","publish":[7,null,true,{},[],"mixed.allowed"]}"#,
    "mixed.allowed",
    UBUS_STATUS_OK
);
publish_rule_case!(
    wrong_type_access_does_not_discard_publish_grant,
    r#"{"user":"testuser","publish":["typed.allowed"],"access":[]}"#,
    "typed.allowed",
    UBUS_STATUS_OK
);
publish_rule_case!(
    wrong_type_subscribe_does_not_discard_publish_grant,
    r#"{"user":"testuser","publish":["typed.allowed"],"subscribe":"*"}"#,
    "typed.allowed",
    UBUS_STATUS_OK
);
publish_rule_case!(
    wrong_type_publish_is_not_a_grant,
    r#"{"user":"testuser","publish":"*"}"#,
    "typed.denied",
    UBUS_STATUS_INVALID_ARGUMENT
);
publish_rule_case!(
    send_grant_does_not_allow_publish,
    r#"{"user":"testuser","send":["*"]}"#,
    "operation.independent",
    UBUS_STATUS_INVALID_ARGUMENT
);
publish_rule_case!(
    empty_pattern_does_not_allow_nonempty_path,
    r#"{"user":"testuser","publish":[""]}"#,
    "pattern.nonempty",
    UBUS_STATUS_INVALID_ARGUMENT
);

publish_rule_case!(
    wrong_type_user_allows_valid_group_identity,
    r#"{"user":42,"group":"testgroup","publish":["*"]}"#,
    "identity.group",
    UBUS_STATUS_OK
);

publish_rule_case!(
    duplicate_root_user_uses_last_identity,
    r#"{"user":"no-such-ubus-test-user","user":"testuser","publish":["root.*"]}"#,
    "root.user",
    UBUS_STATUS_OK
);
publish_rule_case!(
    duplicate_root_group_uses_last_identity,
    r#"{"group":"no-such-ubus-test-group","group":"testgroup","publish":["root.*"]}"#,
    "root.group",
    UBUS_STATUS_OK
);
publish_rule_case!(
    duplicate_root_publish_uses_last_array,
    r#"{"user":"testuser","publish":["denied.*"],"publish":["root.*"]}"#,
    "root.publish",
    UBUS_STATUS_OK
);

acl_case!(duplicate_root_access_last_value_revokes, arm, {
    let acl = AclDir::with_grant(
        r#"{"user":"testuser","publish":["svc.*"],"access":{"svc.*":{"methods":["ping"]}},"access":{}}"#,
    );
    run_nonroot(
        "ubusd_acl duplicate_root_access_last_value_revokes",
        arm,
        Some(&acl),
        &|path| {
            let mut owner = connect_and_wait_hello(path);
            let object = add_named(&mut owner, 1, "svc.rules");
            let mut caller = connect_and_wait_hello(path);
            invoke(
                &mut caller,
                2,
                object,
                "ping",
                None,
                UBUS_STATUS_PERMISSION_DENIED,
            );
        },
    );
});

macro_rules! access_rule_case {
    ($name:ident, $access:expr, $expected:expr) => {
        acl_case!($name, arm, {
            let grant = format!(
                r#"{{"user":"testuser","publish":["svc.*"],"access":{}}}"#,
                $access
            );
            let acl = AclDir::with_grant(&grant);
            run_nonroot(
                concat!("ubusd_acl ", stringify!($name)),
                arm,
                Some(&acl),
                &|path| {
                    if $expected == UBUS_STATUS_OK {
                        access_body(path, "svc.rules", "ping", $expected)
                    } else {
                        // An unauthorized object is hidden from LOOKUP, so take
                        // its id from the owner's ADD_OBJECT reply instead.
                        let mut owner = connect_and_wait_hello(path);
                        let obj = add_named(&mut owner, 1, "svc.rules");
                        let mut caller = connect_and_wait_hello(path);
                        invoke(&mut caller, 2, obj, "ping", None, $expected);
                    }
                },
            );
        });
    };
}

access_rule_case!(
    access_object_prefix_allows_method,
    r#"{"svc.*":{"methods":["ping"]}}"#,
    UBUS_STATUS_OK
);
access_rule_case!(
    access_empty_methods_denies,
    r#"{"svc.rules":{"methods":[]}}"#,
    UBUS_STATUS_PERMISSION_DENIED
);
access_rule_case!(
    access_wrong_type_methods_denies,
    r#"{"svc.rules":{"methods":"*"}}"#,
    UBUS_STATUS_PERMISSION_DENIED
);
access_rule_case!(
    access_method_prefix_is_not_a_wildcard,
    r#"{"svc.rules":{"methods":["pi*"]}}"#,
    UBUS_STATUS_PERMISSION_DENIED
);
access_rule_case!(
    access_mixed_methods_ignores_non_strings,
    r#"{"svc.rules":{"methods":[4,true,null,"ping"]}}"#,
    UBUS_STATUS_OK
);
access_rule_case!(
    access_tags_do_not_allow_invocation,
    r#"{"svc.rules":{"tags":["ping"]}}"#,
    UBUS_STATUS_PERMISSION_DENIED
);
access_rule_case!(
    access_privileges_do_not_allow_invocation,
    r#"{"svc.rules":{"acl":{"ping":true}}}"#,
    UBUS_STATUS_PERMISSION_DENIED
);
access_rule_case!(
    access_exact_object_does_not_match_child,
    r#"{"svc":{"methods":["*"]}}"#,
    UBUS_STATUS_PERMISSION_DENIED
);

access_rule_case!(
    access_duplicate_object_last_rule_revokes,
    r#"{"svc.rules":{"methods":["ping"]},"svc.rules":{"methods":[]}}"#,
    UBUS_STATUS_PERMISSION_DENIED
);
access_rule_case!(
    access_duplicate_object_last_rule_allows,
    r#"{"svc.rules":{"methods":[]},"svc.rules":{"methods":["ping"]}}"#,
    UBUS_STATUS_OK
);
access_rule_case!(
    access_duplicate_methods_last_wrong_type_revokes,
    r#"{"svc.rules":{"methods":["ping"],"methods":"*"}}"#,
    UBUS_STATUS_PERMISSION_DENIED
);
access_rule_case!(
    access_duplicate_methods_last_array_allows,
    r#"{"svc.rules":{"methods":[],"methods":["ping"]}}"#,
    UBUS_STATUS_OK
);

acl_case!(separate_acl_files_combine_publish_and_access, arm, {
    let acl = AclDir::with_files(&[
        (
            "10-publish.json",
            r#"{"user":"testuser","publish":["svc.*"]}"#,
        ),
        (
            "20-access.json",
            r#"{"group":"testgroup","access":{"svc.*":{"methods":["ping"]}}}"#,
        ),
    ]);
    run_nonroot(
        "ubusd_acl separate_acl_files_combine_publish_and_access",
        arm,
        Some(&acl),
        &|path| access_body(path, "svc.combined", "ping", UBUS_STATUS_OK),
    );
});

acl_case!(non_json_filename_does_not_load_grant, arm, {
    let acl = AclDir::with_files(&[("grant.txt", r#"{"user":"testuser","publish":["*"]}"#)]);
    run_nonroot(
        "ubusd_acl non_json_filename_does_not_load_grant",
        arm,
        Some(&acl),
        &|path| named_add_body(path, "filename.denied", UBUS_STATUS_INVALID_ARGUMENT),
    );
});

macro_rules! ownership_case {
    ($name:ident, $key:literal, $value:literal, $expected:expr) => {
        acl_case!($name, arm, {
            let acl = AclDir::with_grant(r#"{"user":"testuser","publish":["safe.*"]}"#);
            let mut options = nonroot_opts(Some(&acl));
            options.env.push(($key.to_owned(), $value.to_owned()));
            run_matrix_case_with(
                concat!("ubusd_acl ", stringify!($name)),
                arm,
                &options,
                &|path| named_add_body(path, "safe.object", $expected),
            );
        });
    };
}

ownership_case!(
    nonroot_acl_owner_is_rejected,
    "UBUS_TESTKIT_STAT_UID",
    "1000",
    UBUS_STATUS_INVALID_ARGUMENT
);
ownership_case!(
    nonroot_acl_group_is_rejected,
    "UBUS_TESTKIT_STAT_GID",
    "1000",
    UBUS_STATUS_INVALID_ARGUMENT
);
ownership_case!(
    group_writable_acl_is_rejected,
    "UBUS_TESTKIT_STAT_MODE",
    "0664",
    UBUS_STATUS_INVALID_ARGUMENT
);
ownership_case!(
    other_writable_acl_is_rejected,
    "UBUS_TESTKIT_STAT_MODE",
    "0646",
    UBUS_STATUS_INVALID_ARGUMENT
);
ownership_case!(
    other_executable_acl_is_rejected,
    "UBUS_TESTKIT_STAT_MODE",
    "0645",
    UBUS_STATUS_INVALID_ARGUMENT
);
ownership_case!(
    owner_executable_acl_is_allowed,
    "UBUS_TESTKIT_STAT_MODE",
    "0744",
    UBUS_STATUS_OK
);
ownership_case!(
    group_executable_acl_is_allowed,
    "UBUS_TESTKIT_STAT_MODE",
    "0654",
    UBUS_STATUS_OK
);

macro_rules! supplementary_group_case {
    ($name:ident, $extra:expr, $expected:expr) => {
        acl_case!($name, arm, {
            let acl = AclDir::with_grant(r#"{"group":"secondary","publish":["groups.*"]}"#);
            let mut options = nonroot_opts(Some(&acl));
            options.env.push((
                "UBUS_TESTKIT_NSS_GROUPS".to_owned(),
                "1000:testgroup,2000:secondary,0:root".to_owned(),
            ));
            options
                .env
                .push(("UBUS_TESTKIT_NSS_EXTRA_GIDS".to_owned(), $extra.to_owned()));
            run_matrix_case_with(
                concat!("ubusd_acl ", stringify!($name)),
                arm,
                &options,
                &|path| named_add_body(path, "groups.object", $expected),
            );
        });
    };
}

supplementary_group_case!(supplementary_group_grant_allows, "2000", UBUS_STATUS_OK);
supplementary_group_case!(
    supplementary_duplicate_group_grant_allows,
    "1000 2000 2000",
    UBUS_STATUS_OK
);
supplementary_group_case!(
    empty_supplementary_groups_do_not_grant,
    "",
    UBUS_STATUS_INVALID_ARGUMENT
);
supplementary_group_case!(
    unmatched_supplementary_groups_do_not_grant,
    "3000 4000",
    UBUS_STATUS_INVALID_ARGUMENT
);

supplementary_group_case!(
    supplementary_groups_stop_at_invalid_token,
    "3000 invalid 2000",
    UBUS_STATUS_INVALID_ARGUMENT
);
supplementary_group_case!(
    valid_supplementary_group_before_invalid_token_allows,
    "2000 invalid",
    UBUS_STATUS_OK
);
supplementary_group_case!(
    supplementary_group_at_limit_allows,
    format!("{}2000", "3000 ".repeat(63)),
    UBUS_STATUS_OK
);
supplementary_group_case!(
    supplementary_group_beyond_limit_denies,
    format!("{}2000", "3000 ".repeat(64)),
    UBUS_STATUS_INVALID_ARGUMENT
);

macro_rules! identity_alias_case {
    ($name:ident, $grant:literal, $key:literal, $mapping:literal) => {
        acl_case!($name, arm, {
            let acl = AclDir::with_grant($grant);
            let mut options = nonroot_opts(Some(&acl));
            options.env.push(($key.to_owned(), $mapping.to_owned()));
            run_matrix_case_with(
                concat!("ubusd_acl ", stringify!($name)),
                arm,
                &options,
                &|path| named_add_body(path, "aliases.object", UBUS_STATUS_OK),
            );
        });
    };
}

identity_alias_case!(
    user_alias_matches_numeric_uid,
    r#"{"user":"alias","publish":["aliases.*"]}"#,
    "UBUS_TESTKIT_NSS_USERS",
    "1000:testuser,1000:alias,0:root"
);
identity_alias_case!(
    group_alias_matches_numeric_gid,
    r#"{"group":"alias","publish":["aliases.*"]}"#,
    "UBUS_TESTKIT_NSS_GROUPS",
    "1000:testgroup,1000:alias,0:root"
);

acl_case!(unreadable_proc_status_keeps_primary_group_grants, arm, {
    let acl = AclDir::with_grant(r#"{"group":"testgroup","publish":["groups.*"]}"#);
    let mut options = nonroot_opts(Some(&acl));
    options
        .env
        .push(("UBUS_TESTKIT_NSS_PROC_FAIL".to_owned(), "1".to_owned()));
    run_matrix_case_with(
        "ubusd_acl unreadable_proc_status_keeps_primary_group_grants",
        arm,
        &options,
        &|path| named_add_body(path, "groups.primary", UBUS_STATUS_OK),
    );
});

acl_case!(primary_gid_can_differ_from_uid, arm, {
    let acl = AclDir::with_grant(r#"{"group":"secondary","publish":["groups.*"]}"#);
    let mut options = opts(Some(&acl), "1000:2000:1".to_owned());
    options.env.push((
        "UBUS_TESTKIT_NSS_GROUPS".to_owned(),
        "2000:secondary,0:root".to_owned(),
    ));
    run_matrix_case_with(
        "ubusd_acl primary_gid_can_differ_from_uid",
        arm,
        &options,
        &|path| named_add_body(path, "groups.primary", UBUS_STATUS_OK),
    );
});

fn visibility_body(path: &str, query: Option<&str>, methods: &[&str], empty_signature: bool) {
    let mut owner = connect_and_wait_hello(path);
    let mut signature = BlobBuilder::new();
    if !empty_signature {
        for method in ["ping", "secret"] {
            signature.open_table(Some(method));
            signature.put_i32(Some("argument"), 3);
            signature.close_table();
        }
    }
    let signature = signature.finish();
    let (object, object_type) =
        common::raw_add_object_with_signature(&mut owner, 1, "visible.object", &signature);
    let mut caller = connect_and_wait_hello(path);
    let mut env = EnvBuilder::new();
    if let Some(query) = query {
        env.push_str(UbusAttr::ObjPath, query);
    }
    caller
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Lookup,
                seq: 2,
                peer: 0,
            },
            payload: env.finish(),
        })
        .expect("LOOKUP visibility");
    if !methods.is_empty() || empty_signature {
        let data = recv_until(&mut caller, Duration::from_secs(5), |frame| {
            frame.hdr.seq == 2 && frame.hdr.ty == MsgType::Data
        })
        .expect("visible object DATA");
        assert_eq!(
            find(&data.payload, UbusAttr::ObjId).and_then(|a| a.as_u32()),
            Some(object)
        );
        assert_eq!(
            find(&data.payload, UbusAttr::ObjType).and_then(|a| a.as_u32()),
            Some(object_type)
        );
        let visible = find(&data.payload, UbusAttr::Signature)
            .expect("visible signature")
            .payload;
        assert_eq!(
            BlobIter::new(visible)
                .map(|a| a.name.expect("method name"))
                .collect::<Vec<_>>(),
            methods
        );
        // Verify method policies survive filtering without re-encoding.
        for method in BlobIter::new(visible) {
            assert_eq!(
                blob_lookup(method.payload, "argument").and_then(|a| a.value().ok()),
                Some(BlobValue::Int32(3))
            );
        }
    }
    // A matching path still terminates with OK when ACL hides every method.
    expect_status_only(&mut caller, 2, UBUS_STATUS_OK, "filtered LOOKUP");
}

macro_rules! visibility_case {
    ($name:ident, $access:literal, $query:expr, $methods:expr, $empty:expr) => {
        acl_case!($name, arm, {
            let grant = format!(
                r#"{{"user":"testuser","publish":["visible.*"],"access":{}}}"#,
                $access
            );
            let acl = AclDir::with_grant(&grant);
            run_nonroot(
                concat!("ubusd_acl ", stringify!($name)),
                arm,
                Some(&acl),
                &|path| visibility_body(path, $query, $methods, $empty),
            );
        });
    };
}

visibility_case!(
    lookup_exact_hides_denied_methods,
    "{}",
    Some("visible.object"),
    &[],
    false
);
visibility_case!(
    lookup_prefix_hides_denied_methods,
    "{}",
    Some("visible.*"),
    &[],
    false
);
visibility_case!(lookup_all_hides_denied_methods, "{}", None, &[], false);
visibility_case!(
    lookup_exact_filters_method_signature,
    r#"{"visible.*":{"methods":["ping"]}}"#,
    Some("visible.object"),
    &["ping"],
    false
);
visibility_case!(
    lookup_prefix_filters_method_signature,
    r#"{"visible.*":{"methods":["ping"]}}"#,
    Some("visible.*"),
    &["ping"],
    false
);
visibility_case!(
    lookup_all_filters_method_signature,
    r#"{"visible.*":{"methods":["ping"]}}"#,
    None,
    &["ping"],
    false
);
visibility_case!(
    lookup_star_access_retains_method_order,
    r#"{"visible.*":{"methods":["*"]}}"#,
    Some("visible.object"),
    &["ping", "secret"],
    false
);
visibility_case!(
    lookup_empty_signature_remains_visible_without_access,
    "{}",
    Some("visible.object"),
    &[],
    true
);

// The pinned reference maps failed ACL name resolution to numeric zero.
// Upstream fixed that unsafe invalid-configuration fallback in 008655958eb4.
// Keep this Rust-only regression separate from valid-policy parity: a typo
// must never grant permission to non-root clients carrying group zero.
#[rstest]
#[case::unknown_group_primary_zero(
    "group",
    "missinggroup",
    "1000:0:1",
    "",
    UBUS_STATUS_INVALID_ARGUMENT
)]
#[case::unknown_group_supplementary_zero(
    "group",
    "missinggroup",
    "1000:1000:1",
    "0",
    UBUS_STATUS_INVALID_ARGUMENT
)]
#[case::unknown_user("user", "missinguser", "1000:0:1", "0", UBUS_STATUS_INVALID_ARGUMENT)]
#[case::resolved_primary_zero("group", "root", "1000:0:1", "", UBUS_STATUS_OK)]
#[case::resolved_supplementary_zero("group", "root", "1000:1000:1", "0", UBUS_STATUS_OK)]
#[case::resolved_zero_alias("group", "root-alias", "1000:0:1", "", UBUS_STATUS_OK)]
#[case::root_bypasses_unknown_group("group", "missinggroup", "0:0:1", "", UBUS_STATUS_OK)]
fn acl_identity_resolution_preserves_root_group_boundary(
    #[case] principal: &str,
    #[case] name: &str,
    #[case] credentials: &str,
    #[case] extra_gids: &str,
    #[case] expected: i32,
) {
    let acl = AclDir::with_grant(&format!(
        r#"{{"{principal}":"{name}","publish":["security.*"]}}"#
    ));
    let mut options = opts(Some(&acl), credentials.to_owned());
    options.env.extend([
        (
            "UBUS_TESTKIT_NSS_USERS".to_owned(),
            "1000:testuser,0:root,4242:missinguser".to_owned(),
        ),
        (
            "UBUS_TESTKIT_NSS_GROUPS".to_owned(),
            "1000:testgroup,0:root,0:root-alias,4242:missinggroup".to_owned(),
        ),
        ("UBUS_TESTKIT_NSS_FAIL_UIDS".to_owned(), "4242".to_owned()),
        ("UBUS_TESTKIT_NSS_FAIL_GIDS".to_owned(), "4242".to_owned()),
        (
            "UBUS_TESTKIT_NSS_EXTRA_GIDS".to_owned(),
            extra_gids.to_owned(),
        ),
    ]);
    run_matrix_case_with(
        "ubusd_acl acl_identity_resolution_preserves_root_group_boundary",
        MatrixArm::Rs,
        &options,
        &|path| named_add_body(path, "security.object", expected),
    );
}

fn signature_shape(shape: &str) -> (Vec<u8>, Vec<u8>) {
    let valid = signature_with_ping_method();
    if let Some(integer) = shape.strip_prefix("integer-") {
        let (kind, width, variant) =
            match integer.split_once('-').expect("integer width and length") {
                ("int64", variant) => (4_u8, 8, variant),
                ("int32", variant) => (5, 4, variant),
                ("int16", variant) => (6, 2, variant),
                ("int8", variant) => (7, 1, variant),
                _ => unreachable!(),
            };
        let size = match variant {
            "short" => width - 1,
            "exact" => width,
            "oversized" => width + 1,
            _ => unreachable!(),
        };
        // Retain the valid "ping" name/header and replace only its scalar
        // type and payload length. The outer alignment bytes remain padding.
        let mut scalar = valid;
        scalar[0] = 0x80 | kind;
        scalar.resize(scalar.len() + size, 0x5a);
        let raw_len = scalar.len() as u32;
        scalar[1..4].copy_from_slice(&raw_len.to_be_bytes()[1..4]);
        scalar.resize(ubus::blob::align4(raw_len as usize), 0);
        let expected = if variant == "exact" {
            scalar.clone()
        } else {
            Vec::new()
        };
        return (scalar, expected);
    }
    let mut malformed = valid.clone();
    match shape {
        "valid" => return (valid.clone(), valid),
        "empty" => return (Vec::new(), Vec::new()),
        "empty-name" => {
            let mut b = BlobBuilder::new();
            b.open_table(None);
            b.close_table();
            malformed = b.finish();
        }
        "missing-terminator" => malformed[10] = 1,
        "nonextended" => malformed[0] &= 0x7f,
        "invalid-type" => malformed[0] = 0x89,
        "short-int32" => malformed[0] = 0x85,
        "unterminated-string" => malformed[0] = 0x83,
        "mixed-valid-invalid" => {
            malformed[10] = 1;
            malformed.splice(..0, valid.iter().copied());
        }
        "truncated-tail" => {
            malformed.extend_from_slice(&0x8200_0100_u32.to_be_bytes());
            return (malformed, valid);
        }
        _ => unreachable!(),
    }
    (malformed, Vec::new())
}

fn signature_registration_body(path: &str, shape: &str, typed: bool) {
    let (signature, expected_signature) = signature_shape(shape);
    let mut owner = connect_and_wait_hello(path);
    let mut env = EnvBuilder::new();
    env.push_str(UbusAttr::ObjPath, "signature.object");
    env.push_signature(&signature);
    owner
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::AddObject,
                seq: 1,
                peer: 0,
            },
            payload: env.finish(),
        })
        .expect("ADD_OBJECT signature shape");
    let added = recv_until(&mut owner, Duration::from_secs(5), |f| {
        f.hdr.ty == MsgType::Data && f.hdr.seq == 1
    })
    .expect("ADD_OBJECT DATA");
    let object = find(&added.payload, UbusAttr::ObjId)
        .and_then(|a| a.as_u32())
        .expect("object id");
    assert_eq!(
        find(&added.payload, UbusAttr::ObjType).is_some(),
        typed,
        "{shape} object type"
    );
    expect_status_only(&mut owner, 1, UBUS_STATUS_OK, "signature registration");

    let mut caller = connect_and_wait_hello(path);
    let mut lookup = EnvBuilder::new();
    lookup.push_str(UbusAttr::ObjPath, "signature.object");
    caller
        .send(&Frame {
            hdr: Header {
                version: PROTO_VERSION,
                ty: MsgType::Lookup,
                seq: 2,
                peer: 0,
            },
            payload: lookup.finish(),
        })
        .expect("LOOKUP signature shape");
    if typed {
        let data = recv_until(&mut caller, Duration::from_secs(5), |f| {
            f.hdr.ty == MsgType::Data && f.hdr.seq == 2
        })
        .expect("typed LOOKUP DATA");
        assert_eq!(
            find(&data.payload, UbusAttr::ObjId).and_then(|a| a.as_u32()),
            Some(object)
        );
        assert_eq!(
            find(&data.payload, UbusAttr::Signature)
                .expect("lookup signature")
                .payload,
            expected_signature
        );
    }
    expect_status_only(
        &mut caller,
        2,
        UBUS_STATUS_OK,
        "signature LOOKUP visibility",
    );
}

macro_rules! signature_registration_case {
    ($name:ident, $shape:literal, $typed:literal) => {
        acl_case!($name, arm, {
            let acl = AclDir::with_grant(r#"{"user":"testuser","publish":["signature.*"],"access":{"signature.*":{"methods":["*"]}}}"#);
            run_nonroot(concat!("ubusd_acl ", stringify!($name)), arm, Some(&acl), &|path| {
                signature_registration_body(path, $shape, $typed)
            });
        });
    };
}

signature_registration_case!(
    signature_int64_short_discards_type,
    "integer-int64-short",
    false
);
signature_registration_case!(
    signature_int64_exact_retains_type,
    "integer-int64-exact",
    true
);
signature_registration_case!(
    signature_int64_oversized_discards_type,
    "integer-int64-oversized",
    false
);
signature_registration_case!(
    signature_int32_short_discards_type,
    "integer-int32-short",
    false
);
signature_registration_case!(
    signature_int32_exact_retains_type,
    "integer-int32-exact",
    true
);
signature_registration_case!(
    signature_int32_oversized_discards_type,
    "integer-int32-oversized",
    false
);
signature_registration_case!(
    signature_int16_short_discards_type,
    "integer-int16-short",
    false
);
signature_registration_case!(
    signature_int16_exact_retains_type,
    "integer-int16-exact",
    true
);
signature_registration_case!(
    signature_int16_oversized_discards_type,
    "integer-int16-oversized",
    false
);
signature_registration_case!(
    signature_int8_short_discards_type,
    "integer-int8-short",
    false
);
signature_registration_case!(
    signature_int8_exact_retains_type,
    "integer-int8-exact",
    true
);
signature_registration_case!(
    signature_int8_oversized_discards_type,
    "integer-int8-oversized",
    false
);

signature_registration_case!(signature_valid_method_retains_type, "valid", true);
signature_registration_case!(signature_empty_method_list_retains_type, "empty", true);
signature_registration_case!(
    signature_empty_method_name_discards_type,
    "empty-name",
    false
);
signature_registration_case!(
    signature_missing_name_terminator_discards_type,
    "missing-terminator",
    false
);
signature_registration_case!(
    signature_nonextended_method_discards_type,
    "nonextended",
    false
);
signature_registration_case!(
    signature_unknown_method_type_discards_type,
    "invalid-type",
    false
);
signature_registration_case!(
    signature_short_scalar_method_discards_type,
    "short-int32",
    false
);
signature_registration_case!(
    signature_unterminated_string_method_discards_type,
    "unterminated-string",
    false
);
signature_registration_case!(
    signature_mixed_valid_invalid_methods_discard_whole_type,
    "mixed-valid-invalid",
    false
);
signature_registration_case!(
    signature_truncated_tail_keeps_complete_methods,
    "truncated-tail",
    true
);

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

// The pinned C broker reads only the first 511 bytes of Groups:. Besides
// losing later groups, this can turn a partial numeric token into a different
// valid GID. Parse the complete record in Rust while retaining the 64-group
// cap; keep this security regression outside the pinned parity contract.
#[rstest]
#[case::group_crossing_old_buffer_boundary(1_234_567_890_u32, UBUS_STATUS_OK)]
#[case::group_after_old_buffer_boundary(2_000_000_000_u32, UBUS_STATUS_OK)]
#[case::truncated_numeric_prefix_is_not_a_group(12_345_678_u32, UBUS_STATUS_INVALID_ARGUMENT)]
fn long_supplementary_group_record_keeps_exact_group_ids(
    #[case] grant_gid: u32,
    #[case] expected: i32,
) {
    let acl = AclDir::with_grant(r#"{"group":"secondary","publish":["long-groups.*"]}"#);
    let prefix = (1_000_000_000_u32..1_000_000_045)
        .map(|gid| format!("{gid} "))
        .collect::<String>();
    let extra_gids = format!("{prefix}1234567890 2000000000");
    // A real proc Groups record uses this prefix and sorted decimal IDs.
    // All 47 memberships fit below the retained 64-group compatibility cap.
    let record = format!("Groups:\t{extra_gids}\n");
    assert_eq!(record[..511].split_whitespace().last(), Some("12345678"));
    let mut options = nonroot_opts(Some(&acl));
    options.env.extend([
        (
            "UBUS_TESTKIT_NSS_GROUPS".to_owned(),
            format!("1000:testgroup,{grant_gid}:secondary,0:root"),
        ),
        ("UBUS_TESTKIT_NSS_EXTRA_GIDS".to_owned(), extra_gids),
    ]);
    run_matrix_case_with(
        "ubusd_acl long_supplementary_group_record_keeps_exact_group_ids",
        MatrixArm::Rs,
        &options,
        &|path| named_add_body(path, "long-groups.object", expected),
    );
}

acl_case!(
    lookup_unions_matching_grants_and_preserves_method_bytes,
    arm,
    {
        let acl = AclDir::with_files(&[
            (
                "publish.json",
                r#"{"user":"testuser","publish":["union.*"]}"#,
            ),
            (
                "user.json",
                r#"{"user":"alias","access":{"union.*":{"methods":["alpha"]}}}"#,
            ),
            (
                "primary-group.json",
                r#"{"group":"testgroup","access":{"union.object":{"methods":["beta"]}}}"#,
            ),
            (
                "supplementary-group.json",
                r#"{"group":"secondary","access":{"union.*":{"methods":["gamma"]}}}"#,
            ),
            (
                "other-user.json",
                r#"{"user":"other","access":{"union.*":{"methods":["denied"]}}}"#,
            ),
            (
                "other-path.json",
                r#"{"user":"testuser","access":{"other.*":{"methods":["denied"]}}}"#,
            ),
        ]);
        let mut options = nonroot_opts(Some(&acl));
        options.env.extend([
            (
                "UBUS_TESTKIT_NSS_USERS".to_owned(),
                "1000:testuser,1000:alias,2000:other,0:root".to_owned(),
            ),
            (
                "UBUS_TESTKIT_NSS_GROUPS".to_owned(),
                "1000:testgroup,2000:secondary,0:root".to_owned(),
            ),
            ("UBUS_TESTKIT_NSS_EXTRA_GIDS".to_owned(), "2000".to_owned()),
        ]);
        run_matrix_case_with(
            "ubusd_acl lookup_unions_matching_grants_and_preserves_method_bytes",
            arm,
            &options,
            &|path| {
                let mut signature = Vec::new();
                let mut expected = Vec::new();
                for (index, method) in ["gamma", "denied", "alpha", "beta"].iter().enumerate() {
                    let mut builder = BlobBuilder::new();
                    builder.open_table(Some(method));
                    builder.put_i32(Some("argument"), index as i32);
                    builder.close_table();
                    let bytes = builder.finish();
                    signature.extend_from_slice(&bytes);
                    if *method != "denied" {
                        expected.extend_from_slice(&bytes);
                    }
                }
                let mut owner = connect_and_wait_hello(path);
                let (object, _) = common::raw_add_object_with_signature(
                    &mut owner,
                    1,
                    "union.object",
                    &signature,
                );
                let mut caller = connect_and_wait_hello(path);
                let mut request = EnvBuilder::new();
                request.push_str(UbusAttr::ObjPath, "union.object");
                caller
                    .send(&Frame::to_broker(MsgType::Lookup, 2, request.finish()))
                    .expect("union LOOKUP");
                let data = recv_until(&mut caller, Duration::from_secs(5), |frame| {
                    frame.hdr.seq == 2 && frame.hdr.ty == MsgType::Data
                })
                .expect("union LOOKUP DATA");
                assert_eq!(
                    find(&data.payload, UbusAttr::ObjId).and_then(|attr| attr.as_u32()),
                    Some(object)
                );
                assert_eq!(
                    find(&data.payload, UbusAttr::Signature)
                        .expect("visible signature")
                        .payload,
                    expected
                );
                expect_status_only(&mut caller, 2, UBUS_STATUS_OK, "union LOOKUP");
            },
        );
    }
);
