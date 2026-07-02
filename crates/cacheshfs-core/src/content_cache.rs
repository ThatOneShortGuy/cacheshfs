//! On-disk content cache: whole-file hydration for reads.
//!
//! On the first read of a file (in a content-caching mode) the whole file is
//! downloaded from the remote into `cache_dir` and subsequent reads are served
//! locally until the file changes or is written. Hydration is crash-safe: data
//! is streamed to a temporary file, flushed to disk, then atomically renamed
//! into place, so a partially written download is never mistaken for a complete
//! cache entry.
//!
//! Cache files are named by `NodeId` under `cache_dir/objects`. The hydration
//! index (which nodes are hydrated, and the size/mtime they were hydrated at) is
//! kept in memory, so caching is per-session; cross-restart persistence and
//! range-based caching are later refinements.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Error, FileAttributes, NodeId, RemoteFilesystem, RemotePath, Result};

/// Whole-file downloads are streamed in chunks of this size.
const CHUNK: u32 = 128 * 1024;

/// What a cached file was hydrated at, for revalidation.
struct Hydrated {
    size: u64,
    modified: Option<i64>,
}

/// On-disk whole-file content cache.
pub(crate) struct ContentCache {
    dir: PathBuf,
    temp_counter: AtomicU64,
    index: Mutex<HashMap<NodeId, Hydrated>>,
}

fn cache_io(error: std::io::Error) -> Error {
    Error::RemoteBackend(format!("content cache io error: {error}"))
}

impl ContentCache {
    pub(crate) fn new(cache_dir: PathBuf) -> Self {
        // Directories are created lazily on first hydration so constructing a
        // VFS never touches the filesystem.
        Self {
            dir: cache_dir,
            temp_counter: AtomicU64::new(0),
            index: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<NodeId, Hydrated>> {
        self.index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn objects_dir(&self) -> PathBuf {
        self.dir.join("objects")
    }

    fn object_path(&self, node: NodeId) -> PathBuf {
        self.objects_dir().join(node.0.to_string())
    }

    /// Read a range of `node`, hydrating the whole file from `remote` first if it
    /// is not cached or the cached copy is stale relative to `attrs`.
    pub(crate) fn read(
        &self,
        node: NodeId,
        path: &RemotePath,
        remote: &dyn RemoteFilesystem,
        attrs: &FileAttributes,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>> {
        let fresh = {
            let index = self.lock();
            index.get(&node).is_some_and(|hydrated| {
                hydrated.size == attrs.size && hydrated.modified == attrs.modified_unix_seconds
            })
        };
        if !fresh {
            self.hydrate(node, path, remote, attrs)?;
        }
        self.read_range(node, offset, size)
    }

    /// Read a range of `node` only if it is already hydrated; never contacts the
    /// remote. Used in offline mode.
    pub(crate) fn read_cached(&self, node: NodeId, offset: u64, size: u32) -> Result<Vec<u8>> {
        if !self.lock().contains_key(&node) {
            return Err(Error::Unavailable(
                "file contents are not cached and the mount is offline".to_string(),
            ));
        }
        self.read_range(node, offset, size)
    }

    /// Drop the cached content for `node` (e.g. after a write or unlink).
    pub(crate) fn invalidate(&self, node: NodeId) {
        if self.lock().remove(&node).is_some() {
            // Best-effort: the index entry is authoritative, so a failed removal
            // only leaves a harmless orphan that a later hydration overwrites.
            let _ = fs::remove_file(self.object_path(node));
        }
    }

    /// Download the whole file to a temp file, flush it, atomically rename it
    /// into place, and record the hydration.
    fn hydrate(
        &self,
        node: NodeId,
        path: &RemotePath,
        remote: &dyn RemoteFilesystem,
        attrs: &FileAttributes,
    ) -> Result<()> {
        let objects = self.objects_dir();
        let temp_dir = self.dir.join("tmp");
        fs::create_dir_all(&objects).map_err(cache_io)?;
        fs::create_dir_all(&temp_dir).map_err(cache_io)?;

        let temp = temp_dir.join(format!(
            "{}.{}",
            node.0,
            self.temp_counter.fetch_add(1, Ordering::Relaxed)
        ));

        // Stream the whole file into the temp file. Errors leave only the temp
        // file behind, never a half-written cache object.
        let result = (|| -> Result<()> {
            let mut file = File::create(&temp).map_err(cache_io)?;
            let mut offset = 0u64;
            while offset < attrs.size {
                let want = (attrs.size - offset).min(CHUNK as u64) as u32;
                let chunk = remote.read(path, offset, want)?;
                if chunk.is_empty() {
                    break;
                }
                file.write_all(&chunk).map_err(cache_io)?;
                offset += chunk.len() as u64;
            }
            file.sync_all().map_err(cache_io)?;
            Ok(())
        })();

        if let Err(error) = result {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }

        fs::rename(&temp, self.object_path(node)).map_err(cache_io)?;
        self.lock().insert(
            node,
            Hydrated {
                size: attrs.size,
                modified: attrs.modified_unix_seconds,
            },
        );
        Ok(())
    }

    fn read_range(&self, node: NodeId, offset: u64, size: u32) -> Result<Vec<u8>> {
        let mut file = File::open(self.object_path(node)).map_err(cache_io)?;
        file.seek(SeekFrom::Start(offset)).map_err(cache_io)?;

        let mut buffer = vec![0u8; size as usize];
        let mut filled = 0;
        while filled < buffer.len() {
            let read = file.read(&mut buffer[filled..]).map_err(cache_io)?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        buffer.truncate(filled);
        Ok(buffer)
    }
}

/// Whether `cache_dir` currently holds a cached object for `node` (test helper).
#[cfg(test)]
pub(crate) fn object_exists(cache_dir: &std::path::Path, node: NodeId) -> bool {
    cache_dir.join("objects").join(node.0.to_string()).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_cached_reports_unavailable_when_not_hydrated() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentCache::new(dir.path().to_path_buf());
        // Nothing hydrated: an offline read must fail cleanly, not touch disk.
        assert!(matches!(
            cache.read_cached(NodeId(1), 0, 16),
            Err(Error::Unavailable(_))
        ));
    }
}
