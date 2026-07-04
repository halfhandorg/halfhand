//! Content-addressed, zstd-compressed blob store (SRS §4.1, FR-1.4, NFR-4).
//!
//! Blobs are keyed by BLAKE3 hash and stored at `blobs/<hash[0..2]>/<hash>.zst`.
//! The `blobs` table holds a refcount so that deleting a session can garbage-
//! collect blobs no longer referenced by any event. Files and directories are
//! created with `0600`/`0700` permissions per NFR-4.

use crate::error::{BlobError, Result, StorageError};
use std::fs;
use std::path::{Path, PathBuf};
use zstd::stream::{decode_all, encode_all};

/// Compression level for zstd. 3 is fast and compact; Halfhand stores file
/// snapshots and MCP payloads, not archives — level 3 is a good default.
const ZSTD_LEVEL: i32 = 3;

/// BLAKE3 hex length (64 chars).
const HASH_HEX_LEN: usize = 64;

/// A content-addressed blob store backed by a directory on disk and a
/// `blobs` table in SQLite for refcounting.
pub struct BlobStore {
    blobs_dir: PathBuf,
}

/// The outcome of storing a blob: the hash and the new refcount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutOutcome {
    /// BLAKE3 hex of the stored content.
    pub hash: String,
    /// The size of the uncompressed content in bytes.
    pub size: u64,
}

impl BlobStore {
    /// Create a new [`BlobStore`] rooted at `blobs_dir`. The directory itself
    /// is created lazily on first write with `0700` permissions.
    pub fn new(blobs_dir: PathBuf) -> Self {
        Self { blobs_dir }
    }

    /// Return the on-disk path for a given hash.
    pub(crate) fn blob_path(&self, hash: &str) -> PathBuf {
        let prefix = &hash[..2];
        self.blobs_dir.join(prefix).join(format!("{hash}.zst"))
    }

    /// Compute the BLAKE3 hex hash of `content` without storing it (FR-1.4:
    /// binary files record hashes even when content storage is skipped).
    /// Centralized here so the recorder does not depend on `blake3` directly.
    #[must_use]
    pub fn hash(content: &[u8]) -> String {
        blake3::hash(content).to_hex().to_string()
    }

    /// Store `content`, compressed with zstd, keyed by its BLAKE3 hash. Returns
    /// the hash. If the blob already exists on disk, content is not rewritten
    /// (content-addressing makes it identical). The `blobs` table refcount is
    /// bumped by the store's writer when an event references this hash.
    pub fn put(&self, content: &[u8]) -> Result<PutOutcome> {
        let hash = blake3::hash(content).to_hex().to_string();
        let size = u64::try_from(content.len()).unwrap_or(u64::MAX);
        let path = self.blob_path(&hash);
        if !path.exists() {
            Self::write_blob(&path, content)?;
        }
        Ok(PutOutcome { hash, size })
    }

    /// Read a blob by hash, decompress it, and return the bytes.
    pub fn get(&self, hash: &str) -> Result<Vec<u8>> {
        if hash.len() != HASH_HEX_LEN {
            return Err(BlobError::HashMismatch {
                expected: format!("{HASH_HEX_LEN}-char blake3 hex"),
                actual: hash.to_string(),
            }
            .into());
        }
        let path = self.blob_path(hash);
        let compressed = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::MissingBlob(hash.to_string()).into());
            }
            Err(e) => {
                return Err(BlobError::Io { path, source: e }.into());
            }
        };
        let decompressed =
            decode_all(&compressed[..]).map_err(|e| BlobError::Zstd(e.to_string()))?;
        // Verify content hash to detect corruption.
        let actual = blake3::hash(&decompressed).to_hex().to_string();
        if actual != hash {
            return Err(BlobError::HashMismatch {
                expected: hash.to_string(),
                actual,
            }
            .into());
        }
        Ok(decompressed)
    }

    fn write_blob(path: &Path, content: &[u8]) -> Result<()> {
        let parent = path.parent().ok_or_else(|| BlobError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "blob path has no parent",
            ),
        })?;
        // 0700 on the shard directory (NFR-4).
        create_dir_secure(parent)?;
        // Compress to a buffer, then write atomically with 0600 perms (NFR-4).
        let compressed =
            encode_all(content, ZSTD_LEVEL).map_err(|e| BlobError::Zstd(e.to_string()))?;
        write_file_secure(path, &compressed)
    }

    /// Delete a blob from disk if its refcount has dropped to zero. Returns
    /// `true` if a file was removed. The caller must have already decremented
    /// the refcount in the DB; this is the GC step.
    pub fn remove_if_unreferenced(&self, hash: &str, refcount: i64) -> Result<bool> {
        if refcount > 0 {
            return Ok(false);
        }
        let path = self.blob_path(hash);
        match fs::remove_file(&path) {
            Ok(()) => {
                // Best-effort cleanup of the now-empty shard dir.
                if let Some(shard) = path.parent() {
                    let _ = fs::remove_dir(shard);
                }
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(BlobError::Io { path, source: e }.into()),
        }
    }

    /// The root directory of the store.
    pub fn root(&self) -> &Path {
        &self.blobs_dir
    }
}

/// Create a directory with `0700` permissions (NFR-4). On Unix the mode is set
/// explicitly; on non-Unix the default umask applies (best-effort, SRS §2.2).
fn create_dir_secure(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|e| BlobError::Io {
                path: path.to_path_buf(),
                source: e,
            })?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path).map_err(|e| BlobError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
    }
    Ok(())
}

/// Write `bytes` to `path` with `0600` permissions (NFR-4), using a temp file +
/// rename for atomicity. Truncates any existing file.
fn write_file_secure(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("zst.tmp");
    {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| BlobError::Io {
                    path: tmp.clone(),
                    source: e,
                })?;
            f.write_all(bytes).map_err(|e| BlobError::Io {
                path: tmp.clone(),
                source: e,
            })?;
            f.sync_all().map_err(|e| BlobError::Io {
                path: tmp.clone(),
                source: e,
            })?;
        }
        #[cfg(not(unix))]
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp).map_err(|e| BlobError::Io {
                path: tmp.clone(),
                source: e,
            })?;
            f.write_all(bytes).map_err(|e| BlobError::Io {
                path: tmp.clone(),
                source: e,
            })?;
            f.sync_all().map_err(|e| BlobError::Io {
                path: tmp.clone(),
                source: e,
            })?;
        }
    }
    fs::rename(&tmp, path).map_err(|e| BlobError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    // Ensure the renamed file keeps 0600 even if it replaced an existing one.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, BlobStore) {
        let tmp = TempDir::new().unwrap();
        let s = BlobStore::new(tmp.path().join("blobs"));
        (tmp, s)
    }

    #[test]
    fn put_get_roundtrip_small() {
        let (_tmp, s) = store();
        let content = b"hello halfhand blobs";
        let out = s.put(content).unwrap();
        assert_eq!(out.size, content.len() as u64);
        // On-disk layout: blobs/<h2>/<hash>.zst
        let p = s.blob_path(&out.hash);
        assert!(p.exists());
        assert!(p.starts_with(s.root()));
        assert!(p.parent().unwrap().file_name().unwrap().len() == 2);
        let got = s.get(&out.hash).unwrap();
        assert_eq!(got, content);
    }

    #[test]
    fn put_get_roundtrip_compressible() {
        let (_tmp, s) = store();
        // Highly compressible content to exercise zstd.
        let content = "a".repeat(64 * 1024).into_bytes();
        let out = s.put(&content).unwrap();
        let on_disk = fs::read(s.blob_path(&out.hash)).unwrap();
        assert!(
            on_disk.len() < content.len(),
            "zstd should compress repetitive input"
        );
        let got = s.get(&out.hash).unwrap();
        assert_eq!(got, content);
    }

    #[test]
    fn put_is_idempotent_on_disk() {
        let (_tmp, s) = store();
        let out1 = s.put(b"same content").unwrap();
        let out2 = s.put(b"same content").unwrap();
        assert_eq!(out1.hash, out2.hash);
        // Only one file on disk.
        assert_eq!(fs::read_dir(s.root()).unwrap().count(), 1);
    }

    #[test]
    fn get_missing_blob_errors() {
        let (_tmp, s) = store();
        let h = "a".repeat(64);
        let err = s.get(&h).unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Storage(StorageError::MissingBlob(_))
        ));
    }

    #[test]
    fn get_detects_corruption() {
        let (_tmp, s) = store();
        let out = s.put(b"original content").unwrap();
        let path = s.blob_path(&out.hash);
        // Overwrite the compressed file with valid zstd of different content.
        let bad = zstd::stream::encode_all(b"different content".as_ref(), 3).unwrap();
        fs::write(&path, &bad).unwrap();
        let err = s.get(&out.hash).unwrap_err();
        assert!(matches!(
            err,
            crate::Error::Blob(BlobError::HashMismatch { .. })
        ));
    }

    #[test]
    fn remove_deletes_when_unreferenced() {
        let (_tmp, s) = store();
        let out = s.put(b"to be deleted").unwrap();
        let path = s.blob_path(&out.hash);
        assert!(path.exists());
        assert!(s.remove_if_unreferenced(&out.hash, 0).unwrap());
        assert!(!path.exists());
        // Second call is a no-op (already gone).
        assert!(!s.remove_if_unreferenced(&out.hash, 0).unwrap());
    }

    #[test]
    fn remove_keeps_when_still_referenced() {
        let (_tmp, s) = store();
        let out = s.put(b"still referenced").unwrap();
        let path = s.blob_path(&out.hash);
        assert!(!s.remove_if_unreferenced(&out.hash, 1).unwrap());
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn blob_files_are_0600_and_dirs_0700() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, s) = store();
        let out = s.put(b"secret bytes").unwrap();
        let path = s.blob_path(&out.hash);
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let shard = path.parent().unwrap();
        let dmode = fs::metadata(shard).unwrap().permissions().mode();
        assert_eq!(dmode & 0o777, 0o700);
    }
}
