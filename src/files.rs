//! Content-addressed file storage on the volume.
//!
//! A file lives at `<root>/<first two hex digits>/<sha256>`. Callers name a
//! file only by a validated sha256, so no caller supplied string ever becomes
//! part of a path.

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use sha2::{Digest, Sha256};
use tokio::{fs, io::AsyncWriteExt};

/// A lowercase hex sha256, checked on construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sha256Hex(String);

impl Sha256Hex {
    pub fn parse(value: &str) -> Option<Self> {
        let valid = value.len() == 64 && value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        valid.then(|| Self(value.to_owned()))
    }

    pub fn of(bytes: &[u8]) -> Self {
        Self(hex::encode(Sha256::digest(bytes)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha256Hex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

pub struct FileStore {
    root: PathBuf,
    temp_counter: AtomicU64,
}

impl FileStore {
    pub async fn open(root: &Path) -> std::io::Result<Self> {
        fs::create_dir_all(root.join("tmp")).await?;
        Ok(Self {
            root: root.to_owned(),
            temp_counter: AtomicU64::new(0),
        })
    }

    fn path(&self, sha: &Sha256Hex) -> PathBuf {
        self.root.join(&sha.0[..2]).join(&sha.0)
    }

    /// Stores the bytes and returns their hash. Writing to a temporary file
    /// and renaming it means a reader never sees a partial file, and storing
    /// the same content twice is harmless.
    pub async fn put(&self, bytes: &[u8]) -> std::io::Result<Sha256Hex> {
        let sha = Sha256Hex::of(bytes);
        let path = self.path(&sha);
        if fs::try_exists(&path).await? {
            return Ok(sha);
        }
        let n = self.temp_counter.fetch_add(1, Ordering::Relaxed);
        let temp = self
            .root
            .join("tmp")
            .join(format!("{}-{}-{n}", sha, std::process::id()));
        let mut file = fs::File::create(&temp).await?;
        file.write_all(bytes).await?;
        file.sync_all().await?;
        drop(file);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).await?;
        }
        fs::rename(&temp, &path).await?;
        Ok(sha)
    }

    pub async fn get(&self, sha: &Sha256Hex) -> std::io::Result<Vec<u8>> {
        fs::read(self.path(sha)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_lowercase_hex_of_the_right_length_parses() {
        let good = "a".repeat(64);
        assert!(Sha256Hex::parse(&good).is_some());
        assert!(Sha256Hex::parse(&"A".repeat(64)).is_none());
        assert!(Sha256Hex::parse(&"a".repeat(63)).is_none());
        assert!(Sha256Hex::parse(&format!("../{}", "a".repeat(61))).is_none());
    }

    #[tokio::test]
    async fn stored_bytes_read_back_by_their_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileStore::open(dir.path()).await.expect("store");
        let sha = store.put(b"hello").await.expect("put");
        assert_eq!(
            sha.as_str(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(store.get(&sha).await.expect("get"), b"hello");
        assert_eq!(store.put(b"hello").await.expect("second put"), sha);
    }
}
