//! Byte ranges shared by repository and object readers, independent of HTTP headers.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ByteRange {
    #[default]
    Full,
    /// Internal preview window; an offset beyond EOF produces an empty window at EOF.
    Window { offset: u64, length: u64 },
    /// Repository blob preview from `offset`: the reader counts prefix lines and keeps the
    /// previous page boundary. Only a streaming blob reader can serve it.
    Preview { offset: u64, line: Option<u32> },
    Bytes { start: u64, end: Option<u64> },
    Suffix { length: u64 },
}

impl ByteRange {
    pub fn parse(value: &str) -> Result<Self, ()> {
        let (start, end) = value.strip_prefix("bytes=").and_then(|value| value.split_once('-')).ok_or(())?;
        let decimal = |value: &str| -> Result<u64, ()> {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) { return Err(()); }
            value.parse().map_err(|_| ())
        };
        if start.is_empty() {
            let length = decimal(end)?;
            if length == 0 { return Err(()); }
            Ok(Self::Suffix { length })
        } else {
            let start = decimal(start)?;
            let end = (!end.is_empty()).then(|| decimal(end)).transpose()?;
            if end.is_some_and(|end| end < start) { return Err(()); }
            Ok(Self::Bytes { start, end })
        }
    }

    /// Offset and length, after checking satisfiability against the immutable object's size.
    pub fn normalize(self, size: u64) -> Result<(u64, u64), ()> {
        match self {
            Self::Full => Ok((0, size)),
            Self::Preview { .. } => Err(()),
            Self::Window { offset, length } => {
                let offset = offset.min(size);
                Ok((offset, length.min(size - offset)))
            }
            Self::Bytes { start, end } => {
                if start >= size || end.is_some_and(|end| end < start) { return Err(()); }
                let end = end.unwrap_or(size - 1).min(size - 1);
                Ok((start, end - start + 1))
            }
            Self::Suffix { length } => {
                if size == 0 || length == 0 { return Err(()); }
                let length = length.min(size);
                Ok((size - length, length))
            }
        }
    }
}
