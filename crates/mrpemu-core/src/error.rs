use std::io;

/// Errors produced while loading or running MRP content.
#[derive(Debug, thiserror::Error)]
pub enum MrpError {
    /// The file could not be read from disk.
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// The content is shorter than the smallest valid MRP header.
    #[error("file too small: got {got} bytes, need at least {expected}")]
    TooSmall { expected: usize, got: usize },

    /// The content does not start with the `MRPG` signature.
    #[error("invalid MRP magic: expected \"MRPG\", found {found:02x?}")]
    BadMagic { found: [u8; 4] },

    /// A header field points outside the file.
    #[error("invalid header field {field}: {value} is out of range for a {size}-byte file")]
    BadOffset {
        field: &'static str,
        value: u32,
        size: usize,
    },

    /// The content uses a feature the emulator does not support yet.
    #[error("unsupported: {0}")]
    Unsupported(String),
}
