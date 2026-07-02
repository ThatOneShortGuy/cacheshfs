//! The shared cache-backed virtual filesystem.
//!
//! This is the layer platform adapters (`cacheshfs-linux`, `cacheshfs-windows`)
//! talk to. It owns the `NodeId` ⇄ remote-path mapping, the open-handle table,
//! and an in-memory metadata cache, and forwards operations to a
//! [`RemoteFilesystem`].
//!
//! **Metadata caching:** `getattr`, `lookup` (including negative results), and
//! `readdir` results are cached with a TTL so repeated metadata queries (as file
//! managers issue constantly) avoid remote round-trips. Mutations invalidate the
//! affected cache entries. In [`CacheMode::Offline`] the cache is served
//! regardless of age and the remote is never contacted.
//!
//! **Writes** are still write-through and uncached: reads and writes of file
//! *contents* always go to the remote (`flush` is a no-op, no dirty state). An
//! on-disk content cache is a later layer. When the mount is read-only, mutating
//! operations are rejected with [`Error::PermissionDenied`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::{
    CacheMode, CreatedFile, DirectoryEntry, Error, FileAttributes, FileHandle, FileMetadata, NodeId,
    OpenFlags, RemoteFilesystem, RemotePath, Result, SetAttributes, VirtualFilesystem,
};

/// Longest a negative (name-not-found) cache entry is trusted, capped so a file
/// created elsewhere becomes visible quickly.
const NEGATIVE_TTL_CAP: Duration = Duration::from_secs(1);

/// State for one open file handle.
struct OpenFile {
    node: NodeId,
    path: RemotePath,
}

/// Mutable registry and metadata cache shared behind a mutex.
struct State {
    next_node: u64,
    next_handle: u64,
    path_to_node: HashMap<RemotePath, NodeId>,
    node_to_path: HashMap<NodeId, RemotePath>,
    handles: HashMap<FileHandle, OpenFile>,

    // Metadata cache. Each entry records when it was fetched so a TTL can be
    // applied. `lookups` maps (parent, name) to the child node, or `None` for a
    // cached negative (not-found) result.
    attrs: HashMap<NodeId, (FileAttributes, Instant)>,
    lookups: HashMap<(NodeId, String), (Option<NodeId>, Instant)>,
    dirs: HashMap<NodeId, (Vec<DirectoryEntry>, Instant)>,
}

impl State {
    /// Return the remote path for a node, or [`Error::NotFound`] if unknown.
    fn path_of(&self, node: NodeId) -> Result<RemotePath> {
        self.node_to_path.get(&node).cloned().ok_or(Error::NotFound)
    }

    /// Return the existing `NodeId` for `path`, allocating a fresh one if this
    /// is the first time the path is seen. Identity is stable for as long as the
    /// mapping lives, which is what FUSE/WinFsp expect from inode numbers.
    fn intern(&mut self, path: RemotePath) -> NodeId {
        if let Some(&node) = self.path_to_node.get(&path) {
            return node;
        }
        let node = NodeId(self.next_node);
        self.next_node += 1;
        self.path_to_node.insert(path.clone(), node);
        self.node_to_path.insert(node, path);
        node
    }

    /// Allocate a fresh file handle for `node` at `path`.
    fn open_handle(&mut self, node: NodeId, path: RemotePath) -> FileHandle {
        let handle = FileHandle(self.next_handle);
        self.next_handle += 1;
        self.handles.insert(handle, OpenFile { node, path });
        handle
    }

    /// Re-point the node mapped to `from` (if any) at `to` after a rename, so the
    /// node identity survives the move. Descendants of a renamed directory are
    /// not rewritten — they are re-resolved by path on next access.
    fn rename_path(&mut self, from: &RemotePath, to: &RemotePath) {
        if let Some(node) = self.path_to_node.remove(from) {
            self.path_to_node.insert(to.clone(), node);
            self.node_to_path.insert(node, to.clone());
        }
    }

    /// Drop a node's cached attributes and (if it is a directory) its listing.
    fn invalidate_node(&mut self, node: NodeId) {
        self.attrs.remove(&node);
        self.dirs.remove(&node);
    }

    /// Record that `name` no longer exists under `parent` (negative entry).
    fn mark_absent(&mut self, parent: NodeId, name: &str, now: Instant) {
        self.lookups.insert((parent, name.to_string()), (None, now));
    }

    /// Forget any cached resolution of `name` under `parent`.
    fn forget_name(&mut self, parent: NodeId, name: &str) {
        self.lookups.remove(&(parent, name.to_string()));
    }
}

/// Cache-backed write-through virtual filesystem over a [`RemoteFilesystem`].
pub struct CacheVfs {
    remote: Arc<dyn RemoteFilesystem>,
    read_only: bool,
    cache_mode: CacheMode,
    metadata_ttl: Duration,
    negative_ttl: Duration,
    state: Mutex<State>,
}

impl CacheVfs {
    /// Create a VFS rooted at `root` on `remote`. The root maps to
    /// [`NodeId::ROOT`]. `read_only` rejects mutations; `cache_mode` and
    /// `metadata_ttl` control metadata caching.
    pub fn new(
        remote: Arc<dyn RemoteFilesystem>,
        root: RemotePath,
        read_only: bool,
        cache_mode: CacheMode,
        metadata_ttl: Duration,
    ) -> Self {
        let mut path_to_node = HashMap::new();
        let mut node_to_path = HashMap::new();
        path_to_node.insert(root.clone(), NodeId::ROOT);
        node_to_path.insert(NodeId::ROOT, root);

        CacheVfs {
            remote,
            read_only,
            cache_mode,
            metadata_ttl,
            negative_ttl: metadata_ttl.min(NEGATIVE_TTL_CAP),
            state: Mutex::new(State {
                next_node: NodeId::ROOT.0 + 1,
                next_handle: 1,
                path_to_node,
                node_to_path,
                handles: HashMap::new(),
                attrs: HashMap::new(),
                lookups: HashMap::new(),
                dirs: HashMap::new(),
            }),
        }
    }

    /// Lock the state, recovering from a poisoned mutex rather than propagating
    /// a panic across the FFI boundary into a platform adapter.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn offline(&self) -> bool {
        self.cache_mode == CacheMode::Offline
    }

    /// Whether a positive cache entry fetched at `at` is still trusted. Offline
    /// mode trusts cached metadata regardless of age.
    fn fresh(&self, at: Instant) -> bool {
        self.offline() || at.elapsed() < self.metadata_ttl
    }

    /// Whether a negative (not-found) cache entry fetched at `at` is trusted.
    fn fresh_negative(&self, at: Instant) -> bool {
        self.offline() || at.elapsed() < self.negative_ttl
    }

    /// Reject the operation when the mount is read-only.
    fn ensure_writable(&self) -> Result<()> {
        if self.read_only {
            Err(Error::PermissionDenied)
        } else {
            Ok(())
        }
    }

    /// Resolve `parent`'s path and append `name` to it.
    fn child_path(&self, parent: NodeId, name: &str) -> Result<RemotePath> {
        self.lock().path_of(parent)?.join(name)
    }
}

impl VirtualFilesystem for CacheVfs {
    fn lookup(&self, parent: NodeId, name: &str) -> Result<FileMetadata> {
        // Serve from the metadata cache when the name resolution and the child's
        // attributes are both still fresh.
        {
            let state = self.lock();
            if let Some((child, at)) = state.lookups.get(&(parent, name.to_string())) {
                match child {
                    None if self.fresh_negative(*at) => return Err(Error::NotFound),
                    Some(child) if self.fresh(*at) => {
                        if let Some((attributes, attr_at)) = state.attrs.get(child)
                            && self.fresh(*attr_at)
                        {
                            return Ok(FileMetadata {
                                node: *child,
                                attributes: attributes.clone(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        let child_path = self.lock().path_of(parent)?.join(name)?;
        if self.offline() {
            return Err(Error::NotFound);
        }

        match self.remote.stat(&child_path) {
            Ok(attributes) => {
                let now = Instant::now();
                let mut state = self.lock();
                let node = state.intern(child_path);
                state
                    .lookups
                    .insert((parent, name.to_string()), (Some(node), now));
                state.attrs.insert(node, (attributes.clone(), now));
                Ok(FileMetadata { node, attributes })
            }
            Err(Error::NotFound) => {
                self.lock().mark_absent(parent, name, Instant::now());
                Err(Error::NotFound)
            }
            Err(other) => Err(other),
        }
    }

    fn getattr(&self, node: NodeId) -> Result<FileMetadata> {
        {
            let state = self.lock();
            if let Some((attributes, at)) = state.attrs.get(&node)
                && self.fresh(*at)
            {
                return Ok(FileMetadata {
                    node,
                    attributes: attributes.clone(),
                });
            }
        }

        let path = self.lock().path_of(node)?;
        if self.offline() {
            return Err(Error::NotFound);
        }
        let attributes = self.remote.stat(&path)?;
        self.lock()
            .attrs
            .insert(node, (attributes.clone(), Instant::now()));
        Ok(FileMetadata { node, attributes })
    }

    fn readdir(&self, node: NodeId) -> Result<Vec<DirectoryEntry>> {
        {
            let state = self.lock();
            if let Some((entries, at)) = state.dirs.get(&node)
                && self.fresh(*at)
            {
                return Ok(entries.clone());
            }
        }

        let path = self.lock().path_of(node)?;
        if self.offline() {
            return Err(Error::NotFound);
        }
        let entries = self.remote.read_dir(&path)?;

        let now = Instant::now();
        let mut state = self.lock();
        let mut result = Vec::with_capacity(entries.len());
        for entry in entries {
            // The transport may surface "." / ".."; the VFS exposes only real
            // children and lets the platform adapter synthesize the dot entries.
            if entry.name == "." || entry.name == ".." {
                continue;
            }
            let child_path = path.join(&entry.name)?;
            let child = state.intern(child_path);
            // Populate the per-child caches so following getattr/lookup calls
            // (which a listing is invariably followed by) are cache hits.
            state.attrs.insert(child, (entry.attributes.clone(), now));
            state
                .lookups
                .insert((node, entry.name.clone()), (Some(child), now));
            result.push(DirectoryEntry {
                name: entry.name,
                metadata: FileMetadata {
                    node: child,
                    attributes: entry.attributes,
                },
            });
        }
        state.dirs.insert(node, (result.clone(), now));
        Ok(result)
    }

    fn open(&self, node: NodeId, flags: OpenFlags) -> Result<FileHandle> {
        if flags.write || flags.append || flags.truncate {
            self.ensure_writable()?;
        }
        let path = self.lock().path_of(node)?;
        // Truncate-on-open: zero the remote file before handing back a handle.
        if flags.truncate {
            self.remote.setattr(
                &path,
                SetAttributes {
                    size: Some(0),
                    ..SetAttributes::default()
                },
            )?;
            self.lock().invalidate_node(node);
        }
        Ok(self.lock().open_handle(node, path))
    }

    fn read(&self, handle: FileHandle, offset: u64, size: u32) -> Result<Vec<u8>> {
        let path = {
            let state = self.lock();
            state
                .handles
                .get(&handle)
                .map(|file| file.path.clone())
                .ok_or_else(|| Error::InvalidInput("unknown file handle".to_string()))?
        };
        self.remote.read(&path, offset, size)
    }

    fn write(&self, handle: FileHandle, offset: u64, data: &[u8]) -> Result<u32> {
        self.ensure_writable()?;
        let (node, path) = {
            let state = self.lock();
            let file = state
                .handles
                .get(&handle)
                .ok_or_else(|| Error::InvalidInput("unknown file handle".to_string()))?;
            (file.node, file.path.clone())
        };
        let written = self.remote.write(&path, offset, data)?;
        // The write changed size/mtime; drop the stale cached attributes.
        self.lock().invalidate_node(node);
        Ok(written)
    }

    fn flush(&self, _handle: FileHandle) -> Result<()> {
        // Writes are applied to the remote immediately, so nothing is buffered.
        Ok(())
    }

    fn release(&self, handle: FileHandle) -> Result<()> {
        self.lock().handles.remove(&handle);
        Ok(())
    }

    fn create(
        &self,
        parent: NodeId,
        name: &str,
        mode: u32,
        _flags: OpenFlags,
    ) -> Result<CreatedFile> {
        self.ensure_writable()?;
        let child_path = self.child_path(parent, name)?;
        let attributes = self.remote.create(&child_path, mode)?;

        let now = Instant::now();
        let mut state = self.lock();
        let node = state.intern(child_path.clone());
        let handle = state.open_handle(node, child_path);
        state.attrs.insert(node, (attributes.clone(), now));
        state
            .lookups
            .insert((parent, name.to_string()), (Some(node), now));
        state.dirs.remove(&parent);
        Ok(CreatedFile {
            metadata: FileMetadata { node, attributes },
            handle,
        })
    }

    fn mkdir(&self, parent: NodeId, name: &str, mode: u32) -> Result<FileMetadata> {
        self.ensure_writable()?;
        let child_path = self.child_path(parent, name)?;
        let attributes = self.remote.mkdir(&child_path, mode)?;

        let now = Instant::now();
        let mut state = self.lock();
        let node = state.intern(child_path);
        state.attrs.insert(node, (attributes.clone(), now));
        state
            .lookups
            .insert((parent, name.to_string()), (Some(node), now));
        state.dirs.remove(&parent);
        Ok(FileMetadata { node, attributes })
    }

    fn unlink(&self, parent: NodeId, name: &str) -> Result<()> {
        self.ensure_writable()?;
        let child_path = self.child_path(parent, name)?;
        self.remote.unlink(&child_path)?;

        let mut state = self.lock();
        if let Some(&node) = state.path_to_node.get(&child_path) {
            state.invalidate_node(node);
        }
        state.mark_absent(parent, name, Instant::now());
        state.dirs.remove(&parent);
        Ok(())
    }

    fn rmdir(&self, parent: NodeId, name: &str) -> Result<()> {
        self.ensure_writable()?;
        let child_path = self.child_path(parent, name)?;
        self.remote.rmdir(&child_path)?;

        let mut state = self.lock();
        if let Some(&node) = state.path_to_node.get(&child_path) {
            state.invalidate_node(node);
        }
        state.mark_absent(parent, name, Instant::now());
        state.dirs.remove(&parent);
        Ok(())
    }

    fn rename(&self, parent: NodeId, name: &str, new_parent: NodeId, new_name: &str) -> Result<()> {
        self.ensure_writable()?;
        let from = self.child_path(parent, name)?;
        let to = self.child_path(new_parent, new_name)?;
        self.remote.rename(&from, &to)?;

        let mut state = self.lock();
        state.rename_path(&from, &to);
        if let Some(&node) = state.path_to_node.get(&to) {
            state.invalidate_node(node);
        }
        state.mark_absent(parent, name, Instant::now());
        state.forget_name(new_parent, new_name);
        state.dirs.remove(&parent);
        state.dirs.remove(&new_parent);
        Ok(())
    }

    fn setattr(&self, node: NodeId, attributes: SetAttributes) -> Result<FileMetadata> {
        self.ensure_writable()?;
        let path = self.lock().path_of(node)?;
        let attributes = self.remote.setattr(&path, attributes)?;
        // Refresh the cache with the server's post-change attributes.
        self.lock()
            .attrs
            .insert(node, (attributes.clone(), Instant::now()));
        Ok(FileMetadata { node, attributes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileAttributes, FileKind, RemoteDirectoryEntry};
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    /// A tiny in-memory remote tree for exercising the VFS.
    ///
    /// Layout:
    /// ```text
    /// /            (dir)
    /// /readme.txt  (file: "hello")
    /// /sub         (dir)
    /// /sub/a.txt   (file: "aaaa")
    /// ```
    struct MockState {
        /// path -> (attributes, optional file contents)
        nodes: HashMap<String, (FileAttributes, Option<Vec<u8>>)>,
        /// dir path -> child names
        dirs: HashMap<String, Vec<String>>,
        /// remote call counters, for asserting cache hits
        stat_calls: usize,
        read_dir_calls: usize,
    }

    /// In-memory mutable remote tree (interior mutability so `&self` methods can
    /// mutate, like a real transport over a shared connection).
    struct MockRemote {
        state: StdMutex<MockState>,
    }

    /// Split an absolute path into (parent, leaf).
    fn split(path: &str) -> (String, String) {
        let trimmed = path.trim_end_matches('/');
        match trimmed.rsplit_once('/') {
            Some((parent, name)) => {
                let parent = if parent.is_empty() {
                    "/".to_string()
                } else {
                    parent.to_string()
                };
                (parent, name.to_string())
            }
            None => ("/".to_string(), trimmed.to_string()),
        }
    }

    fn dir_attrs() -> FileAttributes {
        FileAttributes {
            kind: FileKind::Directory,
            size: 0,
            mode: 0o755,
            uid: 0,
            gid: 0,
            modified_unix_seconds: None,
            accessed_unix_seconds: None,
            changed_unix_seconds: None,
        }
    }

    fn file_attrs(size: u64) -> FileAttributes {
        FileAttributes {
            kind: FileKind::File,
            size,
            mode: 0o644,
            uid: 0,
            gid: 0,
            modified_unix_seconds: Some(1_700_000_000),
            accessed_unix_seconds: None,
            changed_unix_seconds: None,
        }
    }

    impl MockRemote {
        fn new() -> Self {
            let mut nodes = HashMap::new();
            nodes.insert("/".to_string(), (dir_attrs(), None));
            nodes.insert(
                "/readme.txt".to_string(),
                (file_attrs(5), Some(b"hello".to_vec())),
            );
            nodes.insert("/sub".to_string(), (dir_attrs(), None));
            nodes.insert(
                "/sub/a.txt".to_string(),
                (file_attrs(4), Some(b"aaaa".to_vec())),
            );

            let mut dirs = HashMap::new();
            dirs.insert(
                "/".to_string(),
                vec!["readme.txt".to_string(), "sub".to_string()],
            );
            dirs.insert("/sub".to_string(), vec!["a.txt".to_string()]);

            MockRemote {
                state: StdMutex::new(MockState {
                    nodes,
                    dirs,
                    stat_calls: 0,
                    read_dir_calls: 0,
                }),
            }
        }

        fn stat_calls(&self) -> usize {
            self.state.lock().unwrap().stat_calls
        }

        fn read_dir_calls(&self) -> usize {
            self.state.lock().unwrap().read_dir_calls
        }
    }

    impl RemoteFilesystem for MockRemote {
        fn stat(&self, path: &RemotePath) -> Result<FileAttributes> {
            let mut state = self.state.lock().unwrap();
            state.stat_calls += 1;
            state
                .nodes
                .get(path.as_str())
                .map(|(attrs, _)| attrs.clone())
                .ok_or(Error::NotFound)
        }

        fn read_dir(&self, path: &RemotePath) -> Result<Vec<RemoteDirectoryEntry>> {
            let mut state = self.state.lock().unwrap();
            state.read_dir_calls += 1;
            let names = state.dirs.get(path.as_str()).ok_or(Error::NotFound)?;
            Ok(names
                .iter()
                .map(|name| {
                    let child = path.join(name).unwrap();
                    let (attrs, _) = state.nodes.get(child.as_str()).unwrap();
                    RemoteDirectoryEntry {
                        name: name.clone(),
                        attributes: attrs.clone(),
                    }
                })
                .collect())
        }

        fn read(&self, path: &RemotePath, offset: u64, size: u32) -> Result<Vec<u8>> {
            let state = self.state.lock().unwrap();
            let (_, contents) = state.nodes.get(path.as_str()).ok_or(Error::NotFound)?;
            let contents = contents
                .as_ref()
                .ok_or(Error::InvalidInput("is a directory".into()))?;
            let start = (offset as usize).min(contents.len());
            let end = (start + size as usize).min(contents.len());
            Ok(contents[start..end].to_vec())
        }

        fn write(&self, path: &RemotePath, offset: u64, data: &[u8]) -> Result<u32> {
            let mut state = self.state.lock().unwrap();
            let (attrs, contents) = state.nodes.get_mut(path.as_str()).ok_or(Error::NotFound)?;
            let contents = contents
                .as_mut()
                .ok_or(Error::InvalidInput("is a directory".into()))?;
            let end = offset as usize + data.len();
            if contents.len() < end {
                contents.resize(end, 0);
            }
            contents[offset as usize..end].copy_from_slice(data);
            attrs.size = contents.len() as u64;
            Ok(data.len() as u32)
        }

        fn create(&self, path: &RemotePath, mode: u32) -> Result<FileAttributes> {
            let mut state = self.state.lock().unwrap();
            if state.nodes.contains_key(path.as_str()) {
                return Err(Error::AlreadyExists);
            }
            let mut attrs = file_attrs(0);
            attrs.mode = mode;
            state
                .nodes
                .insert(path.as_str().to_string(), (attrs.clone(), Some(Vec::new())));
            let (parent, name) = split(path.as_str());
            state.dirs.entry(parent).or_default().push(name);
            Ok(attrs)
        }

        fn mkdir(&self, path: &RemotePath, mode: u32) -> Result<FileAttributes> {
            let mut state = self.state.lock().unwrap();
            if state.nodes.contains_key(path.as_str()) {
                return Err(Error::AlreadyExists);
            }
            let mut attrs = dir_attrs();
            attrs.mode = mode;
            state
                .nodes
                .insert(path.as_str().to_string(), (attrs.clone(), None));
            state.dirs.insert(path.as_str().to_string(), Vec::new());
            let (parent, name) = split(path.as_str());
            state.dirs.entry(parent).or_default().push(name);
            Ok(attrs)
        }

        fn unlink(&self, path: &RemotePath) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            state.nodes.remove(path.as_str()).ok_or(Error::NotFound)?;
            let (parent, name) = split(path.as_str());
            if let Some(children) = state.dirs.get_mut(&parent) {
                children.retain(|child| child != &name);
            }
            Ok(())
        }

        fn rmdir(&self, path: &RemotePath) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            state.nodes.remove(path.as_str()).ok_or(Error::NotFound)?;
            state.dirs.remove(path.as_str());
            let (parent, name) = split(path.as_str());
            if let Some(children) = state.dirs.get_mut(&parent) {
                children.retain(|child| child != &name);
            }
            Ok(())
        }

        fn rename(&self, from: &RemotePath, to: &RemotePath) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            let node = state.nodes.remove(from.as_str()).ok_or(Error::NotFound)?;
            state.nodes.insert(to.as_str().to_string(), node);
            if let Some(listing) = state.dirs.remove(from.as_str()) {
                state.dirs.insert(to.as_str().to_string(), listing);
            }
            let (from_parent, from_name) = split(from.as_str());
            if let Some(children) = state.dirs.get_mut(&from_parent) {
                children.retain(|child| child != &from_name);
            }
            let (to_parent, to_name) = split(to.as_str());
            state.dirs.entry(to_parent).or_default().push(to_name);
            Ok(())
        }

        fn setattr(&self, path: &RemotePath, set: SetAttributes) -> Result<FileAttributes> {
            let mut state = self.state.lock().unwrap();
            let (attrs, contents) = state.nodes.get_mut(path.as_str()).ok_or(Error::NotFound)?;
            if let Some(size) = set.size {
                if let Some(contents) = contents.as_mut() {
                    contents.resize(size as usize, 0);
                }
                attrs.size = size;
            }
            if let Some(mode) = set.mode {
                attrs.mode = mode;
            }
            if set.modified_unix_seconds.is_some() {
                attrs.modified_unix_seconds = set.modified_unix_seconds;
            }
            if set.accessed_unix_seconds.is_some() {
                attrs.accessed_unix_seconds = set.accessed_unix_seconds;
            }
            Ok(attrs.clone())
        }
    }

    // A generous TTL so caching is active and invalidation logic is exercised.
    const LONG_TTL: Duration = Duration::from_secs(3600);

    fn vfs() -> CacheVfs {
        CacheVfs::new(
            Arc::new(MockRemote::new()),
            RemotePath::root(),
            false,
            CacheMode::OnDemand,
            LONG_TTL,
        )
    }

    fn read_only_vfs() -> CacheVfs {
        CacheVfs::new(
            Arc::new(MockRemote::new()),
            RemotePath::root(),
            true,
            CacheMode::OnDemand,
            LONG_TTL,
        )
    }

    /// A VFS plus a handle to its mock remote, for asserting on call counts.
    fn vfs_with(mode: CacheMode, ttl: Duration) -> (CacheVfs, Arc<MockRemote>) {
        let remote = Arc::new(MockRemote::new());
        let vfs = CacheVfs::new(remote.clone(), RemotePath::root(), false, mode, ttl);
        (vfs, remote)
    }

    #[test]
    fn getattr_root_is_a_directory() {
        let meta = vfs().getattr(NodeId::ROOT).unwrap();
        assert_eq!(meta.node, NodeId::ROOT);
        assert_eq!(meta.attributes.kind, FileKind::Directory);
    }

    #[test]
    fn lookup_returns_stable_node_ids() {
        let vfs = vfs();
        let first = vfs.lookup(NodeId::ROOT, "readme.txt").unwrap();
        let second = vfs.lookup(NodeId::ROOT, "readme.txt").unwrap();
        assert_eq!(first.node, second.node);
        assert_ne!(first.node, NodeId::ROOT);
        assert_eq!(first.attributes.size, 5);
    }

    #[test]
    fn lookup_missing_is_not_found() {
        assert!(matches!(
            vfs().lookup(NodeId::ROOT, "nope"),
            Err(Error::NotFound)
        ));
    }

    #[test]
    fn readdir_lists_children_with_consistent_nodes() {
        let vfs = vfs();
        let entries = vfs.readdir(NodeId::ROOT).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"readme.txt"));
        assert!(names.contains(&"sub"));

        // A node id from readdir matches the one from a direct lookup.
        let from_lookup = vfs.lookup(NodeId::ROOT, "sub").unwrap().node;
        let from_readdir = entries
            .iter()
            .find(|e| e.name == "sub")
            .unwrap()
            .metadata
            .node;
        assert_eq!(from_lookup, from_readdir);
    }

    #[test]
    fn nested_lookup_and_read() {
        let vfs = vfs();
        let sub = vfs.lookup(NodeId::ROOT, "sub").unwrap().node;
        let file = vfs.lookup(sub, "a.txt").unwrap();
        assert_eq!(file.attributes.kind, FileKind::File);

        let handle = vfs
            .open(
                file.node,
                OpenFlags {
                    read: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let data = vfs.read(handle, 0, 1024).unwrap();
        assert_eq!(data, b"aaaa");

        // Partial read honoring offset/size.
        let partial = vfs.read(handle, 1, 2).unwrap();
        assert_eq!(partial, b"aa");

        vfs.release(handle).unwrap();
        // Reading a released handle fails.
        assert!(vfs.read(handle, 0, 4).is_err());
    }

    #[test]
    fn create_write_and_read_back() {
        let vfs = vfs();
        let created = vfs
            .create(NodeId::ROOT, "new.txt", 0o644, OpenFlags::default())
            .unwrap();
        assert_eq!(created.metadata.attributes.kind, FileKind::File);

        let written = vfs.write(created.handle, 0, b"hello world").unwrap();
        assert_eq!(written, 11);

        // Re-open and read it back through a fresh handle.
        let handle = vfs
            .open(
                created.metadata.node,
                OpenFlags {
                    read: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(vfs.read(handle, 0, 1024).unwrap(), b"hello world");

        // It shows up in the parent listing.
        let names: Vec<_> = vfs
            .readdir(NodeId::ROOT)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.iter().any(|n| n == "new.txt"));
    }

    #[test]
    fn truncate_via_setattr_and_open() {
        let vfs = vfs();
        let file = vfs.lookup(NodeId::ROOT, "readme.txt").unwrap().node;
        let meta = vfs
            .setattr(
                file,
                SetAttributes {
                    size: Some(2),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(meta.attributes.size, 2);

        // Open with truncate zeroes it.
        vfs.open(
            file,
            OpenFlags {
                write: true,
                truncate: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(vfs.getattr(file).unwrap().attributes.size, 0);
    }

    #[test]
    fn mkdir_unlink_and_rmdir() {
        let vfs = vfs();
        let dir = vfs.mkdir(NodeId::ROOT, "d", 0o755).unwrap();
        assert_eq!(dir.attributes.kind, FileKind::Directory);

        vfs.create(dir.node, "f.txt", 0o644, OpenFlags::default())
            .unwrap();
        assert_eq!(vfs.readdir(dir.node).unwrap().len(), 1);
        vfs.unlink(dir.node, "f.txt").unwrap();
        assert!(vfs.readdir(dir.node).unwrap().is_empty());

        vfs.rmdir(NodeId::ROOT, "d").unwrap();
        assert!(matches!(
            vfs.lookup(NodeId::ROOT, "d"),
            Err(Error::NotFound)
        ));
    }

    #[test]
    fn rename_moves_and_keeps_node_identity() {
        let vfs = vfs();
        let node = vfs.lookup(NodeId::ROOT, "readme.txt").unwrap().node;
        vfs.rename(NodeId::ROOT, "readme.txt", NodeId::ROOT, "renamed.txt")
            .unwrap();

        // Old name is gone, new name resolves to the same node.
        assert!(matches!(
            vfs.lookup(NodeId::ROOT, "readme.txt"),
            Err(Error::NotFound)
        ));
        let moved = vfs.lookup(NodeId::ROOT, "renamed.txt").unwrap();
        assert_eq!(moved.node, node);
        // The retained node now reports the new path's metadata.
        assert_eq!(vfs.getattr(node).unwrap().attributes.size, 5);
    }

    #[test]
    fn read_only_mount_rejects_mutations_but_allows_reads() {
        let vfs = read_only_vfs();
        // Reads still work.
        assert_eq!(
            vfs.getattr(NodeId::ROOT).unwrap().attributes.kind,
            FileKind::Directory
        );
        let file = vfs.lookup(NodeId::ROOT, "readme.txt").unwrap().node;
        let handle = vfs
            .open(
                file,
                OpenFlags {
                    read: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(vfs.read(handle, 0, 5).unwrap(), b"hello");

        // Every mutation is denied.
        assert!(matches!(
            vfs.open(
                file,
                OpenFlags {
                    write: true,
                    ..Default::default()
                }
            ),
            Err(Error::PermissionDenied)
        ));
        assert!(matches!(
            vfs.create(NodeId::ROOT, "x", 0o644, OpenFlags::default()),
            Err(Error::PermissionDenied)
        ));
        assert!(matches!(
            vfs.mkdir(NodeId::ROOT, "x", 0o755),
            Err(Error::PermissionDenied)
        ));
        assert!(matches!(
            vfs.unlink(NodeId::ROOT, "readme.txt"),
            Err(Error::PermissionDenied)
        ));
        assert!(matches!(
            vfs.setattr(file, SetAttributes::default()),
            Err(Error::PermissionDenied)
        ));
    }

    #[test]
    fn lookup_rejects_path_traversal() {
        let vfs = vfs();
        assert!(matches!(
            vfs.lookup(NodeId::ROOT, ".."),
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            vfs.lookup(NodeId::ROOT, "a/b"),
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn getattr_unknown_node_is_not_found() {
        assert!(matches!(vfs().getattr(NodeId(999)), Err(Error::NotFound)));
    }

    #[test]
    fn create_write_flush_release_and_read_round_trip() {
        let vfs = vfs();
        let created = vfs
            .create(
                NodeId::ROOT,
                "created.txt",
                0o640,
                OpenFlags {
                    read: true,
                    write: true,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(created.metadata.attributes.kind, FileKind::File);
        assert_eq!(created.metadata.attributes.mode, 0o640);
        assert_eq!(created.metadata.attributes.size, 0);

        assert_eq!(vfs.write(created.handle, 0, b"hello").unwrap(), 5);
        vfs.flush(created.handle).unwrap();
        vfs.release(created.handle).unwrap();

        let looked_up = vfs.lookup(NodeId::ROOT, "created.txt").unwrap();
        assert_eq!(looked_up.node, created.metadata.node);
        assert_eq!(looked_up.attributes.size, 5);

        let handle = vfs
            .open(
                looked_up.node,
                OpenFlags {
                    read: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(vfs.read(handle, 0, 32).unwrap(), b"hello");
        vfs.release(handle).unwrap();
    }

    #[test]
    fn write_updates_existing_file_at_offset_and_refreshes_size() {
        let vfs = vfs();
        let file = vfs.lookup(NodeId::ROOT, "readme.txt").unwrap();
        let handle = vfs
            .open(
                file.node,
                OpenFlags {
                    read: true,
                    write: true,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(vfs.write(handle, 2, b"YY").unwrap(), 2);
        assert_eq!(vfs.write(handle, 5, b"!").unwrap(), 1);
        vfs.flush(handle).unwrap();
        vfs.release(handle).unwrap();

        let refreshed = vfs.getattr(file.node).unwrap();
        assert_eq!(refreshed.attributes.size, 6);

        let handle = vfs
            .open(
                file.node,
                OpenFlags {
                    read: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(vfs.read(handle, 0, 32).unwrap(), b"heYYo!");
        vfs.release(handle).unwrap();
    }

    #[test]
    fn getattr_is_cached_within_ttl() {
        let (vfs, remote) = vfs_with(CacheMode::OnDemand, LONG_TTL);
        vfs.getattr(NodeId::ROOT).unwrap();
        vfs.getattr(NodeId::ROOT).unwrap();
        assert_eq!(remote.stat_calls(), 1, "second getattr should be a cache hit");
    }

    #[test]
    fn getattr_refetches_after_write_invalidation() {
        let (vfs, remote) = vfs_with(CacheMode::OnDemand, LONG_TTL);
        let file = vfs.lookup(NodeId::ROOT, "readme.txt").unwrap().node;
        vfs.getattr(file).unwrap();
        let before = remote.stat_calls();

        let handle = vfs
            .open(file, OpenFlags { write: true, ..Default::default() })
            .unwrap();
        vfs.write(handle, 0, b"X").unwrap();

        vfs.getattr(file).unwrap();
        assert_eq!(
            remote.stat_calls(),
            before + 1,
            "write should invalidate the cached attributes"
        );
    }

    #[test]
    fn readdir_is_cached_and_populates_lookup() {
        let (vfs, remote) = vfs_with(CacheMode::OnDemand, LONG_TTL);
        vfs.readdir(NodeId::ROOT).unwrap();
        vfs.readdir(NodeId::ROOT).unwrap();
        assert_eq!(remote.read_dir_calls(), 1, "second readdir should be cached");
        // The listing primed the lookup/attr caches, so this needs no remote stat.
        vfs.lookup(NodeId::ROOT, "readme.txt").unwrap();
        assert_eq!(remote.stat_calls(), 0, "lookup after readdir should be cached");
    }

    #[test]
    fn repeated_lookup_is_cached() {
        let (vfs, remote) = vfs_with(CacheMode::OnDemand, LONG_TTL);
        vfs.lookup(NodeId::ROOT, "readme.txt").unwrap();
        vfs.lookup(NodeId::ROOT, "readme.txt").unwrap();
        assert_eq!(remote.stat_calls(), 1, "second lookup should be a cache hit");
    }

    #[test]
    fn negative_lookup_is_cached() {
        let (vfs, remote) = vfs_with(CacheMode::OnDemand, LONG_TTL);
        assert!(matches!(vfs.lookup(NodeId::ROOT, "nope"), Err(Error::NotFound)));
        assert!(matches!(vfs.lookup(NodeId::ROOT, "nope"), Err(Error::NotFound)));
        assert_eq!(remote.stat_calls(), 1, "negative result should be cached");
    }

    #[test]
    fn zero_ttl_disables_caching() {
        let (vfs, remote) = vfs_with(CacheMode::OnDemand, Duration::ZERO);
        vfs.getattr(NodeId::ROOT).unwrap();
        vfs.getattr(NodeId::ROOT).unwrap();
        assert_eq!(remote.stat_calls(), 2, "TTL of zero should always re-fetch");
    }

    #[test]
    fn offline_mode_never_contacts_remote() {
        let (vfs, remote) = vfs_with(CacheMode::Offline, LONG_TTL);
        // Nothing is cached and we must not reach out to the remote.
        assert!(matches!(vfs.getattr(NodeId::ROOT), Err(Error::NotFound)));
        assert!(matches!(vfs.readdir(NodeId::ROOT), Err(Error::NotFound)));
        assert_eq!(remote.stat_calls(), 0);
        assert_eq!(remote.read_dir_calls(), 0);
    }

    #[test]
    fn create_clears_a_cached_negative_lookup() {
        let vfs = vfs();
        // Cache a negative result, then create the same name.
        assert!(matches!(
            vfs.lookup(NodeId::ROOT, "fresh.txt"),
            Err(Error::NotFound)
        ));
        vfs.create(NodeId::ROOT, "fresh.txt", 0o644, OpenFlags::default())
            .unwrap();
        // The stale negative must not be served.
        assert_eq!(
            vfs.lookup(NodeId::ROOT, "fresh.txt").unwrap().attributes.kind,
            FileKind::File
        );
        assert!(
            vfs.readdir(NodeId::ROOT)
                .unwrap()
                .iter()
                .any(|e| e.name == "fresh.txt")
        );
    }

    #[test]
    fn unlink_negatively_caches_without_further_remote_calls() {
        let (vfs, remote) = vfs_with(CacheMode::OnDemand, LONG_TTL);
        vfs.lookup(NodeId::ROOT, "readme.txt").unwrap();
        vfs.unlink(NodeId::ROOT, "readme.txt").unwrap();
        let calls = remote.stat_calls();

        // The unlinked name is now cached-negative: NotFound with no extra stat.
        assert!(matches!(
            vfs.lookup(NodeId::ROOT, "readme.txt"),
            Err(Error::NotFound)
        ));
        assert_eq!(remote.stat_calls(), calls);
    }

    #[test]
    fn readdir_reflects_create_and_unlink() {
        let vfs = vfs();
        let before = vfs.readdir(NodeId::ROOT).unwrap().len();

        vfs.create(NodeId::ROOT, "extra.txt", 0o644, OpenFlags::default())
            .unwrap();
        assert_eq!(vfs.readdir(NodeId::ROOT).unwrap().len(), before + 1);

        vfs.unlink(NodeId::ROOT, "extra.txt").unwrap();
        assert_eq!(vfs.readdir(NodeId::ROOT).unwrap().len(), before);
    }

    #[test]
    fn setattr_refreshes_cache_without_a_restat() {
        let (vfs, remote) = vfs_with(CacheMode::OnDemand, LONG_TTL);
        let file = vfs.lookup(NodeId::ROOT, "readme.txt").unwrap().node;
        vfs.setattr(
            file,
            SetAttributes {
                mode: Some(0o600),
                ..Default::default()
            },
        )
        .unwrap();
        let calls = remote.stat_calls();

        // setattr caches the server's returned attributes, so getattr is a hit.
        assert_eq!(vfs.getattr(file).unwrap().attributes.mode, 0o600);
        assert_eq!(remote.stat_calls(), calls);
    }

    #[test]
    fn rename_updates_both_directory_listings() {
        let vfs = vfs();
        vfs.readdir(NodeId::ROOT).unwrap(); // prime the listing cache
        vfs.rename(NodeId::ROOT, "readme.txt", NodeId::ROOT, "moved.txt")
            .unwrap();

        let names: Vec<_> = vfs
            .readdir(NodeId::ROOT)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.iter().any(|n| n == "moved.txt"));
        assert!(!names.iter().any(|n| n == "readme.txt"));
    }
}
