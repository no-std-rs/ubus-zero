//! Shared fixtures for the cross-broker integration tests. Each file
//! under `tests/` is its own crate, so helpers driven from more than
//! one test binary live here rather than being copied per file.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ubus::envelope::{find, iter_attrs, unwrap as unwrap_envelope, EnvBuilder, UbusAttr};
use ubus::server::{UbusConnection, UbusObject};
use ubus::transport::Transport;
use ubus::wire::{Frame, Header, MsgType, PROTO_VERSION, UBUS_STATUS_OK};
use ubus_testkit::unique_socket_path;
use ubus_testkit::UnixUbusTransport;

/// Synthetic identity owned by the nss_spoof shim — never resolved on the
/// host, so ACL transcripts are machine-independent.
pub const TEST_UID: u32 = 1000;
pub const TEST_USER: &str = "testuser";
pub const TEST_GROUP: &str = "testgroup";

/// Temporary directory containing ubusd-format ACL JSON files.
pub struct AclDir {
    dir: PathBuf,
}

impl AclDir {
    pub fn with_files(files: &[(&str, &str)]) -> Self {
        let dir = unique_socket_path("acl").with_extension("d");
        std::fs::create_dir_all(&dir).expect("create acl dir");
        for (name, contents) in files {
            std::fs::write(dir.join(name), contents).expect("write acl file");
        }
        Self { dir }
    }

    pub fn with_grant(grant: &str) -> Self {
        Self::with_files(&[("grant.json", grant)])
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for AclDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A thread is needed because the callee must service `poll_one`
/// concurrently with the test driver — the broker routes INVOKE/
/// SUBSCRIBE traffic through the registered object's connection, and
/// the broker won't make progress on the caller's side until the
/// owner pumps a reply.
pub struct OwnerThread {
    handle: Option<std::thread::JoinHandle<Result<(), String>>>,
    ready_rx: std::sync::mpsc::Receiver<()>,
    done_tx: std::sync::mpsc::Sender<()>,
}

impl OwnerThread {
    /// `build_obj` runs on the owner thread because `UbusObject`'s
    /// boxed method closures aren't `Send`.
    pub fn spawn(path: String, build_obj: impl FnOnce() -> UbusObject + Send + 'static) -> Self {
        Self::spawn_with_deadline(path, Duration::from_secs(5), build_obj)
    }

    /// Like [`Self::spawn`], but gives a shared owner enough time to service
    /// a deliberately long-lived or concurrent test workload.
    pub fn spawn_with_deadline(
        path: String,
        deadline: Duration,
        build_obj: impl FnOnce() -> UbusObject + Send + 'static,
    ) -> Self {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || -> Result<(), String> {
            let transport =
                UnixUbusTransport::connect(&path).map_err(|e| format!("owner connect: {e:?}"))?;
            let mut conn = UbusConnection::connect_and_register(transport, build_obj())
                .map_err(|e| format!("register: {e:?}"))?;
            ready_tx.send(()).map_err(|_| "ready tx".to_string())?;
            let deadline_at = Instant::now() + deadline;
            while Instant::now() < deadline_at {
                if let Err(e) = conn.poll_one() {
                    return Err(format!("owner poll_one: {e:?}"));
                }
                match done_rx.recv_timeout(Duration::from_millis(5)) {
                    Ok(()) => return Ok(()),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                }
            }
            // Deadline elapsed without a done signal — surface as a hard
            // error so a hung/slow caller doesn't masquerade as clean
            // shutdown and silently green-light a broken test.
            Err("owner thread deadline elapsed without done signal".to_string())
        });
        Self {
            handle: Some(handle),
            ready_rx,
            done_tx,
        }
    }

    pub fn wait_ready(&self) {
        self.ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("owner registered in time");
    }

    pub fn shutdown(mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = self.done_tx.send(());
            join_owner(handle);
        }
    }
}

impl Drop for OwnerThread {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = self.done_tx.send(());
            // Surface a panicking owner so an unwinding test driver
            // doesn't leave the operator hunting a vanished thread.
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("owner thread error during drop: {e}"),
                Err(p) => eprintln!("owner thread panicked during drop: {p:?}"),
            }
        }
    }
}

pub fn join_owner(handle: std::thread::JoinHandle<Result<(), String>>) {
    match handle.join() {
        Err(_) => panic!("owner thread panicked"),
        Ok(Err(e)) => panic!("owner thread error: {e}"),
        Ok(Ok(())) => {}
    }
}

/// Send a typed ADD_OBJECT and return the broker-assigned object and type ids.
pub fn raw_add_object_with_signature(
    transport: &mut UnixUbusTransport,
    seq: u16,
    path: &str,
    signature: &[u8],
) -> (u32, u32) {
    let mut env = EnvBuilder::new();
    env.push_str(UbusAttr::ObjPath, path);
    env.push_signature(signature);
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
        .expect("send typed ADD_OBJECT");
    expect_typed_data_then_status_ok(transport, seq, "typed ADD_OBJECT")
}

/// Build the minimal method signature used by raw publisher fixtures.
pub fn signature_with_ping_method() -> Vec<u8> {
    let mut sig = ubus::blobmsg::BlobBuilder::new();
    sig.open_table(Some("ping"));
    sig.close_table();
    sig.finish()
}

pub fn expect_typed_data_then_status_ok(
    transport: &mut UnixUbusTransport,
    seq: u16,
    what: &str,
) -> (u32, u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut data: Option<(u32, u32)> = None;
    while Instant::now() < deadline {
        match transport.recv() {
            Ok(Some(f)) if f.hdr.seq == seq && f.hdr.ty == MsgType::Data => {
                let inner = unwrap_envelope(&f.payload).expect("valid envelope");
                let attr_ids: Vec<u8> = iter_attrs(inner).map(|a| a.id).collect();
                assert_eq!(
                    attr_ids,
                    vec![UbusAttr::ObjId as u8, UbusAttr::ObjType as u8],
                    "{what} DATA attr order"
                );
                let obj_id = find(&f.payload, UbusAttr::ObjId)
                    .and_then(|a| a.as_u32())
                    .unwrap_or_else(|| panic!("{what} DATA must carry OBJID"));
                let obj_type = find(&f.payload, UbusAttr::ObjType)
                    .and_then(|a| a.as_u32())
                    .unwrap_or_else(|| panic!("{what} DATA must carry OBJTYPE"));
                assert_ne!(obj_id, 0, "{what} DATA OBJID must be non-zero");
                assert_ne!(obj_type, 0, "{what} DATA OBJTYPE must be non-zero");
                assert_ne!(obj_id, obj_type, "{what} OBJTYPE must differ from OBJID");
                data = Some((obj_id, obj_type));
            }
            Ok(Some(f)) if f.hdr.seq == seq && f.hdr.ty == MsgType::Status => {
                let ids = data.unwrap_or_else(|| panic!("{what} STATUS arrived before DATA"));
                let s = find(&f.payload, UbusAttr::Status)
                    .and_then(|a| a.as_u32())
                    .map(|u| u as i32)
                    .expect("STATUS attr");
                assert_eq!(s, UBUS_STATUS_OK, "{what} STATUS must be 0");
                return ids;
            }
            Ok(Some(_)) => {}
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(e) => panic!("{what} recv: {e:?}"),
        }
    }
    panic!("{what}: typed DATA-then-STATUS reply timed out");
}

pub fn recv_until(
    transport: &mut UnixUbusTransport,
    timeout: Duration,
    pred: impl Fn(&Frame) -> bool,
) -> Result<Frame, String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match transport.recv() {
            Ok(Some(f)) if pred(&f) => return Ok(f),
            Ok(Some(_)) => continue,
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(e) => return Err(format!("recv: {e:?}")),
        }
    }
    Err("recv_until: timeout".into())
}
