//! Allocation-free encoders over caller-owned storage.
//!
//! Capacity, invalid strings and container depth are reported as errors. A
//! failed append leaves the preceding bytes intact, so callers can retry with
//! a smaller value. These encoders do not log, lock, call user code or allocate.

use crate::{
    blob::{align4, EXT_FLAG, LEN_MASK},
    blobmsg::BlobType,
    envelope::UbusAttr,
    wire::MAX_MSG_LEN,
    BufferError,
};

struct Writer<'a> {
    bytes: &'a mut [u8],
    len: usize,
}

impl Writer<'_> {
    fn append(
        &mut self,
        id: u8,
        name: Option<&str>,
        extended: bool,
        parts: &[&[u8]],
    ) -> Result<usize, BufferError> {
        if id >= 128 {
            return Err(BufferError::InvalidId);
        }
        let name = name.unwrap_or("");
        if extended && (name.len() > u16::MAX as usize || name.as_bytes().contains(&0)) {
            return Err(if name.len() > u16::MAX as usize {
                BufferError::Length
            } else {
                BufferError::EmbeddedNul
            });
        }
        let name_size = if extended {
            align4(2 + name.len() + 1)
        } else {
            0
        };
        let length = parts.iter().try_fold(4 + name_size, |size, part| {
            size.checked_add(part.len()).ok_or(BufferError::Length)
        })?;
        if length > LEN_MASK as usize || length > MAX_MSG_LEN {
            return Err(BufferError::Length);
        }
        let end = self
            .len
            .checked_add(align4(length))
            .ok_or(BufferError::Length)?;
        if end > MAX_MSG_LEN {
            return Err(BufferError::Length);
        }
        if end > self.bytes.len() {
            return Err(BufferError::Capacity {
                needed: end,
                available: self.bytes.len(),
            });
        }
        let start = self.len;
        let tag = ((id as u32) << 24) | if extended { EXT_FLAG } else { 0 } | length as u32;
        self.bytes[start..start + 4].copy_from_slice(&tag.to_be_bytes());
        let mut cursor = start + 4;
        if extended {
            self.bytes[cursor..cursor + name_size].fill(0);
            self.bytes[cursor..cursor + 2].copy_from_slice(&(name.len() as u16).to_be_bytes());
            self.bytes[cursor + 2..cursor + 2 + name.len()].copy_from_slice(name.as_bytes());
            cursor += name_size;
        }
        for part in parts {
            self.bytes[cursor..cursor + part.len()].copy_from_slice(part);
            cursor += part.len();
        }
        self.bytes[cursor..end].fill(0);
        self.len = end;
        Ok(start)
    }
}

/// An envelope with a four-byte outer attribute, stored in a caller's slice.
pub struct EnvSliceBuilder<'a> {
    writer: Writer<'a>,
}

impl<'a> EnvSliceBuilder<'a> {
    pub fn new(bytes: &'a mut [u8]) -> Result<Self, BufferError> {
        if bytes.len() < 4 {
            return Err(BufferError::Capacity {
                needed: 4,
                available: bytes.len(),
            });
        }
        bytes[..4].copy_from_slice(&4_u32.to_be_bytes());
        Ok(Self {
            writer: Writer { bytes, len: 4 },
        })
    }

    pub fn push_bytes_id(&mut self, id: u8, bytes: &[u8]) -> Result<(), BufferError> {
        self.writer.append(id, None, false, &[bytes]).map(|_| ())
    }

    pub fn push_u32(&mut self, id: UbusAttr, value: u32) -> Result<(), BufferError> {
        self.push_bytes_id(id as u8, &value.to_be_bytes())
    }

    pub fn push_i32(&mut self, id: UbusAttr, value: i32) -> Result<(), BufferError> {
        self.push_bytes_id(id as u8, &value.to_be_bytes())
    }

    pub fn push_u8(&mut self, id: UbusAttr, value: u8) -> Result<(), BufferError> {
        self.push_bytes_id(id as u8, &[value])
    }

    pub fn push_str(&mut self, id: UbusAttr, value: &str) -> Result<(), BufferError> {
        if value.as_bytes().contains(&0) {
            return Err(BufferError::EmbeddedNul);
        }
        self.writer
            .append(id as u8, None, false, &[value.as_bytes(), &[0]])
            .map(|_| ())
    }

    /// Embed blobmsg table contents, as with `EnvBuilder::push_data`.
    pub fn push_data(&mut self, contents: &[u8]) -> Result<(), BufferError> {
        self.push_bytes_id(UbusAttr::Data as u8, contents)
    }

    pub fn finish(self) -> &'a [u8] {
        self.writer.bytes[..4].copy_from_slice(&(self.writer.len as u32).to_be_bytes());
        &self.writer.bytes[..self.writer.len]
    }
}

/// Named blobmsg attributes with a fixed maximum container depth.
///
/// `finish` returns the attribute stream. Open and close an anonymous table
/// around it when a complete outer TABLE is needed, or embed the stream
/// directly with [`EnvSliceBuilder::push_data`].
pub struct BlobSliceBuilder<'a, const DEPTH: usize = 8> {
    writer: Writer<'a>,
    containers: [usize; DEPTH],
    depth: usize,
}

impl<'a, const DEPTH: usize> BlobSliceBuilder<'a, DEPTH> {
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self {
            writer: Writer { bytes, len: 0 },
            containers: [0; DEPTH],
            depth: 0,
        }
    }

    pub fn put_str(&mut self, name: Option<&str>, value: &str) -> Result<(), BufferError> {
        if value.as_bytes().contains(&0) {
            return Err(BufferError::EmbeddedNul);
        }
        self.writer
            .append(
                BlobType::String as u8,
                name,
                true,
                &[value.as_bytes(), &[0]],
            )
            .map(|_| ())
    }

    pub fn put_i32(&mut self, name: Option<&str>, value: i32) -> Result<(), BufferError> {
        self.put_raw(BlobType::Int32, name, &value.to_be_bytes())
    }

    pub fn put_i64(&mut self, name: Option<&str>, value: i64) -> Result<(), BufferError> {
        self.put_raw(BlobType::Int64, name, &value.to_be_bytes())
    }

    pub fn put_bool(&mut self, name: Option<&str>, value: bool) -> Result<(), BufferError> {
        self.put_raw(BlobType::Int8, name, &[u8::from(value)])
    }

    pub fn put_double(&mut self, name: Option<&str>, value: f64) -> Result<(), BufferError> {
        self.put_raw(BlobType::Double, name, &value.to_bits().to_be_bytes())
    }

    pub fn put_raw(
        &mut self,
        ty: BlobType,
        name: Option<&str>,
        bytes: &[u8],
    ) -> Result<(), BufferError> {
        self.writer
            .append(ty as u8, name, true, &[bytes])
            .map(|_| ())
    }

    pub fn open_table(&mut self, name: Option<&str>) -> Result<(), BufferError> {
        self.open(BlobType::Table, name)
    }

    pub fn open_array(&mut self, name: Option<&str>) -> Result<(), BufferError> {
        self.open(BlobType::Array, name)
    }

    fn open(&mut self, ty: BlobType, name: Option<&str>) -> Result<(), BufferError> {
        if self.depth == DEPTH {
            return Err(BufferError::NestingLimit);
        }
        let start = self.writer.append(ty as u8, name, true, &[])?;
        self.containers[self.depth] = start;
        self.depth += 1;
        Ok(())
    }

    pub fn close(&mut self) -> Result<(), BufferError> {
        if self.depth == 0 {
            return Err(BufferError::Unbalanced);
        }
        let start = self.containers[self.depth - 1];
        let tag = u32::from_be_bytes(
            self.writer.bytes[start..start + 4]
                .try_into()
                .map_err(|_| BufferError::Unbalanced)?,
        ) & !LEN_MASK;
        self.writer.bytes[start..start + 4]
            .copy_from_slice(&(tag | (self.writer.len - start) as u32).to_be_bytes());
        self.depth -= 1;
        Ok(())
    }

    pub fn finish(self) -> Result<&'a [u8], BufferError> {
        if self.depth != 0 {
            return Err(BufferError::Unbalanced);
        }
        Ok(&self.writer.bytes[..self.writer.len])
    }
}
