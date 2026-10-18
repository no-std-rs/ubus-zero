//! Named, type-tagged layer over [`crate::blob`], mirroring
//! `<libubox/blobmsg.h>`. Every blobmsg attr sets the BLOB_ATTR_EXTENDED
//! bit on its id_len and is preceded by a name header (`name_len: u16
//! BE`, NUL-terminated name, pad to 4) before the inner payload.
//! "Anonymous" attrs (array elements) still carry a header with
//! `name_len = 0` — libubox emits one unconditionally and `blobmsg_data`
//! relies on the EXT bit, not on the array-vs-table context, to skip it.

#[cfg(feature = "alloc")]
use alloc::format;
#[cfg(feature = "alloc")]
use alloc::string::{String, ToString};
#[cfg(feature = "alloc")]
use alloc::vec::Vec;

use crate::blob::{EXT_FLAG, ID_MASK, ID_SHIFT, LEN_MASK};
use crate::error::UbusError;

#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum BlobType {
    Unspec = 0,
    Array = 1,
    Table = 2,
    String = 3,
    Int64 = 4,
    Int32 = 5,
    Int16 = 6,
    Int8 = 7,
    Double = 8,
}

impl BlobType {
    pub fn from_u8(v: u8) -> Result<Self, UbusError> {
        Ok(match v {
            0 => Self::Unspec,
            1 => Self::Array,
            2 => Self::Table,
            3 => Self::String,
            4 => Self::Int64,
            5 => Self::Int32,
            6 => Self::Int16,
            7 => Self::Int8,
            8 => Self::Double,
            v => return Err(UbusError::UnknownAttr(v)),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum BlobValue<'a> {
    String(&'a str),
    Int8(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Double(f64),
    Table(&'a [u8]),
    Array(&'a [u8]),
    Raw(&'a [u8]),
}

#[cfg(feature = "alloc")]
pub struct BlobBuilder {
    buf: Vec<u8>,
    /// Offsets into `buf` of currently-open container `id_len` fields,
    /// back-patched on close.
    open: Vec<usize>,
}

#[cfg(feature = "alloc")]
impl Default for BlobBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "alloc")]
impl BlobBuilder {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            open: Vec::new(),
        }
    }

    pub fn open_table(&mut self, name: Option<&str>) {
        self.open_container(BlobType::Table, name);
    }

    pub fn close_table(&mut self) {
        self.close_container();
    }

    pub fn open_array(&mut self, name: Option<&str>) {
        self.open_container(BlobType::Array, name);
    }

    pub fn close_array(&mut self) {
        self.close_container();
    }

    pub fn put_str(&mut self, name: Option<&str>, val: &str) {
        let mut payload = Vec::with_capacity(val.len() + 1);
        payload.extend_from_slice(val.as_bytes());
        payload.push(0);
        self.put_attr(BlobType::String, name, &payload);
    }

    pub fn put_i32(&mut self, name: Option<&str>, val: i32) {
        self.put_attr(BlobType::Int32, name, &val.to_be_bytes());
    }

    pub fn put_i64(&mut self, name: Option<&str>, val: i64) {
        self.put_attr(BlobType::Int64, name, &val.to_be_bytes());
    }

    pub fn put_double(&mut self, name: Option<&str>, val: f64) {
        self.put_attr(BlobType::Double, name, &val.to_bits().to_be_bytes());
    }

    /// Emits the UNSPEC representation libblobmsg_json uses for JSON
    /// `null`. Consumers use this for JSON-null parity.
    pub fn put_null(&mut self, name: Option<&str>) {
        self.put_attr(BlobType::Unspec, name, &[]);
    }

    pub fn put_bool(&mut self, name: Option<&str>, val: bool) {
        self.put_attr(BlobType::Int8, name, &[val as u8]);
    }

    /// Embed an existing table without decoding and rebuilding its attributes.
    pub fn put_table(&mut self, name: Option<&str>, table: &BlobMsgTable) {
        self.put_attr(BlobType::Table, name, table.contents());
    }

    /// Panics in debug if any container is still open. Release builds
    /// would emit a frame with the outer length field unpatched (zero)
    /// and ubusd would reject it; either way the bug would only show
    /// up on the wire, so the assert exists to fail loudly in tests.
    pub fn finish(self) -> Vec<u8> {
        debug_assert!(self.open.is_empty(), "unbalanced open_*/close_*");
        self.buf
    }

    /// Like `finish`, but returns a [`BlobMsgTable`] after debug-asserting
    /// the buffer parses as exactly one top-level TABLE attr. Use this
    /// when you've imperatively built `open_table(None)` … `close_table()`.
    pub fn finish_table(self) -> BlobMsgTable {
        let bytes = self.finish();
        if cfg!(debug_assertions) {
            BlobMsgTable::validate_single_table(&bytes)
                .expect("finish_table called on buffer that is not a single TABLE attr");
        }
        BlobMsgTable { bytes }
    }

    fn open_container(&mut self, ty: BlobType, name: Option<&str>) {
        let start = self.buf.len();
        let tag = ((ty as u32) << ID_SHIFT) | EXT_FLAG;
        self.buf.extend_from_slice(&tag.to_be_bytes());
        self.write_name_header(name);
        self.open.push(start);
    }

    fn close_container(&mut self) {
        let start = self.open.pop().expect("close without open");
        let total = self.buf.len() - start;
        assert!(
            total <= LEN_MASK as usize,
            "container size exceeds 24-bit length field"
        );
        // Preserve the EXT bit + 7 type bits set on open; only the
        // length field changes.
        let tag = u32::from_be_bytes(self.buf[start..start + 4].try_into().unwrap()) & !LEN_MASK;
        let id_len = tag | (total as u32 & LEN_MASK);
        self.buf[start..start + 4].copy_from_slice(&id_len.to_be_bytes());
    }

    fn put_attr(&mut self, ty: BlobType, name: Option<&str>, payload: &[u8]) {
        let start = self.buf.len();
        self.buf.extend_from_slice(&[0u8; 4]);
        self.write_name_header(name);
        self.buf.extend_from_slice(payload);
        let total = self.buf.len() - start;
        assert!(
            total <= LEN_MASK as usize,
            "attr size exceeds 24-bit length field"
        );
        let tag = ((ty as u32) << ID_SHIFT) | EXT_FLAG;
        let id_len = tag | (total as u32 & LEN_MASK);
        self.buf[start..start + 4].copy_from_slice(&id_len.to_be_bytes());
        // Pad outside id_len so peers walking the stream skip the same
        // number of bytes.
        let pad = (4 - (self.buf.len() & 3)) & 3;
        self.buf.resize(self.buf.len() + pad, 0);
    }

    /// libubox always emits a name header even when the caller passes a
    /// NULL name — `namelen=0`, a NUL terminator, and pad to 4.
    /// `blobmsg_data()` skips this on the EXT bit alone, so omitting the
    /// header here would make our payloads decode at the wrong offset on
    /// any real ubusd.
    fn write_name_header(&mut self, name: Option<&str>) {
        let name_bytes = name.map(str::as_bytes).unwrap_or(&[]);
        let len: u16 = name_bytes
            .len()
            .try_into()
            .expect("blobmsg attr name exceeds 65535 bytes");
        self.buf.extend_from_slice(&len.to_be_bytes());
        self.buf.extend_from_slice(name_bytes);
        self.buf.push(0);
        let used = 2 + name_bytes.len() + 1;
        let pad = (4 - (used & 3)) & 3;
        self.buf.resize(self.buf.len() + pad, 0);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BlobRef<'a> {
    pub ty: BlobType,
    pub name: Option<&'a str>,
    pub payload: &'a [u8],
}

impl<'a> BlobRef<'a> {
    pub fn value(&self) -> Result<BlobValue<'a>, UbusError> {
        Ok(match self.ty {
            BlobType::String => {
                let p = self.payload;
                let end = p
                    .iter()
                    .position(|&b| b == 0)
                    .ok_or(UbusError::Malformed("unterminated string"))?;
                BlobValue::String(
                    core::str::from_utf8(&p[..end])
                        .map_err(|_| UbusError::Malformed("non-utf8 string"))?,
                )
            }
            BlobType::Int8 => {
                if self.payload.is_empty() {
                    return Err(UbusError::Malformed("short int8"));
                }
                BlobValue::Int8(self.payload[0] as i8)
            }
            BlobType::Int16 => {
                if self.payload.len() < 2 {
                    return Err(UbusError::Malformed("short int16"));
                }
                BlobValue::Int16(i16::from_be_bytes([self.payload[0], self.payload[1]]))
            }
            BlobType::Int32 => {
                if self.payload.len() < 4 {
                    return Err(UbusError::Malformed("short int32"));
                }
                BlobValue::Int32(i32::from_be_bytes([
                    self.payload[0],
                    self.payload[1],
                    self.payload[2],
                    self.payload[3],
                ]))
            }
            BlobType::Int64 => {
                if self.payload.len() < 8 {
                    return Err(UbusError::Malformed("short int64"));
                }
                let mut a = [0u8; 8];
                a.copy_from_slice(&self.payload[..8]);
                BlobValue::Int64(i64::from_be_bytes(a))
            }
            BlobType::Double => {
                if self.payload.len() < 8 {
                    return Err(UbusError::Malformed("short double"));
                }
                let mut a = [0u8; 8];
                a.copy_from_slice(&self.payload[..8]);
                BlobValue::Double(f64::from_bits(u64::from_be_bytes(a)))
            }
            BlobType::Table => BlobValue::Table(self.payload),
            BlobType::Array => BlobValue::Array(self.payload),
            BlobType::Unspec => BlobValue::Raw(self.payload),
        })
    }

    pub fn as_str(&self) -> Option<&'a str> {
        match self.value().ok()? {
            BlobValue::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i32(&self) -> Option<i32> {
        match self.value().ok()? {
            BlobValue::Int32(n) => Some(n),
            // i64 → i32 must round-trip; silent truncation here would
            // mask wire bugs where a 64-bit value is read as a much
            // smaller (or sign-flipped) 32-bit value.
            BlobValue::Int64(n) => i32::try_from(n).ok(),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self.value().ok()? {
            BlobValue::Double(n) => Some(n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self.value().ok()? {
            BlobValue::Int8(n) => Some(n != 0),
            _ => None,
        }
    }
}

/// Walks a blobmsg attr stream. Every attr is expected to carry the
/// EXT bit and a name header; an attr with EXT cleared is rejected as
/// malformed (it would be a bare blob attr in the wrong layer).
/// "Anonymous" attrs (array elements) parse with `name = None` because
/// their stored `namelen` is zero.
pub struct BlobIter<'a> {
    buf: &'a [u8],
}

impl<'a> BlobIter<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    /// Result-yielding parse step. Use this when the *absence* of
    /// further attrs needs to be distinguished from a corrupted blobmsg
    /// stream (e.g. server-side INVOKE dispatch). On `Err`, the iterator
    /// is drained — TLV chains can't be safely resumed past corruption.
    pub fn try_next(&mut self) -> Result<Option<BlobRef<'a>>, UbusError> {
        if self.buf.is_empty() {
            return Ok(None);
        }
        if self.buf.len() < 4 {
            // Capture before draining: callers inspect `have` to size
            // the next read; reporting 0 after `self.buf = &[]` would
            // be a lie.
            let have = self.buf.len();
            self.buf = &[];
            return Err(UbusError::Short { need: 4, have });
        }
        let id_len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
        if (id_len & EXT_FLAG) == 0 {
            self.buf = &[];
            return Err(UbusError::Malformed("blobmsg attr missing EXT bit"));
        }
        let ty_raw = ((id_len & ID_MASK) >> ID_SHIFT) as u8;
        let ty = match BlobType::from_u8(ty_raw) {
            Ok(t) => t,
            Err(e) => {
                self.buf = &[];
                return Err(e);
            }
        };
        let total = (id_len & LEN_MASK) as usize;
        if total < 4 || total > self.buf.len() {
            self.buf = &[];
            return Err(UbusError::Malformed("blobmsg attr length"));
        }
        let mut cursor = 4;
        if cursor + 2 > total {
            self.buf = &[];
            return Err(UbusError::Malformed("blobmsg name header"));
        }
        let nlen = u16::from_be_bytes([self.buf[cursor], self.buf[cursor + 1]]) as usize;
        cursor += 2;
        if cursor + nlen + 1 > total {
            self.buf = &[];
            return Err(UbusError::Malformed("blobmsg name overflow"));
        }
        let name_bytes = &self.buf[cursor..cursor + nlen];
        cursor += nlen + 1;
        let used = 2 + nlen + 1;
        cursor += (4 - (used & 3)) & 3;
        let name = if nlen == 0 {
            None
        } else {
            match core::str::from_utf8(name_bytes) {
                Ok(s) => Some(s),
                Err(_) => {
                    self.buf = &[];
                    return Err(UbusError::Malformed("non-utf8 attr name"));
                }
            }
        };
        if cursor > total {
            self.buf = &[];
            return Err(UbusError::Malformed("blobmsg cursor past total"));
        }
        let payload = &self.buf[cursor..total];
        let pad = (4 - (total & 3)) & 3;
        let consumed = (total + pad).min(self.buf.len());
        self.buf = &self.buf[consumed..];
        Ok(Some(BlobRef { ty, name, payload }))
    }
}

/// Silently terminates on parse error — best-effort walk for `lookup` /
/// `to_json`. Strict consumers (server INVOKE dispatch, client reply
/// extraction) should drive `try_next` to surface `Malformed`.
impl<'a> Iterator for BlobIter<'a> {
    type Item = BlobRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.try_next().unwrap_or_default()
    }
}

pub fn lookup<'a>(buf: &'a [u8], key: &str) -> Option<BlobRef<'a>> {
    BlobIter::new(buf).find(|a| a.name == Some(key))
}

/// Strict variant of [`lookup`]: surfaces parse errors instead of
/// collapsing "stream corrupt before key" with "key absent". Use this
/// when the absence of the key matters (e.g. parsing untrusted INVOKE
/// args) and "broker sent garbage" must be distinguishable from "the
/// caller didn't include this field".
pub fn try_lookup<'a>(buf: &'a [u8], key: &str) -> Result<Option<BlobRef<'a>>, UbusError> {
    let mut it = BlobIter::new(buf);
    while let Some(a) = it.try_next()? {
        if a.name == Some(key) {
            return Ok(Some(a));
        }
    }
    Ok(None)
}

/// Owns a buffer that contains exactly one top-level blobmsg TABLE
/// attr, the on-wire shape ubusd's `Data` attr expects. Use
/// [`BlobMsgTable::build`] to construct one — the closure runs inside
/// an already-opened TABLE so the result is correct by construction.
/// `BlobBuilder::finish_table` is the imperative-style escape hatch
/// (debug-asserts the shape).
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg(feature = "alloc")]
pub struct BlobMsgTable {
    bytes: Vec<u8>,
}

#[cfg(feature = "alloc")]
impl BlobMsgTable {
    pub fn build(f: impl FnOnce(&mut BlobBuilder)) -> Self {
        let mut b = BlobBuilder::new();
        b.open_table(None);
        f(&mut b);
        b.close_table();
        Self { bytes: b.finish() }
    }

    pub fn empty() -> Self {
        Self::build(|_| {})
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the *inner* contents of the wrapper TABLE (i.e. the
    /// bytes a libubox `blob_data(table_attr)` call would yield) — the
    /// shape that goes directly under `UBUS_ATTR_DATA` on the wire.
    /// libubox's `ubus_invoke_async_fd` puts `blob_data(msg)` /
    /// `blob_len(msg)` under DATA, so being byte-compatible means we
    /// also send contents-without-wrapper. An `empty()` table yields a
    /// zero-length slice, matching libubox's `blob_put(DATA, NULL, 0)`
    /// no-args shape.
    pub fn contents(&self) -> &[u8] {
        BlobIter::new(&self.bytes)
            .next()
            .map(|a| a.payload)
            .unwrap_or(&[])
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// `lookup(key)` walks the wrapped table's *contents*, hiding the
    /// outer TABLE strip from callers.
    pub fn lookup(&self, key: &str) -> Option<BlobRef<'_>> {
        let outer = BlobIter::new(&self.bytes).next()?;
        lookup(outer.payload, key)
    }

    /// Returns `Ok` if `bytes` parses as exactly one top-level TABLE
    /// attr. Used by `BlobBuilder::finish_table` to debug-validate
    /// imperative builds without paying for parse on every call in
    /// release.
    fn validate_single_table(bytes: &[u8]) -> Result<(), UbusError> {
        let mut it = BlobIter::new(bytes);
        let outer = it
            .try_next()?
            .ok_or(UbusError::Malformed("empty BlobMsgTable"))?;
        if outer.ty != BlobType::Table {
            return Err(UbusError::Malformed("BlobMsgTable outer attr is not TABLE"));
        }
        if it.try_next()?.is_some() {
            return Err(UbusError::Malformed(
                "BlobMsgTable has more than one top-level attr",
            ));
        }
        Ok(())
    }
}

/// Best-effort JSON render. `named` only controls outer brackets
/// (`{}` vs `[]`); the per-attr name header is always present on the
/// wire and rendered when non-empty.
#[cfg(feature = "alloc")]
pub fn to_json(buf: &[u8], named: bool) -> String {
    let mut s = String::new();
    if named {
        s.push('{');
    } else {
        s.push('[');
    }
    let mut first = true;
    for a in BlobIter::new(buf) {
        if !first {
            s.push(',');
        }
        first = false;
        if let Some(n) = a.name {
            s.push('"');
            json_escape_into(&mut s, n);
            s.push_str("\":");
        }
        match a.value().ok() {
            Some(BlobValue::String(v)) => {
                s.push('"');
                json_escape_into(&mut s, v);
                s.push('"');
            }
            // libubox uses int8 for booleans.
            Some(BlobValue::Int8(v)) => s.push_str(if v != 0 { "true" } else { "false" }),
            Some(BlobValue::Int16(v)) => push_i64(&mut s, v as i64),
            Some(BlobValue::Int32(v)) => push_i64(&mut s, v as i64),
            Some(BlobValue::Int64(v)) => push_i64(&mut s, v),
            Some(BlobValue::Double(v)) => s.push_str(&fmt_double_g17(v)),
            Some(BlobValue::Table(p)) => s.push_str(&to_json(p, true)),
            Some(BlobValue::Array(p)) => s.push_str(&to_json(p, false)),
            _ => s.push_str("null"),
        }
    }
    if named {
        s.push('}');
    } else {
        s.push(']');
    }
    s
}

#[cfg(feature = "alloc")]
fn json_escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use core::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

/// Formats a double as libblobmsg_json's `%.17g` output.
#[cfg(feature = "alloc")]
fn fmt_double_g17(v: f64) -> String {
    if v.is_nan() {
        return if v.is_sign_negative() {
            "-nan".into()
        } else {
            "nan".into()
        };
    }
    if v.is_infinite() {
        return if v.is_sign_negative() {
            "-inf".into()
        } else {
            "inf".into()
        };
    }

    let rendered = format!("{v:.16e}");
    let (negative, rendered) = match rendered.strip_prefix('-') {
        Some(rendered) => (true, rendered),
        None => (false, rendered.as_str()),
    };
    let (mantissa, exponent) = rendered
        .split_once('e')
        .expect("f64 LowerExp format must contain an exponent");
    let mut digits = String::with_capacity(17);
    for c in mantissa.chars() {
        if c != '.' {
            digits.push(c);
        }
    }
    let exponent: i32 = exponent
        .parse()
        .expect("f64 LowerExp format must have a decimal exponent");

    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if !(-4..17).contains(&exponent) {
        out.push_str(&digits[..1]);
        let fraction = digits[1..].trim_end_matches('0');
        if !fraction.is_empty() {
            out.push('.');
            out.push_str(fraction);
        }
        out.push('e');
        out.push(if exponent < 0 { '-' } else { '+' });
        let exponent = exponent.unsigned_abs();
        if exponent < 10 {
            out.push('0');
        }
        out.push_str(&exponent.to_string());
    } else if exponent >= 0 {
        let split = exponent as usize + 1;
        out.push_str(&digits[..split]);
        let fraction = digits[split..].trim_end_matches('0');
        if !fraction.is_empty() {
            out.push('.');
            out.push_str(fraction);
        }
    } else {
        out.push_str("0.");
        for _ in 0..(-exponent - 1) {
            out.push('0');
        }
        out.push_str(digits.trim_end_matches('0'));
    }
    out
}

#[cfg(feature = "alloc")]
fn push_i64(out: &mut String, v: i64) {
    out.push_str(&v.to_string());
}

#[cfg(all(test, feature = "alloc"))]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_string_int_bool() {
        let mut b = BlobBuilder::new();
        b.open_table(None);
        b.put_str(Some("hello"), "world");
        b.put_i32(Some("n"), 42);
        b.put_bool(Some("ok"), true);
        b.close_table();
        let buf = b.finish();

        let outer = BlobIter::new(&buf).next().unwrap();
        assert_eq!(outer.ty, BlobType::Table);
        let inner = outer.payload;
        let hello = lookup(inner, "hello").unwrap();
        assert_eq!(hello.as_str(), Some("world"));
        let n = lookup(inner, "n").unwrap();
        assert_eq!(n.as_i32(), Some(42));
        let ok = lookup(inner, "ok").unwrap();
        assert_eq!(ok.as_bool(), Some(true));
    }

    #[test]
    fn array_anonymous_attrs() {
        let mut b = BlobBuilder::new();
        b.open_array(None);
        b.put_str(None, "a");
        b.put_str(None, "b");
        b.close_array();
        let buf = b.finish();
        let outer = BlobIter::new(&buf).next().unwrap();
        assert_eq!(outer.ty, BlobType::Array);
        let items: alloc::vec::Vec<&str> = BlobIter::new(outer.payload)
            .filter_map(|a| a.as_str())
            .collect();
        assert_eq!(items, alloc::vec!["a", "b"]);
    }

    #[test]
    fn json_render() {
        let mut b = BlobBuilder::new();
        b.open_table(None);
        b.put_str(Some("k"), "v\"esc");
        b.put_i32(Some("n"), -7);
        b.close_table();
        let buf = b.finish();
        let outer = BlobIter::new(&buf).next().unwrap();
        let j = to_json(outer.payload, true);
        assert!(j.contains("\"k\":\"v\\\"esc\""), "{j}");
        assert!(j.contains("\"n\":-7"), "{j}");
    }

    #[test]
    fn double_wire_shape_and_value_round_trip() {
        let mut b = BlobBuilder::new();
        b.open_table(None);
        b.put_double(Some("x"), 2.5);
        b.put_double(Some("negative"), -0.5);
        b.close_table();
        let buf = b.finish();

        let outer = BlobIter::new(&buf).next().unwrap();
        let x = lookup(outer.payload, "x").unwrap();
        assert_eq!(x.ty, BlobType::Double);
        assert_eq!(x.payload, &[0x40, 0x04, 0, 0, 0, 0, 0, 0]);
        assert_eq!(x.value().unwrap(), BlobValue::Double(2.5));
        assert_eq!(x.as_f64(), Some(2.5));
        assert_eq!(
            lookup(outer.payload, "negative").unwrap().value().unwrap(),
            BlobValue::Double(-0.5)
        );
    }

    #[test]
    fn double_value_rejects_short_payload() {
        let attr = BlobRef {
            ty: BlobType::Double,
            name: None,
            payload: &[0; 7],
        };
        assert!(matches!(
            attr.value(),
            Err(UbusError::Malformed("short double"))
        ));
    }

    #[test]
    fn as_f64_is_strict() {
        let mut b = BlobBuilder::new();
        b.put_i32(None, 2);
        let buf = b.finish();
        assert_eq!(BlobIter::new(&buf).next().unwrap().as_f64(), None);
    }

    #[test]
    fn json_render_double_and_nan() {
        let mut b = BlobBuilder::new();
        b.open_table(None);
        b.put_double(Some("x"), 2.5);
        b.put_double(Some("nan"), f64::NAN);
        b.close_table();
        let buf = b.finish();
        let outer = BlobIter::new(&buf).next().unwrap();
        assert_eq!(to_json(outer.payload, true), "{\"x\":2.5,\"nan\":nan}");
    }

    #[test]
    fn fmt_double_matches_c_printf_g17() {
        let cases = [
            (0.1, "0.10000000000000001"),
            (1e20, "1e+20"),
            (100.0, "100"),
            (f64::from_bits(1), "4.9406564584124654e-324"),
            (core::f64::consts::PI, "3.1415926535897931"),
            (0.30000000000000004, "0.30000000000000004"),
            (1e308, "1e+308"),
            (-2.75, "-2.75"),
            (0.001, "0.001"),
            (1e-5, "1.0000000000000001e-05"),
            (1.25e-4, "0.000125"),
            (0.0001, "0.0001"),
            (1e17, "1e+17"),
            (123456789012345680.0, "1.2345678901234568e+17"),
            (
                f64::from_bits(0x000f_ffff_ffff_ffff),
                "2.2250738585072009e-308",
            ),
            (0.0, "0"),
            (-0.0, "-0"),
            (1.5, "1.5"),
            (f64::NAN, "nan"),
            (f64::from_bits(0xfff8_0000_0000_0000), "-nan"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
        ];

        for (value, expected) in cases {
            assert_eq!(fmt_double_g17(value), expected, "value {value:?}");
        }
    }

    #[test]
    fn json_render_int8_nonzero_as_true() {
        let attr = [0x87, 0, 0, 9, 0, 1, b'x', 0, 5, 0, 0, 0];
        assert_eq!(to_json(&attr, true), "{\"x\":true}");
    }

    #[test]
    fn json_render_backspace_escape() {
        let mut b = BlobBuilder::new();
        b.open_table(None);
        b.put_str(Some("x"), "a\u{0008}b");
        b.close_table();
        let buf = b.finish();
        let outer = BlobIter::new(&buf).next().unwrap();
        assert_eq!(to_json(outer.payload, true), "{\"x\":\"a\\bb\"}");
    }

    #[test]
    fn put_null_wire_shape_and_render() {
        let mut b = BlobBuilder::new();
        b.put_null(Some("n"));
        assert_eq!(b.finish(), [0x80, 0, 0, 0x08, 0, 1, b'n', 0]);

        let mut b = BlobBuilder::new();
        b.open_table(None);
        b.put_null(Some("n"));
        b.close_table();
        let buf = b.finish();
        let outer = BlobIter::new(&buf).next().unwrap();
        assert_eq!(to_json(outer.payload, true), "{\"n\":null}");
    }

    /// Pin the libubox wire shape: every blobmsg attr writes
    /// `BLOB_ATTR_EXTENDED` and a name header (with `namelen=0` for
    /// anonymous attrs). Without this match `blobmsg_data()` on a real
    /// ubusd would slice the payload at the wrong offset.
    #[test]
    fn libubox_wire_shape_ext_bit_and_name_header() {
        // Anonymous string attr: id_len bit 31 set, type=String, namelen=0.
        let mut b = BlobBuilder::new();
        b.put_str(None, "x");
        let buf = b.finish();
        let id_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        assert_ne!(id_len & EXT_FLAG, 0, "EXT bit must be set");
        assert_eq!(
            ((id_len & ID_MASK) >> ID_SHIFT) as u8,
            BlobType::String as u8
        );
        // Bytes 4..6 are the BE name length (namelen=0 for anonymous).
        assert_eq!(&buf[4..6], &[0, 0]);
        // Byte 6 is the name's NUL terminator; byte 7 is the name-pad.
        assert_eq!(buf[6], 0);
        assert_eq!(buf[7], 0);
        // Then the payload bytes ("x\0") and trailing pad.
        assert_eq!(&buf[8..10], b"x\0");

        // Named attr: namelen is in BE at offset 4..6.
        let mut b = BlobBuilder::new();
        b.put_str(Some("k"), "v");
        let buf = b.finish();
        let id_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        assert_ne!(id_len & EXT_FLAG, 0);
        assert_eq!(&buf[4..6], &[0, 1]); // namelen = 1
        assert_eq!(&buf[6..8], b"k\0");
        // Container open/close also sets EXT.
        let mut b = BlobBuilder::new();
        b.open_table(None);
        b.close_table();
        let buf = b.finish();
        let id_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        assert_ne!(id_len & EXT_FLAG, 0);
        assert_eq!(
            ((id_len & ID_MASK) >> ID_SHIFT) as u8,
            BlobType::Table as u8
        );
    }

    #[test]
    fn parser_rejects_attr_without_ext_bit() {
        // Hand-craft a TABLE attr with EXT cleared: the parser must
        // reject it rather than silently mis-decoding the payload.
        let mut buf = alloc::vec::Vec::new();
        let id_len: u32 = ((BlobType::Table as u32) << ID_SHIFT) | 8; // total=8, EXT=0
        buf.extend_from_slice(&id_len.to_be_bytes());
        buf.extend_from_slice(&[0, 0, 0, 0]); // body
        let mut it = BlobIter::new(&buf);
        assert!(matches!(
            it.try_next(),
            Err(UbusError::Malformed("blobmsg attr missing EXT bit"))
        ));
    }

    #[test]
    fn parser_rejects_non_utf8_attr_name() {
        // Build a valid attr, then poison the name bytes with 0xFF.
        let mut b = BlobBuilder::new();
        b.put_str(Some("ok"), "v");
        let mut buf = b.finish();
        // Layout: 4 id_len + 2 namelen + 2 name+NUL + payload+pad
        // Name is at offset 6..8 ("ok"); replace 'o' with 0xFF.
        buf[6] = 0xFF;
        let mut it = BlobIter::new(&buf);
        assert!(matches!(
            it.try_next(),
            Err(UbusError::Malformed("non-utf8 attr name"))
        ));
    }

    #[test]
    fn parser_rejects_non_utf8_string_payload() {
        let mut b = BlobBuilder::new();
        b.put_str(None, "v");
        let mut buf = b.finish();
        // Anonymous attr: 4 id_len + 4 (namelen=0+NUL+pad) + payload "v\0"
        buf[8] = 0xFF;
        let mut it = BlobIter::new(&buf);
        let attr = it.try_next().unwrap().unwrap();
        assert!(matches!(
            attr.value(),
            Err(UbusError::Malformed("non-utf8 string"))
        ));
    }

    #[test]
    fn parser_rejects_total_len_smaller_than_name_header() {
        // total=6 declares 4 header + 2 body, but body=2 isn't enough
        // for the 2-byte namelen field plus the NUL.
        let mut buf = alloc::vec::Vec::new();
        let id_len: u32 = ((BlobType::String as u32) << ID_SHIFT) | EXT_FLAG | 6;
        buf.extend_from_slice(&id_len.to_be_bytes());
        buf.extend_from_slice(&[0, 0]); // claims namelen=0 but no NUL
        let mut it = BlobIter::new(&buf);
        assert!(matches!(
            it.try_next(),
            Err(UbusError::Malformed("blobmsg name overflow"))
        ));
    }

    #[test]
    fn parser_rejects_name_len_overflowing_attr_total() {
        // Declared namelen exceeds the attr's total length.
        let mut buf = alloc::vec::Vec::new();
        let id_len: u32 = ((BlobType::String as u32) << ID_SHIFT) | EXT_FLAG | 8;
        buf.extend_from_slice(&id_len.to_be_bytes());
        buf.extend_from_slice(&[0, 100]); // namelen = 100
        buf.extend_from_slice(&[0, 0]); // pretend body
        let mut it = BlobIter::new(&buf);
        assert!(matches!(
            it.try_next(),
            Err(UbusError::Malformed("blobmsg name overflow"))
        ));
    }

    /// A peer that sends a non-NUL-terminated string is producing a
    /// wire violation; libubox always NUL-terminates. The parser must
    /// surface that as Malformed instead of returning a "string ends
    /// at payload end" approximation.
    #[test]
    fn string_value_rejects_unterminated_payload() {
        // Hand-craft a String attr whose payload is "x" (no NUL).
        let mut buf = alloc::vec::Vec::new();
        let total: u32 = 9; // 4 id_len + 4 name-hdr (namelen=0 + NUL + pad) + 1 byte
        let id_len: u32 = ((BlobType::String as u32) << ID_SHIFT) | EXT_FLAG | total;
        buf.extend_from_slice(&id_len.to_be_bytes());
        buf.extend_from_slice(&[0, 0, 0, 0]); // namelen=0, NUL, pad
        buf.push(b'x');
        // Trailing pad up to align4(9) = 12.
        buf.extend_from_slice(&[0, 0, 0]);
        let attr = BlobIter::new(&buf).next().unwrap();
        assert!(matches!(
            attr.value(),
            Err(UbusError::Malformed("unterminated string"))
        ));
    }

    #[test]
    fn blobmsg_table_build_round_trips() {
        let table = BlobMsgTable::build(|b| {
            b.put_str(Some("plmn"), "26201");
            b.put_i32(Some("tac"), 0x1234);
        });
        // External slice access: feeds straight into wire encoders.
        assert!(!table.as_slice().is_empty());
        // Convenience lookup strips the outer TABLE before walking.
        assert_eq!(table.lookup("plmn").and_then(|a| a.as_str()), Some("26201"));
        assert_eq!(table.lookup("tac").and_then(|a| a.as_i32()), Some(0x1234));
        assert!(table.lookup("missing").is_none());
    }

    #[test]
    fn blobmsg_table_empty_is_valid_table() {
        let empty = BlobMsgTable::empty();
        BlobMsgTable::validate_single_table(empty.as_slice()).unwrap();
    }

    #[test]
    fn finish_table_accepts_imperative_table_build() {
        let mut b = BlobBuilder::new();
        b.open_table(None);
        b.put_str(Some("k"), "v");
        b.close_table();
        let table = b.finish_table();
        assert_eq!(table.lookup("k").and_then(|a| a.as_str()), Some("v"));
    }

    /// In debug, `finish_table` must reject buffers that don't carry
    /// exactly one top-level TABLE — silently passing them through
    /// would defeat the type's invariant.
    #[test]
    #[should_panic(expected = "finish_table called on buffer that is not a single TABLE attr")]
    #[cfg(debug_assertions)]
    fn finish_table_rejects_non_table_buffer() {
        let mut b = BlobBuilder::new();
        b.put_str(None, "loose"); // top-level non-TABLE attr
        let _ = b.finish_table();
    }
}
