//! In-memory virtual file system for guest content.
//!
//! Mythroad applications read their resources and persistent data from the
//! phone's storage card (`mythroad/...`). The core models that tree with
//! plain case-sensitive path strings stored in memory; persistence to the
//! host is a later concern and must stay behind this type.

use std::collections::BTreeMap;

/// A memory-backed file tree keyed by guest path (for example
/// `mythroad/game.dat`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryVfs {
    files: BTreeMap<String, Vec<u8>>,
}

impl MemoryVfs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Writes a file, returning the previous contents if it existed.
    pub fn write(&mut self, path: impl Into<String>, data: impl Into<Vec<u8>>) -> Option<Vec<u8>> {
        self.files.insert(path.into(), data.into())
    }

    /// Reads a file, or `None` when it does not exist.
    pub fn read(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(Vec::as_slice)
    }

    /// Removes a file, returning its contents if it existed.
    pub fn remove(&mut self, path: &str) -> Option<Vec<u8>> {
        self.files.remove(path)
    }

    /// All stored paths in deterministic (sorted) order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// Number of stored files.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_read_roundtrip() {
        let mut vfs = MemoryVfs::new();

        assert!(vfs.is_empty());
        assert_eq!(vfs.write("mythroad/a.dat", [1u8, 2, 3]), None);
        assert_eq!(vfs.read("mythroad/a.dat"), Some(&[1u8, 2, 3][..]));
        assert_eq!(vfs.len(), 1);
    }

    #[test]
    fn overwrite_returns_previous_content() {
        let mut vfs = MemoryVfs::new();
        vfs.write("a.dat", [1u8]);

        assert_eq!(vfs.write("a.dat", [9u8, 9]), Some(vec![1u8]));
        assert_eq!(vfs.read("a.dat"), Some(&[9u8, 9][..]));
    }

    #[test]
    fn missing_reads_and_removals_are_none() {
        let mut vfs = MemoryVfs::new();
        vfs.write("a.dat", [1u8]);

        assert_eq!(vfs.read("b.dat"), None);
        assert_eq!(vfs.remove("b.dat"), None);
        assert_eq!(vfs.remove("a.dat"), Some(vec![1u8]));
        assert!(vfs.is_empty());
    }

    #[test]
    fn paths_are_sorted() {
        let mut vfs = MemoryVfs::new();
        vfs.write("b.dat", [2u8]);
        vfs.write("a.dat", [1u8]);

        assert_eq!(vfs.paths().collect::<Vec<_>>(), ["a.dat", "b.dat"]);
    }
}
