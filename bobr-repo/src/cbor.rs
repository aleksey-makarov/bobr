//! Small deterministic-CBOR codec for repository-owned structures.

use crate::RepositoryError;

pub(crate) struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    pub(crate) fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    pub(crate) fn array(&mut self, length: u64) {
        self.head(4, length);
    }

    pub(crate) fn map(&mut self, length: u64) {
        self.head(5, length);
    }

    pub(crate) fn uint(&mut self, value: u64) {
        self.head(0, value);
    }

    pub(crate) fn bytes(&mut self, value: &[u8]) {
        self.head(2, value.len() as u64);
        self.bytes.extend_from_slice(value);
    }

    pub(crate) fn text(&mut self, value: &str) {
        self.head(3, value.len() as u64);
        self.bytes.extend_from_slice(value.as_bytes());
    }

    pub(crate) fn null(&mut self) {
        self.bytes.push(0xf6);
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn head(&mut self, major: u8, value: u64) {
        let prefix = major << 5;
        match value {
            0..=23 => self.bytes.push(prefix | value as u8),
            24..=0xff => self.bytes.extend_from_slice(&[prefix | 24, value as u8]),
            0x100..=0xffff => {
                self.bytes.push(prefix | 25);
                self.bytes.extend_from_slice(&(value as u16).to_be_bytes());
            }
            0x1_0000..=0xffff_ffff => {
                self.bytes.push(prefix | 26);
                self.bytes.extend_from_slice(&(value as u32).to_be_bytes());
            }
            _ => {
                self.bytes.push(prefix | 27);
                self.bytes.extend_from_slice(&value.to_be_bytes());
            }
        }
    }
}

pub(crate) struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    pub(crate) fn array(&mut self) -> Result<u64, RepositoryError> {
        self.head(4, "array")
    }

    pub(crate) fn map(&mut self) -> Result<u64, RepositoryError> {
        self.head(5, "map")
    }

    pub(crate) fn uint(&mut self) -> Result<u64, RepositoryError> {
        self.head(0, "unsigned integer")
    }

    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], RepositoryError> {
        let length = self.head(2, "byte string")?;
        self.take(length)
    }

    pub(crate) fn text(&mut self) -> Result<&'a str, RepositoryError> {
        let length = self.head(3, "text string")?;
        std::str::from_utf8(self.take(length)?)
            .map_err(|_| RepositoryError::new("CBOR text string is not valid UTF-8"))
    }

    pub(crate) fn null_or_uint(&mut self) -> Result<Option<u64>, RepositoryError> {
        if self.peek()? == 0xf6 {
            self.offset += 1;
            Ok(None)
        } else {
            self.uint().map(Some)
        }
    }

    pub(crate) fn finish(self) -> Result<(), RepositoryError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(RepositoryError::new("trailing bytes after CBOR value"))
        }
    }

    fn head(&mut self, expected_major: u8, expected: &str) -> Result<u64, RepositoryError> {
        let initial = self.byte()?;
        let major = initial >> 5;
        let additional = initial & 0x1f;
        if major != expected_major {
            return Err(RepositoryError::new(format!("expected a CBOR {expected}")));
        }
        let value = match additional {
            0..=23 => additional as u64,
            24 => {
                let value = self.byte()? as u64;
                if value < 24 {
                    return Err(non_shortest());
                }
                value
            }
            25 => {
                let value = u16::from_be_bytes(self.take_array()?) as u64;
                if value <= 0xff {
                    return Err(non_shortest());
                }
                value
            }
            26 => {
                let value = u32::from_be_bytes(self.take_array()?) as u64;
                if value <= 0xffff {
                    return Err(non_shortest());
                }
                value
            }
            27 => {
                let value = u64::from_be_bytes(self.take_array()?);
                if value <= 0xffff_ffff {
                    return Err(non_shortest());
                }
                value
            }
            31 => return Err(RepositoryError::new("indefinite-length CBOR is forbidden")),
            _ => return Err(RepositoryError::new("reserved CBOR additional information")),
        };
        Ok(value)
    }

    fn peek(&self) -> Result<u8, RepositoryError> {
        self.bytes
            .get(self.offset)
            .copied()
            .ok_or_else(|| RepositoryError::new("truncated CBOR value"))
    }

    fn byte(&mut self) -> Result<u8, RepositoryError> {
        let byte = self.peek()?;
        self.offset += 1;
        Ok(byte)
    }

    fn take(&mut self, length: u64) -> Result<&'a [u8], RepositoryError> {
        let length = usize::try_from(length).map_err(|_| {
            RepositoryError::new("CBOR length does not fit in memory address space")
        })?;
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| RepositoryError::new("CBOR length overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| RepositoryError::new("truncated CBOR value"))?;
        self.offset = end;
        Ok(value)
    }

    fn take_array<const N: usize>(&mut self) -> Result<[u8; N], RepositoryError> {
        self.take(N as u64)?
            .try_into()
            .map_err(|_| RepositoryError::new("truncated CBOR integer"))
    }
}

fn non_shortest() -> RepositoryError {
    RepositoryError::new("CBOR integer or length is not shortest-encoded")
}
