use alloc::string::String;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum UbusError {
    #[error("short buffer: need {need} bytes, have {have}")]
    Short { need: usize, have: usize },
    #[error("malformed message: {0}")]
    Malformed(&'static str),
    #[error("unknown message type {0}")]
    UnknownType(u8),
    #[error("unknown attribute type {0}")]
    UnknownAttr(u8),
    #[error("io: {0}")]
    Io(String),
    /// A method handler returned a domain-level error (missing arg,
    /// validation failure, downstream-side failure). The dispatcher
    /// converts this — like any handler `Err` — into STATUS=-3 to the
    /// peer; the inner string survives in `DispatchOutcome::HandlerErr`
    /// so the daemon can log a breadcrumb. Distinguished from `Io` so
    /// callers can branch on "this was the handler talking" vs "the
    /// transport blew up."
    #[error("method: {0}")]
    Method(String),
    #[error("ubus status {0}")]
    Status(i32),
    #[error("transport closed")]
    Closed,
    /// API misuse: caller invoked an operation that requires a
    /// successful ADD_OBJECT before the connection was registered.
    /// Distinct from `Malformed` (which means "the wire bytes from the
    /// peer were wrong") so callers can branch on it without string-
    /// matching.
    #[error("ubus operation requires a registered object")]
    NotRegistered,
    /// A request loop exhausted its idle-wait budget without a reply.
    /// The `&'static str` names the phase ("hello", "lookup", "invoke",
    /// "add_object") so callers can apply different retry policies for
    /// "broker silent" vs `Malformed` ("broker said the wrong thing").
    #[error("ubus timeout: {0} phase exceeded idle-wait budget")]
    Timeout(&'static str),
}
