//! Parsing of the `.mrp` application package header.
//!
//! A `.mrp` file starts with a fixed 16-byte header (the `MRPG` magic plus
//! three little-endian `u32` fields) followed by an extended info block of
//! 184 bytes, for 200 bytes in total when present. Legacy packages from the
//! earliest MiniJ era only carry the fixed part. Text fields are GBK
//! encoded. The `ver` and `id` fields at the end of the info block are
//! big-endian (network order), matching the reference community tooling.
//!
//! The layout is reconstructed from public community documentation and
//! tooling; some fields of real packages are still to be verified against
//! samples. See `docs/Mrp-Format.md` for the field table.

use encoding_rs::GBK;

use crate::error::MrpError;

/// The four-byte signature every `.mrp` package starts with.
pub const MRP_MAGIC: [u8; 4] = *b"MRPG";

/// Size of the fixed part of the header: magic plus three little-endian
/// `u32` fields.
pub const FIXED_HEADER_SIZE: usize = 16;

/// Size of the full header when the extended info block is present.
pub const FULL_HEADER_SIZE: usize = 200;

/// Parsed `.mrp` package header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MrpHeader {
    /// Offset 4: size of the info block that follows the fixed header. Some
    /// community tools call this field `data_offset`.
    pub info_size: u32,
    /// Offset 8: total package size in bytes.
    pub file_size: u32,
    /// Offset 12: start of the package file list.
    pub list_offset: u32,

    /// Internal identifier of the package (offset 16, 12 bytes).
    pub internal_name: String,
    /// Display name shown on the phone (offset 28, 24 bytes).
    pub display_name: String,
    /// Device serial the package was issued for, often zeros (offset 52, 16 bytes).
    pub imei: String,
    /// Version number (offset 68).
    pub version: u32,
    /// Application identifier assigned by the platform (offset 72).
    pub app_id: u32,
    /// Author string (offset 88, 40 bytes).
    pub author: String,
    /// Description string (offset 128, 64 bytes).
    pub description: String,
    /// Build version (offset 192, big-endian).
    pub build_version: u32,
    /// Build identifier (offset 196, big-endian).
    pub build_id: u32,

    /// Whether the extended info block was present and parsed. Legacy
    /// packages only carry the fixed 16-byte header.
    pub has_extended_info: bool,
}

impl MrpHeader {
    /// Parses the header from the start of a `.mrp` package.
    ///
    /// Extended info fields are populated only when the buffer holds at
    /// least [`FULL_HEADER_SIZE`] bytes; otherwise they keep their defaults.
    pub fn parse(data: &[u8]) -> Result<Self, MrpError> {
        if data.len() < FIXED_HEADER_SIZE {
            return Err(MrpError::TooSmall {
                expected: FIXED_HEADER_SIZE,
                got: data.len(),
            });
        }

        let magic = [data[0], data[1], data[2], data[3]];
        if magic != MRP_MAGIC {
            return Err(MrpError::BadMagic { found: magic });
        }

        let info_size = read_u32_le(data, 4);
        let file_size = read_u32_le(data, 8);
        let list_offset = read_u32_le(data, 12);

        if file_size as usize > data.len() {
            return Err(MrpError::BadOffset {
                field: "file_size",
                value: file_size,
                size: data.len(),
            });
        }
        if list_offset < FIXED_HEADER_SIZE as u32 || list_offset as usize > data.len() {
            return Err(MrpError::BadOffset {
                field: "list_offset",
                value: list_offset,
                size: data.len(),
            });
        }

        let (
            internal_name,
            display_name,
            imei,
            version,
            app_id,
            author,
            description,
            build_version,
            build_id,
            has_extended_info,
        ) = if data.len() >= FULL_HEADER_SIZE {
            (
                gbk_lossy(&data[16..28]),
                gbk_lossy(&data[28..52]),
                gbk_lossy(&data[52..68]),
                read_u32_le(data, 68),
                read_u32_le(data, 72),
                gbk_lossy(&data[88..128]),
                gbk_lossy(&data[128..192]),
                read_u32_be(data, 192),
                read_u32_be(data, 196),
                true,
            )
        } else {
            (
                String::new(),
                String::new(),
                String::new(),
                0,
                0,
                String::new(),
                String::new(),
                0,
                0,
                false,
            )
        };

        Ok(Self {
            info_size,
            file_size,
            list_offset,
            internal_name,
            display_name,
            imei,
            version,
            app_id,
            author,
            description,
            build_version,
            build_id,
            has_extended_info,
        })
    }
}

fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

fn read_u32_be(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

/// Decodes a fixed-size GBK string field, dropping the NUL padding.
fn gbk_lossy(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let (decoded, _, _) = GBK.decode(&bytes[..end]);
    decoded.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal but complete extended header for testing.
    fn sample_header_bytes() -> Vec<u8> {
        let mut b = vec![0u8; FULL_HEADER_SIZE];
        b[0..4].copy_from_slice(&MRP_MAGIC);
        b[4..8].copy_from_slice(&200u32.to_le_bytes());
        b[8..12].copy_from_slice(&1234u32.to_le_bytes());
        b[12..16].copy_from_slice(&200u32.to_le_bytes());
        b[16..21].copy_from_slice(b"hello");
        b[28..38].copy_from_slice(&GBK.encode("俄罗斯方块").0);
        b[68..72].copy_from_slice(&7u32.to_le_bytes());
        b[72..76].copy_from_slice(&99u32.to_le_bytes());
        b[88..94].copy_from_slice(b"author");
        b[128..136].copy_from_slice(&GBK.encode("说明文字").0);
        b[192..196].copy_from_slice(&5u32.to_be_bytes());
        b[196..200].copy_from_slice(&6u32.to_be_bytes());
        b
    }

    #[test]
    fn parses_full_header() {
        let mut data = sample_header_bytes();
        data.extend_from_slice(&[0u8; 1034]); // payload behind the header

        let header = MrpHeader::parse(&data).expect("valid header must parse");

        assert!(header.has_extended_info);
        assert_eq!(header.info_size, 200);
        assert_eq!(header.file_size, 1234);
        assert_eq!(header.list_offset, 200);
        assert_eq!(header.internal_name, "hello");
        assert_eq!(header.display_name, "俄罗斯方块");
        assert_eq!(header.author, "author");
        assert_eq!(header.description, "说明文字");
        assert_eq!(header.version, 7);
        assert_eq!(header.app_id, 99);
        assert_eq!(header.build_version, 5);
        assert_eq!(header.build_id, 6);
    }

    #[test]
    fn parses_legacy_fixed_header_only() {
        let mut data = vec![0u8; FIXED_HEADER_SIZE];
        data[0..4].copy_from_slice(&MRP_MAGIC);
        data[4..8].copy_from_slice(&16u32.to_le_bytes());
        data[8..12].copy_from_slice(&64u32.to_le_bytes());
        data[12..16].copy_from_slice(&16u32.to_le_bytes());
        data.extend_from_slice(&[0u8; 48]);

        let header = MrpHeader::parse(&data).expect("legacy header must parse");

        assert!(!header.has_extended_info);
        assert_eq!(header.display_name, "");
        assert_eq!(header.file_size, 64);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut data = sample_header_bytes();
        data[0] = b'X';

        match MrpHeader::parse(&data) {
            Err(MrpError::BadMagic { found }) => assert_eq!(found, *b"XRPG"),
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn rejects_truncated_header() {
        let data = vec![0u8; FIXED_HEADER_SIZE - 1];

        match MrpHeader::parse(&data) {
            Err(MrpError::TooSmall { expected, got }) => {
                assert_eq!(expected, FIXED_HEADER_SIZE);
                assert_eq!(got, FIXED_HEADER_SIZE - 1);
            }
            other => panic!("expected TooSmall, got {other:?}"),
        }
    }

    #[test]
    fn rejects_file_size_overflow() {
        let mut data = sample_header_bytes();
        data[8..12].copy_from_slice(&9_999u32.to_le_bytes());

        match MrpHeader::parse(&data) {
            Err(MrpError::BadOffset { field, value, .. }) => {
                assert_eq!(field, "file_size");
                assert_eq!(value, 9_999);
            }
            other => panic!("expected BadOffset, got {other:?}"),
        }
    }
}
