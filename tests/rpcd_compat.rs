//! Real rpcd consumers across both brokers. Every daemon gets private ACL,
//! config, delta, plugin and runtime directories through the child-only path
//! redirect shim, so these tests can run together without host UCI access.

#![cfg(all(feature = "testing", target_os = "linux"))]

use std::fs;
use std::process::Output;

use rstest::rstest;
use serde_json::{json, Value};
use ubus_testkit::{
    skip, RpcdHarness, UbusCli, UbusdHarness, UciCli, UciEntry, RPCD_LOGIN_READONLY,
    RPCD_LOGIN_READWRITE, RPCD_TEST_PASSWORD,
};

const CONFIG: &str =
    "config example 'main'\n\toption value 'original'\n\tlist tags 'one'\n\tlist tags 'two'\n";

struct Fixture {
    // Stop rpcd before tearing down its broker.
    rpcd: RpcdHarness,
    _broker: UbusdHarness,
    cli: UbusCli,
}

impl Fixture {
    fn start(rust: bool) -> Option<Self> {
        let broker = if rust {
            UbusdHarness::start_rust()
        } else {
            UbusdHarness::start_upstream()
        }
        .expect("start broker");
        let Some(broker) = broker else {
            skip::or_skip("rpcd compatibility", "broker unavailable");
            return None;
        };
        let Some(cli) = UbusCli::locate() else {
            skip::or_skip("rpcd compatibility", "upstream ubus CLI unavailable");
            return None;
        };
        let Some(rpcd) = RpcdHarness::start(broker.socket_path(), &[]).expect("start rpcd") else {
            skip::or_skip("rpcd compatibility", "rpcd unavailable");
            return None;
        };
        let files = format!("{}/*", rpcd.files_dir().display());
        let acl = json!({"compat": {
            "description": "Isolated compatibility fixture",
            "read": {"ubus": {"uci": ["get", "changes"], "file": ["read", "stat", "list", "md5"]}, "uci": ["compat"], "file": {files.clone(): ["read", "list"]}},
            "write": {"ubus": {"uci": ["set", "add", "delete", "rename", "commit", "revert"], "file": ["write", "remove"]}, "uci": ["compat"], "file": {files: ["write"]}}
        }});
        fs::write(rpcd.acl_dir().join("compat.json"), acl.to_string()).unwrap();
        fs::write(rpcd.config_dir().join("compat"), CONFIG).unwrap();
        Some(Self {
            rpcd,
            _broker: broker,
            cli,
        })
    }

    fn output(&self, object: &str, method: &str, args: Value) -> Output {
        self.cli
            .run(
                self.rpcd.socket(),
                &["-t", "3", "call", object, method, &args.to_string()],
            )
            .expect("run upstream CLI")
    }

    fn call(&self, object: &str, method: &str, args: Value) -> Value {
        let output = self.output(object, method, args);
        assert!(
            output.status.success(),
            "{object}.{method}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if output.stdout.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&output.stdout).expect("rpcd JSON reply")
        }
    }

    fn status(&self, object: &str, method: &str, args: Value, status: i32) {
        let output = self.output(object, method, args);
        assert_eq!(
            output.status.code(),
            Some((256 - status) % 256),
            "{object}.{method}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty(), "error returned a success payload");
    }

    fn login(&self, user: &str) -> String {
        let reply = self.call(
            "session",
            "login",
            json!({"username":user, "password":RPCD_TEST_PASSWORD}),
        );
        assert_eq!(reply["data"]["username"], user);
        let sid = reply["ubus_rpc_session"].as_str().expect("session id");
        assert_eq!(sid.len(), 32);
        assert!(sid.bytes().all(|byte| byte.is_ascii_hexdigit()));
        sid.to_owned()
    }

    fn get(&self, sid: &str) -> Value {
        self.call(
            "uci",
            "get",
            json!({"config":"compat", "section":"main", "ubus_rpc_session":sid}),
        )["values"]
            .clone()
    }

    fn file_plugin(&self) -> bool {
        if !self.rpcd.has_file_plugin() {
            skip::or_skip(
                "rpcd file compatibility",
                "upstream rpcd file.so unavailable",
            );
            return false;
        }
        let out = self.cli.run(self.rpcd.socket(), &["list", "file"]).unwrap();
        assert!(
            out.status.success() && out.stdout == b"file\n",
            "file plugin failed to register: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        true
    }
}

#[rstest]
#[case::upstream(false)]
#[case::rs(true)]
fn login_permissions_and_destroy(#[case] rust: bool) {
    let Some(f) = Fixture::start(rust) else {
        return;
    };
    f.status(
        "session",
        "login",
        json!({"username":"rw", "password":"wrong"}),
        6,
    );
    f.status(
        "session",
        "login",
        json!({"username":"unknown", "password":RPCD_TEST_PASSWORD}),
        6,
    );
    let ro = f.login(RPCD_LOGIN_READONLY);
    let rw = f.login(RPCD_LOGIN_READWRITE);
    assert_ne!(ro, rw);
    for (sid, write) in [(&ro, false), (&rw, true)] {
        for (method, allowed) in [
            ("get", true),
            ("set", write),
            ("delete", write),
            ("missing", false),
        ] {
            assert_eq!(f.call("session", "access", json!({"ubus_rpc_session":sid, "scope":"ubus", "object":"uci", "function":method})), json!({"access":allowed}));
        }
    }
    f.call("session", "destroy", json!({"ubus_rpc_session":rw}));
    f.status(
        "session",
        "access",
        json!({"ubus_rpc_session":rw, "scope":"uci", "object":"compat", "function":"write"}),
        4,
    );
    f.status("session", "get", json!({"ubus_rpc_session":rw}), 4);
    // Destroying one session must not disturb a concurrent reader.
    assert_eq!(f.get(&ro)["value"], "original");
}

#[rstest]
#[case::upstream(false)]
#[case::rs(true)]
fn session_values_are_typed_and_isolated(#[case] rust: bool) {
    let Some(f) = Fixture::start(rust) else {
        return;
    };
    let first = f.login(RPCD_LOGIN_READWRITE);
    let second = f.login(RPCD_LOGIN_READWRITE);
    let values = json!({"text":"hello", "nested":{"flags":[true, false], "number":42}, "empty":[]});
    f.call(
        "session",
        "set",
        json!({"ubus_rpc_session":first, "values":values}),
    );
    let stored = f.call(
        "session",
        "get",
        json!({"ubus_rpc_session":first, "keys":["text", "nested", "empty", "absent"]}),
    );
    assert_eq!(stored, json!({"values":values}));
    assert_eq!(
        f.call(
            "session",
            "get",
            json!({"ubus_rpc_session":second, "keys":["text", "nested"]})
        ),
        json!({"values":{}})
    );
    f.call(
        "session",
        "unset",
        json!({"ubus_rpc_session":first, "keys":["text"]}),
    );
    assert_eq!(
        f.call(
            "session",
            "get",
            json!({"ubus_rpc_session":first, "keys":["text", "nested"]})
        ),
        json!({"values":{"nested":values["nested"]}})
    );
}

#[rstest]
#[case::upstream(false)]
#[case::rs(true)]
fn dynamic_session_grants_revoke_without_cross_session_leaks(#[case] rust: bool) {
    let Some(f) = Fixture::start(rust) else {
        return;
    };
    let first = f.call("session", "create", json!({}))["ubus_rpc_session"]
        .as_str()
        .unwrap()
        .to_owned();
    let second = f.call("session", "create", json!({}))["ubus_rpc_session"]
        .as_str()
        .unwrap()
        .to_owned();
    let access = |sid: &str, object: &str, function: &str| {
        f.call(
            "session",
            "access",
            json!({"ubus_rpc_session":sid, "scope":"custom", "object":object, "function":function}),
        )
    };
    assert_eq!(
        access(&first, "example.one", "read"),
        json!({"access":false})
    );
    f.call("session", "grant", json!({"ubus_rpc_session":first, "scope":"custom", "objects":[["example.*", "read"],["example.one", "write"]]}));
    assert_eq!(
        access(&first, "example.one", "read"),
        json!({"access":true})
    );
    assert_eq!(
        access(&first, "example.two", "read"),
        json!({"access":true})
    );
    assert_eq!(
        access(&first, "example.one", "write"),
        json!({"access":true})
    );
    assert_eq!(
        access(&first, "example.two", "write"),
        json!({"access":false})
    );
    assert_eq!(
        access(&second, "example.one", "read"),
        json!({"access":false})
    );
    f.call(
        "session",
        "revoke",
        json!({"ubus_rpc_session":first, "scope":"custom", "objects":[["example.one", "write"]]}),
    );
    assert_eq!(
        access(&first, "example.one", "write"),
        json!({"access":false})
    );
    assert_eq!(
        access(&first, "example.one", "read"),
        json!({"access":true})
    );
    f.call(
        "session",
        "revoke",
        json!({"ubus_rpc_session":first, "scope":"custom"}),
    );
    assert_eq!(
        access(&first, "example.two", "read"),
        json!({"access":false})
    );
}

#[rstest]
#[case::upstream(false)]
#[case::rs(true)]
fn uci_readonly_denials_leave_configuration_unchanged(#[case] rust: bool) {
    let Some(f) = Fixture::start(rust) else {
        return;
    };
    let ro = f.login(RPCD_LOGIN_READONLY);
    assert_eq!(f.get(&ro)["tags"], json!(["one", "two"]));
    for method in ["set", "delete", "commit", "revert"] {
        f.status("uci", method, json!({"ubus_rpc_session":ro, "config":"compat", "section":"main", "option":"value", "values":{"value":"denied"}}), 6);
    }
    f.status(
        "uci",
        "get",
        json!({"ubus_rpc_session":ro, "config":"rpcd"}),
        6,
    );
    assert_eq!(f.get(&ro)["value"], "original");
    assert_eq!(
        fs::read_to_string(f.rpcd.config_dir().join("compat")).unwrap(),
        CONFIG
    );
}

#[rstest]
#[case::upstream(false)]
#[case::rs(true)]
fn uci_pending_changes_are_session_private_and_revertible(#[case] rust: bool) {
    let Some(f) = Fixture::start(rust) else {
        return;
    };
    let first = f.login(RPCD_LOGIN_READWRITE);
    let second = f.login(RPCD_LOGIN_READWRITE);
    f.call("uci", "set", json!({"ubus_rpc_session":first, "config":"compat", "section":"main", "values":{"value":"pending", "tags":["three", "four"]}}));
    assert_eq!(f.get(&first)["value"], "pending");
    assert_eq!(f.get(&first)["tags"], json!(["three", "four"]));
    assert_eq!(f.get(&second)["value"], "original");
    let changes = f.call(
        "uci",
        "changes",
        json!({"ubus_rpc_session":first, "config":"compat"}),
    );
    assert!(
        changes["changes"].as_array().is_some_and(|changes| changes
            .iter()
            .any(|change| change == &json!(["set", "main", "value", "pending"]))),
        "{changes}"
    );
    assert_eq!(
        fs::read_to_string(f.rpcd.config_dir().join("compat")).unwrap(),
        CONFIG
    );
    f.call(
        "uci",
        "revert",
        json!({"ubus_rpc_session":first, "config":"compat"}),
    );
    assert_eq!(f.get(&first)["value"], "original");
    assert_eq!(
        f.call(
            "uci",
            "changes",
            json!({"ubus_rpc_session":first, "config":"compat"})
        ),
        json!({"changes":[]})
    );
}

#[rstest]
#[case::upstream(false)]
#[case::rs(true)]
fn uci_commit_matches_independent_uci_cli_export(#[case] rust: bool) {
    let Some(f) = Fixture::start(rust) else {
        return;
    };
    let Some(uci) = UciCli::locate() else {
        skip::or_skip(
            "rpcd committed UCI compatibility",
            "upstream uci CLI unavailable",
        );
        return;
    };
    let rw = f.login(RPCD_LOGIN_READWRITE);
    let ro = f.login(RPCD_LOGIN_READONLY);
    f.call("uci", "set", json!({"ubus_rpc_session":rw, "config":"compat", "section":"main", "values":{"value":"committed", "tags":["alpha", "beta"], "enabled":true, "count":42}}));
    f.call(
        "uci",
        "commit",
        json!({"ubus_rpc_session":rw, "config":"compat"}),
    );
    let sections = uci.export_sections(&f.rpcd.config_dir(), "compat").unwrap();
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].kind, "example");
    assert_eq!(sections[0].name.as_deref(), Some("main"));
    for expected in [
        ("value".to_owned(), UciEntry::Option("committed".to_owned())),
        (
            "tags".to_owned(),
            UciEntry::List(vec!["alpha".to_owned(), "beta".to_owned()]),
        ),
        ("enabled".to_owned(), UciEntry::Option("1".to_owned())),
        ("count".to_owned(), UciEntry::Option("42".to_owned())),
    ] {
        assert!(
            sections[0].entries.contains(&expected),
            "missing {expected:?}: {sections:?}"
        );
    }
    assert_eq!(f.get(&ro)["value"], "committed");
    assert_eq!(
        f.call(
            "uci",
            "changes",
            json!({"ubus_rpc_session":rw, "config":"compat"})
        ),
        json!({"changes":[]})
    );
}

#[rstest]
#[case::upstream(false)]
#[case::rs(true)]
fn uci_section_add_rename_delete_and_type_filter(#[case] rust: bool) {
    let Some(f) = Fixture::start(rust) else {
        return;
    };
    let rw = f.login(RPCD_LOGIN_READWRITE);
    let added = f.call("uci", "add", json!({"ubus_rpc_session":rw, "config":"compat", "type":"extra", "name":"added", "values":{"value":"new", "remove_me":"temporary"}}));
    assert_eq!(added, json!({"section":"added"}));
    f.call("uci", "rename", json!({"ubus_rpc_session":rw, "config":"compat", "section":"added", "option":"value", "name":"renamed_value"}));
    f.call(
        "uci",
        "delete",
        json!({"ubus_rpc_session":rw, "config":"compat", "section":"added", "option":"remove_me"}),
    );
    f.call("uci", "rename", json!({"ubus_rpc_session":rw, "config":"compat", "section":"added", "name":"renamed_section"}));
    let values = f.call(
        "uci",
        "get",
        json!({"ubus_rpc_session":rw, "config":"compat", "type":"extra"}),
    );
    assert_eq!(values["values"].as_object().unwrap().len(), 1);
    assert_eq!(values["values"]["renamed_section"]["renamed_value"], "new");
    assert!(values["values"]["renamed_section"]
        .get("remove_me")
        .is_none());
    assert!(values["values"].get("main").is_none());
    f.call(
        "uci",
        "delete",
        json!({"ubus_rpc_session":rw, "config":"compat", "section":"renamed_section"}),
    );
    let package = f.call(
        "uci",
        "get",
        json!({"ubus_rpc_session":rw, "config":"compat"}),
    );
    assert!(package["values"].get("renamed_section").is_none());
    assert_eq!(f.get(&rw)["value"], "original");
}

#[rstest]
#[case::upstream(false)]
#[case::rs(true)]
fn concurrent_rpcd_sessions_keep_data_private(#[case] rust: bool) {
    let Some(f) = Fixture::start(rust) else {
        return;
    };
    std::thread::scope(|scope| {
        for worker in 0..8 {
            let f = &f;
            scope.spawn(move || {
                let sid = f.login(RPCD_LOGIN_READWRITE);
                for request in 0..3 {
                    let values =
                        json!({"worker":worker, "request":request, "text":"ß".repeat(4096)});
                    f.call(
                        "session",
                        "set",
                        json!({"ubus_rpc_session":sid, "values":values}),
                    );
                    assert_eq!(
                        f.call(
                            "session",
                            "get",
                            json!({"ubus_rpc_session":sid, "keys":["worker", "request", "text"]})
                        ),
                        json!({"values":values})
                    );
                }
                f.call("session", "destroy", json!({"ubus_rpc_session":sid}));
            });
        }
    });
}

#[rstest]
#[case::upstream(false)]
#[case::rs(true)]
fn file_plugin_enforces_session_read_write_permissions(#[case] rust: bool) {
    let Some(f) = Fixture::start(rust) else {
        return;
    };
    if !f.file_plugin() {
        return;
    }
    let file = f.rpcd.files_dir().join("text");
    fs::write(&file, "original").unwrap();
    let ro = f.login(RPCD_LOGIN_READONLY);
    let rw = f.login(RPCD_LOGIN_READWRITE);
    assert_eq!(
        f.call("file", "read", json!({"ubus_rpc_session":ro, "path":file})),
        json!({"data":"original"})
    );
    f.status(
        "file",
        "write",
        json!({"ubus_rpc_session":ro, "path":file, "data":"denied"}),
        6,
    );
    f.status(
        "file",
        "remove",
        json!({"ubus_rpc_session":ro, "path":file}),
        6,
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "original");
    f.call(
        "file",
        "write",
        json!({"ubus_rpc_session":rw, "path":file, "data":"abc"}),
    );
    f.call(
        "file",
        "write",
        json!({"ubus_rpc_session":rw, "path":file, "data":"def", "append":true}),
    );
    assert_eq!(
        f.call("file", "read", json!({"ubus_rpc_session":ro, "path":file})),
        json!({"data":"abcdef"})
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "abcdef");
    f.call(
        "file",
        "remove",
        json!({"ubus_rpc_session":rw, "path":file}),
    );
    assert!(!file.exists());
    f.status(
        "file",
        "read",
        json!({"ubus_rpc_session":rw, "path":file}),
        4,
    );
}

#[rstest]
#[case::upstream(false)]
#[case::rs(true)]
fn file_plugin_binary_roundtrip_metadata_and_acl_boundary(#[case] rust: bool) {
    let Some(f) = Fixture::start(rust) else {
        return;
    };
    if !f.file_plugin() {
        return;
    }
    let file = f.rpcd.files_dir().join("binary");
    let rw = f.login(RPCD_LOGIN_READWRITE);
    f.call(
        "file",
        "write",
        json!({"ubus_rpc_session":rw, "path":file, "data":"AAECA//+", "base64":true, "mode":384}),
    );
    assert_eq!(fs::read(&file).unwrap(), [0, 1, 2, 3, 255, 254]);
    assert_eq!(
        f.call(
            "file",
            "read",
            json!({"ubus_rpc_session":rw, "path":file, "base64":true})
        ),
        json!({"data":"AAECA//+"})
    );
    let stat = f.call("file", "stat", json!({"ubus_rpc_session":rw, "path":file}));
    assert_eq!(stat["size"], 6);
    assert_eq!(stat["type"], "file");
    assert_eq!(stat["mode"].as_u64().unwrap() & 0o777, 0o600);
    // This file exists in our fixture, but lies outside its file ACL. The
    // canonicalized ../ path must be denied without exposing the login hashes.
    let denied = f.rpcd.files_dir().join("../config/rpcd");
    f.status(
        "file",
        "read",
        json!({"ubus_rpc_session":rw, "path":denied}),
        6,
    );
}

#[rstest]
#[case::upstream(false)]
#[case::rs(true)]
fn file_plugin_deferred_exec_returns_stdout_stderr_and_exit_status(#[case] rust: bool) {
    let Some(f) = Fixture::start(rust) else {
        return;
    };
    if !f.file_plugin() {
        return;
    }
    let rw = f.login(RPCD_LOGIN_READWRITE);
    // RpcdHarness's child-only FIFO EOF ordering shim keeps the native
    // SIGCHLD callback ahead of the pinned plugin's EOF-triggered reply.
    // Assert the real nonzero child status; never synthesize or accept zero.
    let command = "printf 'out'; printf 'err' >&2; exit 7";
    let args = json!({"ubus_rpc_session":rw, "command":"/bin/sh", "params":["-c", command]});
    f.status("file", "exec", args.clone(), 6);
    f.call(
        "session",
        "grant",
        json!({"ubus_rpc_session":rw, "scope":"file", "objects":[["/bin/sh", "exec"]]}),
    );
    assert_eq!(
        f.call("file", "exec", args.clone()),
        json!({"code":7, "stdout":"out", "stderr":"err"})
    );
    let mut forbidden_env = args.clone();
    forbidden_env["env"] = json!({"UBUS_TEST_ONLY":"1"});
    f.status("file", "exec", forbidden_env, 6);
    f.call(
        "session",
        "revoke",
        json!({"ubus_rpc_session":rw, "scope":"file", "objects":[["/bin/sh", "exec"]]}),
    );
    f.status("file", "exec", args, 6);
}

#[rstest]
#[case::upstream(false)]
#[case::rs(true)]
fn concurrent_file_exec_replies_keep_distinct_status_and_output(#[case] rust: bool) {
    let Some(f) = Fixture::start(rust) else {
        return;
    };
    if !f.file_plugin() {
        return;
    }
    let rw = f.login(RPCD_LOGIN_READWRITE);
    f.call(
        "session",
        "grant",
        json!({"ubus_rpc_session":rw, "scope":"file", "objects":[["/bin/sh", "exec"]]}),
    );
    std::thread::scope(|scope| {
        for worker in 0..8 {
            let f = &f;
            let rw = &rw;
            scope.spawn(move || {
                for request in 0..2 {
                    let stdout = format!("out-{worker}-{request}");
                    let stderr = format!("err-{worker}-{request}");
                    let status = worker + 1;
                    let command = format!("printf '{stdout}'; printf '{stderr}' >&2; exit {status}");
                    let args = json!({"ubus_rpc_session":rw, "command":"/bin/sh", "params":["-c", command]});
                    assert_eq!(f.call("file", "exec", args), json!({"code":status, "stdout":stdout, "stderr":stderr}));
                }
            });
        }
    });
}
