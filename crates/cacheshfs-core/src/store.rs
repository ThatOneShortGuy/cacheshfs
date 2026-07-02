//! Persistent, path-keyed cache store: metadata and whole-file content.
//!
//! Everything the cache knows about a remote path — its attributes, directory
//! listings, and a reference to hydrated content — lives in a single JSON index
//! (`cache_dir/index.json`) plus content objects under `cache_dir/objects`.
//! Because the index is keyed by the (stable) remote path and persisted, a mount
//! can serve previously cached files after a restart, including offline when the
//! server is unreachable.
//!
//! Writes are crash-safe: content is streamed to a temp file, flushed, then
//! atomically renamed into place, and the index is rewritten the same way.
//!
//! There is no local dirty state — writes are write-through — so on reconnect
//! the server is authoritative: revalidation replaces cached metadata and
//! re-hydrates content whenever the server's size/mtime differ.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::{Error, FileAttributes, FileKind, RemoteFilesystem, RemotePath, Result};

/// Whole-file downloads are streamed in chunks of this size.
const CHUNK: u32 = 128 * 1024;

fn cache_io(error: std::io::Error) -> Error {
    Error::RemoteBackend(format!("content cache io error: {error}"))
}

/// Serializable mirror of [`FileAttributes`], so serde stays out of the public
/// API types.
#[derive(Clone, Serialize, Deserialize)]
struct StoredAttrs {
    kind: u8,
    size: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    modified: Option<i64>,
    accessed: Option<i64>,
    changed: Option<i64>,
}

fn kind_to_u8(kind: FileKind) -> u8 {
    match kind {
        FileKind::File => 0,
        FileKind::Directory => 1,
        FileKind::Symlink => 2,
    }
}

fn u8_to_kind(value: u8) -> FileKind {
    match value {
        1 => FileKind::Directory,
        2 => FileKind::Symlink,
        _ => FileKind::File,
    }
}

impl From<&FileAttributes> for StoredAttrs {
    fn from(a: &FileAttributes) -> Self {
        StoredAttrs {
            kind: kind_to_u8(a.kind),
            size: a.size,
            mode: a.mode,
            uid: a.uid,
            gid: a.gid,
            modified: a.modified_unix_seconds,
            accessed: a.accessed_unix_seconds,
            changed: a.changed_unix_seconds,
        }
    }
}

impl From<&StoredAttrs> for FileAttributes {
    fn from(a: &StoredAttrs) -> Self {
        FileAttributes {
            kind: u8_to_kind(a.kind),
            size: a.size,
            mode: a.mode,
            uid: a.uid,
            gid: a.gid,
            modified_unix_seconds: a.modified,
            accessed_unix_seconds: a.accessed,
            changed_unix_seconds: a.changed,
        }
    }
}

/// Reference to a hydrated content object and the version it holds.
#[derive(Clone, Serialize, Deserialize)]
struct ContentRef {
    object: u64,
    size: u64,
    modified: Option<i64>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    attrs: StoredAttrs,
    #[serde(default)]
    content: Option<ContentRef>,
}

#[derive(Default, Serialize, Deserialize)]
struct Index {
    next_object: u64,
    entries: HashMap<String, Entry>,
    /// Directory path -> child names. Kept separate from `entries` so a listing
    /// can be recorded without knowing the parent directory's own attributes.
    #[serde(default)]
    listings: HashMap<String, Vec<String>>,
}

pub(crate) struct Store {
    dir: PathBuf,
    temp_counter: AtomicU64,
    index: Mutex<Index>,
}

impl Store {
    /// Open (or create) the store at `cache_dir`, loading any existing index. A
    /// missing or unreadable index simply starts empty.
    pub(crate) fn new(cache_dir: PathBuf) -> Self {
        let index = File::open(cache_dir.join("index.json"))
            .ok()
            .and_then(|file| serde_json::from_reader(file).ok())
            .unwrap_or_default();
        Store {
            dir: cache_dir,
            temp_counter: AtomicU64::new(0),
            index: Mutex::new(index),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Index> {
        self.index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn object_path(&self, object: u64) -> PathBuf {
        self.dir.join("objects").join(object.to_string())
    }

    /// Atomically rewrite the index. Best-effort: a failed persist keeps the
    /// in-memory state authoritative for the session.
    fn persist(index: &Index, dir: &std::path::Path) {
        let _ = (|| -> std::io::Result<()> {
            fs::create_dir_all(dir)?;
            let temp = dir.join("index.json.tmp");
            let bytes = serde_json::to_vec(index).map_err(std::io::Error::other)?;
            let mut file = File::create(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temp, dir.join("index.json"))
        })();
    }

    pub(crate) fn get_attrs(&self, path: &RemotePath) -> Option<FileAttributes> {
        self.lock()
            .entries
            .get(path.as_str())
            .map(|entry| (&entry.attrs).into())
    }

    pub(crate) fn get_children(&self, path: &RemotePath) -> Option<Vec<String>> {
        self.lock().listings.get(path.as_str()).cloned()
    }

    /// Record `attrs` for `path`. If content was hydrated at a different
    /// size/mtime, it is dropped so the next read re-hydrates (server wins).
    pub(crate) fn put_attrs(&self, path: &RemotePath, attrs: &FileAttributes) {
        let mut index = self.lock();
        let stale = Self::store_attrs(&mut index, path.as_str(), attrs);
        Self::persist(&index, &self.dir);
        drop(index);
        if let Some(object) = stale {
            let _ = fs::remove_file(self.object_path(object));
        }
    }

    /// Update one entry's attributes in `index`, returning a now-orphaned content
    /// object id if the version changed. Does not persist.
    fn store_attrs(index: &mut Index, key: &str, attrs: &FileAttributes) -> Option<u64> {
        let entry = index.entries.entry(key.to_string()).or_insert_with(|| Entry {
            attrs: attrs.into(),
            content: None,
        });
        entry.attrs = attrs.into();
        if let Some(content) = &entry.content
            && (content.size != attrs.size || content.modified != attrs.modified_unix_seconds)
        {
            let object = content.object;
            entry.content = None;
            return Some(object);
        }
        None
    }

    /// Record a directory listing: store each child's attributes and the parent's
    /// child names, with a single persist.
    pub(crate) fn record_listing(
        &self,
        parent: &RemotePath,
        children: Vec<(String, FileAttributes)>,
    ) {
        let names: Vec<String> = children.iter().map(|(name, _)| name.clone()).collect();
        let mut index = self.lock();
        let mut orphans = Vec::new();
        for (name, attrs) in &children {
            if let Ok(child) = parent.join(name)
                && let Some(object) = Self::store_attrs(&mut index, child.as_str(), attrs)
            {
                orphans.push(object);
            }
        }
        index.listings.insert(parent.as_str().to_string(), names);
        Self::persist(&index, &self.dir);
        drop(index);
        for object in orphans {
            let _ = fs::remove_file(self.object_path(object));
        }
    }

    /// Forget a directory's cached child listing so the next `readdir` refetches.
    pub(crate) fn invalidate_children(&self, path: &RemotePath) {
        let mut index = self.lock();
        if index.listings.remove(path.as_str()).is_some() {
            Self::persist(&index, &self.dir);
        }
    }

    /// Forget `path` (unlink/rmdir or server-side deletion) and its content.
    pub(crate) fn remove(&self, path: &RemotePath) {
        let mut index = self.lock();
        let entry = index.entries.remove(path.as_str());
        let had_listing = index.listings.remove(path.as_str()).is_some();
        if entry.is_some() || had_listing {
            Self::persist(&index, &self.dir);
        }
        drop(index);
        if let Some(content) = entry.and_then(|e| e.content) {
            let _ = fs::remove_file(self.object_path(content.object));
        }
    }

    /// Move `from` to `to` (keeping its content) and evict any descendants of a
    /// renamed directory, whose cached paths are no longer valid.
    pub(crate) fn rename(&self, from: &RemotePath, to: &RemotePath) {
        let mut index = self.lock();
        let mut orphans = Vec::new();

        if let Some(entry) = index.entries.remove(from.as_str()) {
            index.entries.insert(to.as_str().to_string(), entry);
        }
        if let Some(listing) = index.listings.remove(from.as_str()) {
            index.listings.insert(to.as_str().to_string(), listing);
        }
        let prefix = format!("{}/", from.as_str());
        let descendants: Vec<String> = index
            .entries
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .cloned()
            .collect();
        for key in descendants {
            if let Some(entry) = index.entries.remove(&key)
                && let Some(content) = entry.content
            {
                orphans.push(content.object);
            }
            index.listings.remove(&key);
        }
        Self::persist(&index, &self.dir);
        drop(index);
        for object in orphans {
            let _ = fs::remove_file(self.object_path(object));
        }
    }

    /// Drop just the cached content for `path` (e.g. after a write).
    pub(crate) fn invalidate_content(&self, path: &RemotePath) {
        let mut index = self.lock();
        let object = index
            .entries
            .get_mut(path.as_str())
            .and_then(|entry| entry.content.take().map(|c| c.object));
        if object.is_some() {
            Self::persist(&index, &self.dir);
        }
        drop(index);
        if let Some(object) = object {
            let _ = fs::remove_file(self.object_path(object));
        }
    }

    /// Read a range, hydrating the whole file from `remote` if the cached copy is
    /// missing or stale relative to `attrs`.
    pub(crate) fn read(
        &self,
        path: &RemotePath,
        remote: &dyn RemoteFilesystem,
        attrs: &FileAttributes,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>> {
        let object = {
            let index = self.lock();
            index
                .entries
                .get(path.as_str())
                .and_then(|entry| entry.content.as_ref())
                .filter(|c| c.size == attrs.size && c.modified == attrs.modified_unix_seconds)
                .map(|c| c.object)
        };
        let object = match object {
            Some(object) => object,
            None => self.hydrate(path, remote, attrs)?,
        };
        self.read_object(object, offset, size)
    }

    /// Read a range only if content is already hydrated; never contacts the
    /// remote (used in offline mode).
    pub(crate) fn read_cached(&self, path: &RemotePath, offset: u64, size: u32) -> Result<Vec<u8>> {
        let object = self
            .lock()
            .entries
            .get(path.as_str())
            .and_then(|entry| entry.content.as_ref().map(|c| c.object));
        match object {
            Some(object) => self.read_object(object, offset, size),
            None => Err(Error::Unavailable(
                "file contents are not cached and the mount is offline".to_string(),
            )),
        }
    }

    /// Download the whole file to a temp file, flush, atomically rename into the
    /// objects dir, and record the content reference. Returns the object id.
    fn hydrate(
        &self,
        path: &RemotePath,
        remote: &dyn RemoteFilesystem,
        attrs: &FileAttributes,
    ) -> Result<u64> {
        fs::create_dir_all(self.dir.join("objects")).map_err(cache_io)?;
        let temp_dir = self.dir.join("tmp");
        fs::create_dir_all(&temp_dir).map_err(cache_io)?;

        // Reuse this path's object id if it has one, else allocate a new id.
        let object = {
            let mut index = self.lock();
            match index
                .entries
                .get(path.as_str())
                .and_then(|e| e.content.as_ref())
            {
                Some(content) => content.object,
                None => {
                    let id = index.next_object;
                    index.next_object += 1;
                    id
                }
            }
        };

        let temp = temp_dir.join(format!(
            "{object}.{}",
            self.temp_counter.fetch_add(1, Ordering::Relaxed)
        ));
        let download = (|| -> Result<()> {
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
            file.sync_all().map_err(cache_io)
        })();
        if let Err(error) = download {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
        fs::rename(&temp, self.object_path(object)).map_err(cache_io)?;

        let mut index = self.lock();
        let entry = index
            .entries
            .entry(path.as_str().to_string())
            .or_insert_with(|| Entry {
                attrs: attrs.into(),
                content: None,
            });
        entry.content = Some(ContentRef {
            object,
            size: attrs.size,
            modified: attrs.modified_unix_seconds,
        });
        Self::persist(&index, &self.dir);
        Ok(object)
    }

    fn read_object(&self, object: u64, offset: u64, size: u32) -> Result<Vec<u8>> {
        let mut file = File::open(self.object_path(object)).map_err(cache_io)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn file_attrs(size: u64, modified: i64) -> FileAttributes {
        FileAttributes {
            kind: FileKind::File,
            size,
            mode: 0o644,
            uid: 0,
            gid: 0,
            modified_unix_seconds: Some(modified),
            accessed_unix_seconds: None,
            changed_unix_seconds: None,
        }
    }

    /// A remote returning fixed bytes for one file.
    struct BytesRemote(Vec<u8>);
    impl RemoteFilesystem for BytesRemote {
        fn read(&self, _: &RemotePath, offset: u64, size: u32) -> Result<Vec<u8>> {
            let start = (offset as usize).min(self.0.len());
            let end = (start + size as usize).min(self.0.len());
            Ok(self.0[start..end].to_vec())
        }
        fn stat(&self, _: &RemotePath) -> Result<FileAttributes> {
            unreachable!()
        }
        fn read_dir(&self, _: &RemotePath) -> Result<Vec<crate::RemoteDirectoryEntry>> {
            unreachable!()
        }
        fn write(&self, _: &RemotePath, _: u64, _: &[u8]) -> Result<u32> {
            unreachable!()
        }
        fn create(&self, _: &RemotePath, _: u32) -> Result<FileAttributes> {
            unreachable!()
        }
        fn mkdir(&self, _: &RemotePath, _: u32) -> Result<FileAttributes> {
            unreachable!()
        }
        fn unlink(&self, _: &RemotePath) -> Result<()> {
            unreachable!()
        }
        fn rmdir(&self, _: &RemotePath) -> Result<()> {
            unreachable!()
        }
        fn rename(&self, _: &RemotePath, _: &RemotePath) -> Result<()> {
            unreachable!()
        }
        fn setattr(&self, _: &RemotePath, _: crate::SetAttributes) -> Result<FileAttributes> {
            unreachable!()
        }
    }

    #[test]
    fn read_cached_missing_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        assert!(matches!(
            store.read_cached(&RemotePath::new("/x").unwrap(), 0, 8),
            Err(Error::Unavailable(_))
        ));
    }

    #[test]
    fn metadata_and_content_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = RemotePath::new("/a.txt").unwrap();
        let attrs = file_attrs(5, 100);

        {
            let store = Store::new(dir.path().to_path_buf());
            store.put_attrs(&path, &attrs);
            store
                .read(&path, &BytesRemote(b"hello".to_vec()), &attrs, 0, 16)
                .unwrap();
        }

        // Reopen: metadata and content are still available without any remote.
        let store = Store::new(dir.path().to_path_buf());
        assert_eq!(store.get_attrs(&path).unwrap().size, 5);
        assert_eq!(store.read_cached(&path, 0, 16).unwrap(), b"hello");
    }

    #[test]
    fn newer_server_version_drops_stale_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = RemotePath::new("/a.txt").unwrap();
        let store = Store::new(dir.path().to_path_buf());

        store.put_attrs(&path, &file_attrs(5, 100));
        store
            .read(&path, &BytesRemote(b"hello".to_vec()), &file_attrs(5, 100), 0, 16)
            .unwrap();
        assert_eq!(store.read_cached(&path, 0, 16).unwrap(), b"hello");

        // The server now reports a newer version: cached content is dropped.
        store.put_attrs(&path, &file_attrs(8, 200));
        assert!(matches!(
            store.read_cached(&path, 0, 16),
            Err(Error::Unavailable(_))
        ));
    }

    #[test]
    fn listings_persist_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let parent = RemotePath::root();
        {
            let store = Store::new(dir.path().to_path_buf());
            store.record_listing(
                &parent,
                vec![
                    ("a".to_string(), file_attrs(1, 1)),
                    ("b".to_string(), file_attrs(2, 2)),
                ],
            );
        }
        let store = Store::new(dir.path().to_path_buf());
        let children = store.get_children(&parent).unwrap();
        assert_eq!(children, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(store.get_attrs(&RemotePath::new("/a").unwrap()).unwrap().size, 1);
    }
}
