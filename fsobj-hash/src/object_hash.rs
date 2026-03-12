use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectHash(pub(crate) [u8; 32]);

impl ObjectHash {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            use fmt::Write as _;
            let _ = write!(&mut out, "{byte:02x}");
        }
        out
    }

    pub fn to_prefixed_hex(&self) -> String {
        format!("sha256:{}", self.to_hex())
    }
}

impl fmt::Display for ObjectHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("sha256:")?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ObjectHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ObjectHash")
            .field(&self.to_prefixed_hex())
            .finish()
    }
}

impl FromStr for ObjectHash {
    type Err = ParseObjectHashError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s
            .strip_prefix("sha256:")
            .ok_or(ParseObjectHashError::MissingPrefix)?;
        if hex.len() != 64 {
            return Err(ParseObjectHashError::InvalidLength);
        }
        if !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ParseObjectHashError::InvalidHex);
        }

        let mut bytes = [0u8; 32];
        for (idx, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
            let hi = decode_nibble(chunk[0]).ok_or(ParseObjectHashError::InvalidHex)?;
            let lo = decode_nibble(chunk[1]).ok_or(ParseObjectHashError::InvalidHex)?;
            bytes[idx] = (hi << 4) | lo;
        }
        Ok(Self(bytes))
    }
}

fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseObjectHashError {
    MissingPrefix,
    InvalidLength,
    InvalidHex,
}

impl fmt::Display for ParseObjectHashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPrefix => f.write_str("missing sha256: prefix"),
            Self::InvalidLength => f.write_str("hash must contain 64 lowercase hex digits"),
            Self::InvalidHex => f.write_str("hash must contain only lowercase hex digits"),
        }
    }
}

impl std::error::Error for ParseObjectHashError {}
