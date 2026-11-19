//! A downloaded archive whose SHA-256 and size matched the catalog's pins.

use std::fs::{self, File};
use std::io::Seek;
use std::path::{Path, PathBuf};

use crate::catalog::schema::ArchiveFormat;
use crate::error::{Error, Result};

/// A downloaded archive that matched its pins. Only [`fetch`](super::fetch)
/// builds one, after the check, and `extract::unpack` takes nothing else.
///
/// It owns the open handle the bytes were written and hashed through.
/// Extraction reads through that handle and never opens the path again, so
/// the file cannot be swapped between the check and the unpack. The file is
/// removed when the value drops: the archive never outlives the install.
#[derive(Debug)]
pub struct VerifiedFile {
    // Declared before `part`, so the handle closes before the file is
    // removed, which Windows needs.
    pub(in crate::download) file: File,
    pub(in crate::download) part: PartFile,
    pub(in crate::download) format: ArchiveFormat,
    pub(in crate::download) size: u64,
}

impl VerifiedFile {
    pub fn format(&self) -> ArchiveFormat {
        self.format
    }

    /// The pinned size, which is the file's size.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The open handle, rewound to the start.
    pub fn reader(&mut self) -> Result<&mut File> {
        self.file
            .rewind()
            .map_err(|e| Error::io("rewind", self.part.path(), e))?;
        Ok(&mut self.file)
    }

    /// An existing archive with no download behind it, for extraction tests.
    /// It is opened read-only and removed on drop like any other.
    #[cfg(test)]
    pub(crate) fn for_test(path: &Path, format: ArchiveFormat) -> Self {
        let file = File::open(path).unwrap();
        let size = file.metadata().unwrap().len();
        Self {
            file,
            part: PartFile {
                path: path.to_path_buf(),
            },
            format,
            size,
        }
    }
}

/// A file in `downloads/` that is removed when this drops, so a failed or
/// finished download leaves nothing behind.
#[derive(Debug)]
pub struct PartFile {
    pub(in crate::download) path: PathBuf,
}

impl PartFile {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        // Best effort: a file left behind is removed by the next install of
        // the same version, under its lock.
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn the_handle_reads_from_the_start_and_the_file_goes_on_drop() {
        let root = TempDir::new("verified");
        let path = root.path().join("php-8.4.3.1.0.part");
        fs::write(&path, b"archive bytes").unwrap();

        let mut verified = VerifiedFile::for_test(&path, ArchiveFormat::TarGz);
        assert_eq!(
            (verified.format(), verified.size()),
            (ArchiveFormat::TarGz, 13)
        );
        for _ in 0..2 {
            let mut bytes = Vec::new();
            verified.reader().unwrap().read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"archive bytes");
        }

        drop(verified);
        assert!(!path.exists());
    }
}
