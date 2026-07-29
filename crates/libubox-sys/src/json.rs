//! Safe, optional access to libblobmsg_json for codec parity tests.

/// Whether the real `libblobmsg_json` reference implementation is available.
pub const AVAILABLE_JSON: bool = cfg!(have_blobmsg_json);

/// Reference encoding of a JSON object via the real libblobmsg_json.
pub struct Reference {
    /// The member-attr region of the built table (== `blob_data(head)` for
    /// `blob_len(head)` bytes) — comparable to `BlobMsgTable::contents()`.
    pub inner_bytes: Vec<u8>,
    /// `blobmsg_format_json(head, true)` — the reference JSON rendering.
    pub formatted: String,
}

/// Parse `json` (must be a JSON object — the C parser rejects top-level
/// arrays) into a blobmsg table via `blobmsg_add_json_from_string`, then
/// render it back with `blobmsg_format_json`. Returns `None` when the
/// reference library is unavailable, the JSON is rejected, or the formatted
/// output is not UTF-8.
pub fn reference_from_json(json: &str) -> Option<Reference> {
    #[cfg(have_blobmsg_json)]
    {
        reference_from_json_impl(json)
    }
    #[cfg(not(have_blobmsg_json))]
    {
        let _ = json;
        None
    }
}

/// Render raw blobmsg attr bytes through the reference formatter.
/// `attr_bytes` must be one complete, well-formed blobmsg attr (header +
/// name header + payload [+ padding]), e.g. `BlobMsgTable::as_slice()` or a
/// single member attr slice; feeding arbitrary bytes is UB in the C code.
/// `list=true` formats the attr's members (`{...}`/`[...]`); `list=false`
/// formats the single element as `"name":value`.
pub fn format_attr_bytes(attr_bytes: &[u8], list: bool) -> Option<String> {
    #[cfg(have_blobmsg_json)]
    {
        format_attr_bytes_impl(attr_bytes, list)
    }
    #[cfg(not(have_blobmsg_json))]
    {
        let _ = (attr_bytes, list);
        None
    }
}

#[cfg(have_blobmsg_json)]
use std::ffi::{CStr, CString};

#[cfg(have_blobmsg_json)]
use crate as sys;

#[cfg(have_blobmsg_json)]
struct BlobBufGuard {
    buf: sys::blob_buf,
}

#[cfg(have_blobmsg_json)]
impl BlobBufGuard {
    fn new() -> Option<Self> {
        // SAFETY: `blob_buf` is opaque Rust storage. Zeroed storage is the
        // initial state expected by `blob_buf_init`, which initializes it
        // before any other operation observes it.
        let mut guard = Self {
            buf: unsafe { core::mem::MaybeUninit::<sys::blob_buf>::zeroed().assume_init() },
        };
        // SAFETY: `guard.buf` is valid writable storage for libubox's blob
        // buffer. The guard frees it even if initialization reports failure.
        (unsafe { sys::blob_buf_init(&mut guard.buf, 0) } == 0).then_some(guard)
    }
}

#[cfg(have_blobmsg_json)]
impl Drop for BlobBufGuard {
    fn drop(&mut self) {
        // SAFETY: this guard exclusively owns its zero-initialized blob
        // buffer, which was passed to `blob_buf_init` before it was used.
        unsafe { sys::blob_buf_free(&mut self.buf) };
    }
}

#[cfg(have_blobmsg_json)]
fn reference_from_json_impl(json: &str) -> Option<Reference> {
    let input = CString::new(json).ok()?;
    let (inner_bytes, formatted) = {
        let mut buf = BlobBufGuard::new()?;
        // SAFETY: `buf` is initialized and `input` is a NUL-terminated
        // string that remains valid for this immediate C call.
        if !unsafe { sys::blobmsg_add_json_from_string(&mut buf.buf, input.as_ptr()) } {
            return None;
        }
        // SAFETY: a successful parser call builds a table rooted at `head`.
        let head = unsafe { sys::blob_buf_head(&buf.buf) };
        if head.is_null() {
            return None;
        }
        // SAFETY: `head` belongs to the live blob buffer; these accessors
        // expose its member-attribute region and length.
        let (data, len) = unsafe { (sys::blob_data(head), sys::blob_len(head) as usize) };
        let inner_bytes = if len == 0 {
            Vec::new()
        } else {
            if data.is_null() {
                return None;
            }
            // SAFETY: `data` covers exactly `len` bytes while `buf` remains
            // alive.
            unsafe { core::slice::from_raw_parts(data.cast::<u8>(), len) }.to_vec()
        };
        (inner_bytes, format_attr(head, true)?)
    };
    Some(Reference {
        inner_bytes,
        formatted: String::from_utf8(formatted).ok()?,
    })
}

#[cfg(have_blobmsg_json)]
fn format_attr_bytes_impl(attr_bytes: &[u8], list: bool) -> Option<String> {
    if attr_bytes.len() < 4 {
        return None;
    }
    let mut aligned = vec![0u64; attr_bytes.len().div_ceil(8)];
    // SAFETY: the u64 allocation is 8-byte aligned and has at least
    // `attr_bytes.len()` bytes. The source and destination do not overlap.
    unsafe {
        core::ptr::copy_nonoverlapping(
            attr_bytes.as_ptr(),
            aligned.as_mut_ptr().cast::<u8>(),
            attr_bytes.len(),
        );
    }
    let attr = aligned.as_mut_ptr().cast::<sys::blob_attr>();
    let formatted = format_attr(attr, list)?;
    String::from_utf8(formatted).ok()
}

#[cfg(have_blobmsg_json)]
fn format_attr(attr: *mut sys::blob_attr, list: bool) -> Option<Vec<u8>> {
    // SAFETY: callers pass a complete, well-formed, suitably aligned blobmsg
    // attribute that remains alive throughout the formatting call.
    let rendered = unsafe { sys::blobmsg_format_json(attr, list) };
    if rendered.is_null() {
        return None;
    }
    // SAFETY: libblobmsg_json returns a NUL-terminated, malloc-owned string.
    let bytes = unsafe { CStr::from_ptr(rendered).to_bytes().to_vec() };
    // SAFETY: `rendered` was allocated by libblobmsg_json and is freed once,
    // after its contents have been copied into Rust-owned storage.
    unsafe { sys::ubus_zero_free(rendered.cast()) };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::{format_attr_bytes, reference_from_json, AVAILABLE_JSON};

    #[test]
    fn reference_simple_object() {
        if !AVAILABLE_JSON {
            eprintln!("SKIP libblobmsg_json unavailable");
            return;
        }
        let reference = reference_from_json(r#"{"a":"b"}"#).expect("reference JSON");
        assert_eq!(reference.formatted, r#"{"a":"b"}"#);
        assert_eq!(
            reference.inner_bytes,
            [0x83, 0, 0, 0x0a, 0, 1, 0x61, 0, 0x62, 0, 0, 0]
        );
    }

    #[test]
    fn format_single_nested_member() {
        if !AVAILABLE_JSON {
            eprintln!("SKIP libblobmsg_json unavailable");
            return;
        }
        let reference =
            reference_from_json(r#"{"t":{"x":[1,2,"three"]}}"#).expect("reference JSON");
        assert_eq!(
            format_attr_bytes(&reference.inner_bytes, true),
            Some(r#"{"x":[1,2,"three"]}"#.into())
        );
        assert_eq!(
            format_attr_bytes(&reference.inner_bytes, false),
            Some(r#""t":{"x":[1,2,"three"]}"#.into())
        );
    }

    #[test]
    fn rejects_top_level_array() {
        if !AVAILABLE_JSON {
            eprintln!("SKIP libblobmsg_json unavailable");
            return;
        }
        assert!(reference_from_json("[1,2]").is_none());
    }

    #[test]
    fn rejects_invalid_json() {
        if !AVAILABLE_JSON {
            eprintln!("SKIP libblobmsg_json unavailable");
            return;
        }
        assert!(reference_from_json("not json").is_none());
    }

    #[test]
    fn formats_empty_object() {
        if !AVAILABLE_JSON {
            eprintln!("SKIP libblobmsg_json unavailable");
            return;
        }
        let reference = reference_from_json("{}").expect("reference JSON");
        assert!(reference.inner_bytes.is_empty());
        assert_eq!(reference.formatted, "{}");
    }
}
