//! `.mrp` package file list and entry extraction.
//!
//! After the 200-byte header sits the package file list: a sequence of
//! entries (`u32` name length, NUL-padded name bytes, `u32` data offset,
//! `u32` data size, 4 reserved bytes) followed by the file data. The list
//! region ends at `header.info_size + 8`, the heuristic used by community
//! tooling; entry parsing stops there.

use crate::error::MrpError;
use crate::format::MrpHeader;

/// One file stored inside a `.mrp` package.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PackageEntry {
    /// Entry name as stored in the package (e.g. `start.mr`).
    pub name: String,
    /// Absolute byte offset of the file data inside the package.
    pub offset: u32,
    /// Stored byte size of the file data.
    pub size: u32,
}

/// A parsed `.mrp` package with random access to its stored files.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MrpPackage {
    header: MrpHeader,
    entries: Vec<PackageEntry>,
    data: Vec<u8>,
}

impl MrpPackage {
    /// Parses the header and the package file list.
    pub fn parse(data: Vec<u8>) -> Result<Self, MrpError> {
        let header = MrpHeader::parse(&data)?;
        let entries = Self::parse_entries(&data, header.list_offset, header.info_size)?;
        Ok(Self {
            header,
            entries,
            data,
        })
    }

    /// Convenience loader that reads the package from `path` first.
    pub fn from_path(path: impl AsRef<std::path::Path>) -> Result<Self, MrpError> {
        let data = std::fs::read(path.as_ref())?;
        Self::parse(data)
    }

    /// The raw package bytes (magic + header + entries + data).
    pub fn raw_data(&self) -> &[u8] {
        &self.data
    }

    pub fn header(&self) -> &MrpHeader {
        &self.header
    }

    /// All files listed in the package, in list order.
    pub fn entries(&self) -> &[PackageEntry] {
        &self.entries
    }

    /// Raw (still gzip-compressed when stored so) contents of `name`.
    pub fn read_entry_raw(&self, name: &str) -> Result<Vec<u8>, MrpError> {
        let entry =
            self.entries
                .iter()
                .find(|entry| entry.name == name)
                .ok_or(MrpError::Unsupported(format!(
                    "package file not found: {name}"
                )))?;
        let start = entry.offset as usize;
        let end = start
            .checked_add(entry.size as usize)
            .ok_or(MrpError::BadOffset {
                field: "entry end",
                value: entry.offset,
                size: self.data.len(),
            })?;
        if end > self.data.len() {
            return Err(MrpError::BadOffset {
                field: "entry end",
                value: entry.offset,
                size: self.data.len(),
            });
        }
        Ok(self.data[start..end].to_vec())
    }

    /// Contents of `name` with gzip decompression applied when the stored
    /// data carries the `1f 8b` gzip signature. This is how `start.mr`
    /// bytecode and other compressed members are stored.
    pub fn read_entry(&self, name: &str) -> Result<Vec<u8>, MrpError> {
        use std::io::Read;
        let raw = self.read_entry_raw(name)?;
        if raw.len() >= 2 && raw[0] == 0x1F && raw[1] == 0x8B {
            let mut decoded = Vec::new();
            let mut decoder = flate2::read::GzDecoder::new(&raw[..]);
            decoder.read_to_end(&mut decoded)?;
            Ok(decoded)
        } else {
            Ok(raw)
        }
    }

    /// The package's start script file (`start.mr` when present).
    pub fn start_script(&self) -> Result<Vec<u8>, MrpError> {
        self.read_entry("start.mr")
    }

    fn parse_entries(
        data: &[u8],
        list_offset: u32,
        info_size: u32,
    ) -> Result<Vec<PackageEntry>, MrpError> {
        let mut entries = Vec::new();
        let mut pos = list_offset as usize;
        // Community loaders stop once the cursor passes the end-of-list
        // marker; mirror that so malformed padding never becomes entries.
        let stop = info_size.saturating_add(8) as usize;
        while pos < stop && pos + 4 <= data.len() {
            let name_len =
                u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                    as usize;
            pos += 4;
            if name_len == 0 || name_len > 512 || pos + name_len > data.len() {
                break;
            }
            let name = String::from_utf8_lossy(&data[pos..pos + name_len])
                .trim_end_matches('\0')
                .to_string();
            pos += name_len;
            if pos + 12 > data.len() {
                break;
            }
            let offset =
                u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
            let size =
                u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);
            pos += 12;
            if name.is_empty() {
                break;
            }
            entries.push(PackageEntry { name, offset, size });
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::FULL_HEADER_SIZE;

    /// Builds a synthetic package: header + two entries + file data.
    fn sample_package() -> Vec<u8> {
        let mut data = vec![0u8; FULL_HEADER_SIZE];
        data[0..4].copy_from_slice(&crate::format::MRP_MAGIC);
        // info_size doubles as the list-end marker (list ends at 288 here).
        data[4..8].copy_from_slice(&288u32.to_le_bytes());
        data[8..12].copy_from_slice(&0u32.to_le_bytes());
        data[12..16].copy_from_slice(&240u32.to_le_bytes()); // list_offset

        let mut list = Vec::new();
        for (name, offset, size) in [("start.mr", 300u32, 10u32), ("logo.bmp", 310u32, 4u32)] {
            list.extend_from_slice(&(name.len() as u32 + 1).to_le_bytes());
            list.extend_from_slice(name.as_bytes());
            list.push(0);
            list.extend_from_slice(&offset.to_le_bytes());
            list.extend_from_slice(&size.to_le_bytes());
            list.extend_from_slice(&[0, 0, 0, 0]);
        }
        data.truncate(FULL_HEADER_SIZE);
        while data.len() < 240 {
            data.push(0);
        }
        data.extend_from_slice(&list);
        while data.len() < 300 {
            data.push(0);
        }
        data.extend_from_slice(b"1234567890"); // start.mr payload
        data.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // logo.bmp payload
        data
    }

    #[test]
    fn parses_entries_and_reads_data() {
        let package = MrpPackage::parse(sample_package()).expect("package must parse");

        assert_eq!(package.entries().len(), 2);
        assert_eq!(package.entries()[0].name, "start.mr");
        assert_eq!(package.entries()[0].offset, 300);
        assert_eq!(package.entries()[0].size, 10);

        let payload = package
            .read_entry_raw("start.mr")
            .expect("entry must exist");
        assert_eq!(payload, b"1234567890");
        assert!(matches!(
            package.read_entry_raw("missing"),
            Err(MrpError::Unsupported(_))
        ));
    }

    #[test]
    fn start_script_returns_the_start_member() {
        let package = MrpPackage::parse(sample_package()).expect("package must parse");
        assert_eq!(
            package.start_script().expect("start.mr must exist"),
            b"1234567890"
        );
    }
}
