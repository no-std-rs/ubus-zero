//! Bare `blob_attr` TLV codec, mirroring `<libubox/blob.h>`. Layout:
//!
//! ```text
//!   bit 31      bit 30..24      bit 23..0
//!   ┌────────┬───────────────┬────────────────────────────────┐
//!   │  EXT   │  id (7 bit)   │  total length, header included │
//!   └────────┴───────────────┴────────────────────────────────┘
//! ```
//!
//! `EXT = 1` means the payload carries an embedded name (blobmsg).

use alloc::vec::Vec;

use crate::error::UbusError;

pub const ALIGN: usize = 4;
pub const HDR_LEN: usize = 4;
pub const EXT_FLAG: u32 = 0x8000_0000;
pub const ID_MASK: u32 = 0x7f00_0000;
pub const ID_SHIFT: u32 = 24;
pub const LEN_MASK: u32 = 0x00ff_ffff;

#[inline]
pub fn align4(n: usize) -> usize {
    (n + (ALIGN - 1)) & !(ALIGN - 1)
}

/// Pad bytes are NOT counted in `total_len`; they exist only to align
/// the next attribute to 4 bytes (matches libubox byte-for-byte).
///
/// Panics if `payload.len() + HDR_LEN` exceeds the 24-bit `total_len`
/// field (≈16 MiB). Real ubusd frames are kilobyte-scale; truncation
/// here would silently produce a frame whose declared length is smaller
/// than the appended bytes, corrupting the next frame on the wire.
pub fn encode_into(out: &mut Vec<u8>, id: u8, extended: bool, payload: &[u8]) {
    debug_assert!(id < 0x80, "blob id must fit in 7 bits");
    let total_len = HDR_LEN
        .checked_add(payload.len())
        .filter(|n| *n <= LEN_MASK as usize)
        .expect("blob payload exceeds 24-bit length field");
    let mut id_len = ((id as u32) << ID_SHIFT) | (total_len as u32 & LEN_MASK);
    if extended {
        id_len |= EXT_FLAG;
    }
    out.extend_from_slice(&id_len.to_be_bytes());
    out.extend_from_slice(payload);
    let padded = align4(total_len);
    out.resize(out.len() + (padded - total_len), 0);
}

/// `total_len` is the *unpadded* declared length; iterator advance is
/// `align4(total_len)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlobAttr<'a> {
    pub id: u8,
    pub extended: bool,
    pub payload: &'a [u8],
    pub total_len: u32,
}

impl<'a> BlobAttr<'a> {
    /// Returns the view and the aligned consumed byte count (header +
    /// payload + pad). `consumed` is capped at `buf.len()` so a final
    /// frame whose pad bytes were truncated mid-stream still parses.
    pub fn parse(buf: &'a [u8]) -> Result<(Self, usize), UbusError> {
        if buf.len() < HDR_LEN {
            return Err(UbusError::Short {
                need: HDR_LEN,
                have: buf.len(),
            });
        }
        let id_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let total_len = id_len & LEN_MASK;
        if total_len < HDR_LEN as u32 {
            return Err(UbusError::Malformed("attr total_len < header"));
        }
        if buf.len() < total_len as usize {
            return Err(UbusError::Short {
                need: total_len as usize,
                have: buf.len(),
            });
        }
        let id = ((id_len & ID_MASK) >> ID_SHIFT) as u8;
        let extended = (id_len & EXT_FLAG) != 0;
        let payload = &buf[HDR_LEN..total_len as usize];
        // Cap at buf.len(): writer may have truncated the final pad.
        let consumed = align4(total_len as usize).min(buf.len());
        Ok((
            Self {
                id,
                extended,
                payload,
                total_len,
            },
            consumed,
        ))
    }

    pub fn iter(buf: &'a [u8]) -> BareIter<'a> {
        BareIter { rest: buf }
    }
}

/// Stops at the first parse error — TLV chains can't be safely resumed
/// past corruption.
pub struct BareIter<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for BareIter<'a> {
    type Item = Result<BlobAttr<'a>, UbusError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        match BlobAttr::parse(self.rest) {
            Ok((attr, consumed)) => {
                self.rest = &self.rest[consumed..];
                Some(Ok(attr))
            }
            Err(e) => {
                self.rest = &[];
                Some(Err(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align4_rounds_up() {
        assert_eq!(align4(0), 0);
        assert_eq!(align4(1), 4);
        assert_eq!(align4(4), 4);
        assert_eq!(align4(5), 8);
        assert_eq!(align4(7), 8);
        assert_eq!(align4(8), 8);
    }

    #[test]
    fn encode_decode_round_trip() {
        let mut buf = Vec::new();
        encode_into(&mut buf, 7, false, b"hi");
        assert_eq!(&buf[..4], &[0x07, 0x00, 0x00, 0x06]);
        assert_eq!(&buf[4..6], b"hi");
        assert_eq!(&buf[6..8], &[0, 0]);
        let (attr, consumed) = BlobAttr::parse(&buf).unwrap();
        assert_eq!(attr.id, 7);
        assert!(!attr.extended);
        assert_eq!(attr.payload, b"hi");
        assert_eq!(attr.total_len, 6);
        assert_eq!(consumed, 8);
    }

    #[test]
    fn extended_flag_round_trips() {
        let mut buf = Vec::new();
        encode_into(&mut buf, 3, true, &[0xAA, 0xBB, 0xCC, 0xDD]);
        let (attr, _) = BlobAttr::parse(&buf).unwrap();
        assert!(attr.extended);
        assert_eq!(attr.id, 3);
        assert_eq!(attr.payload, &[0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn iter_walks_concatenated_attrs() {
        let mut buf = Vec::new();
        encode_into(&mut buf, 1, false, b"a");
        encode_into(&mut buf, 2, false, b"bb");
        encode_into(&mut buf, 3, false, b"ccc");
        let collected: Vec<_> = BlobAttr::iter(&buf).collect::<Result<_, _>>().unwrap();
        assert_eq!(collected.len(), 3);
        assert_eq!(collected[0].id, 1);
        assert_eq!(collected[0].payload, b"a");
        assert_eq!(collected[1].id, 2);
        assert_eq!(collected[1].payload, b"bb");
        assert_eq!(collected[2].id, 3);
        assert_eq!(collected[2].payload, b"ccc");
    }

    #[test]
    fn truncated_pad_still_parses() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0x09, 0x00, 0x00, 0x05]);
        buf.push(0xAB);
        let (attr, consumed) = BlobAttr::parse(&buf).unwrap();
        assert_eq!(attr.id, 9);
        assert_eq!(attr.payload, &[0xAB]);
        assert_eq!(consumed, buf.len());
    }
}
