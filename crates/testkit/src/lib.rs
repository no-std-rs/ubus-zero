//! Dev-only fixtures for matrix-driven UBUS transport tests.

#[doc(hidden)]
pub mod __private {
    pub use paste::paste;
}

pub mod canon;
pub mod compat;
pub mod fd_peer;
pub mod golden;
pub mod matrix;
pub mod path;
pub mod raw_peer;
pub mod replay;
#[cfg(target_os = "linux")]
pub mod rpcd;
pub mod skip;
pub mod transport_pair;
pub mod ubus_cli;
pub mod ubusd;
pub mod uci_cli;
mod unix_transport;
pub mod wire_tap;

#[cfg(all(target_os = "linux", have_libubox))]
mod libubox_blob;
#[cfg(target_os = "linux")]
pub mod libubox_client;
#[cfg(target_os = "linux")]
pub mod libubox_host;

// Blobmsg↔JSON parity oracle backed by libblobmsg_json when available.
#[cfg(target_os = "linux")]
pub use libubox_sys::json;

pub use canon::{
    canonicalize, diff, diff_events, CanonError, CanonEvent, CanonEventKind, CanonTranscript,
};
pub use compat::{
    run_matrix_case, run_matrix_case_with, run_tapped, run_tapped_with, BrokerArm, BrokerOpts,
    MatrixArm,
};
pub use fd_peer::{pipe, FdPeer};
pub use golden::Golden;
pub use path::unique_socket_path;
pub use raw_peer::RawPeer;
#[cfg(target_os = "linux")]
pub use rpcd::{RpcdHarness, RPCD_LOGIN_READONLY, RPCD_LOGIN_READWRITE, RPCD_TEST_PASSWORD};
pub use transport_pair::TransportPair;
pub use ubus_cli::UbusCli;
pub use ubusd::UbusdHarness;
pub use uci_cli::{UciCli, UciEntry, UciSection};
pub use unix_transport::UnixUbusTransport;
pub use wire_tap::{TapDirection, TapEvent, TapEventKind, TapPolicy, WireTap};

#[cfg(target_os = "linux")]
pub use libubox_client::LibuboxClient;
#[cfg(target_os = "linux")]
pub use libubox_host::{
    DeferChoice, DeferredReply, HostEventHandler, HostObject, HostSubscriber, LibuboxHost,
};
