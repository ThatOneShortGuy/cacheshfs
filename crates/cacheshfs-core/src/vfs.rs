//! The shared cache-backed virtual filesystem.
//!
//! This is the layer platform adapters (`cacheshfs-linux`, `cacheshfs-windows`)
//! talk to. It owns the `NodeId` ⇄ remote-path mapping, the open-handle table,
//! and an in-memory metadata cache, and forwards operations to a
//! [`RemoteFilesystem`].
//!
//! **Metadata + content caching:** metadata (`getattr`/`lookup`/`readdir`) and
//! file content chunks are cached in a persistent, path-keyed [`Store`] under
//! `cache_dir`, so a previously cached tree survives a restart and can be served
//! offline. An in-memory per-session freshness map applies a TTL for online
//! revalidation and a short-TTL negative-lookup cache; in [`CacheMode::Offline`]
//! the store is served regardless of age and the remote is never contacted.
//! `Remote` mode reads straight through without caching content.
//!
//! Writes are write-through to the remote and evict the cached copy (`flush` is
//! a no-op, no dirty state yet) — so on reconnect the server is authoritative.
//! When the mount is read-only, mutating operations are rejected with
//! [`Error::PermissionDenied`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::store::Store;
use crate::{
    CacheMode, CreatedFile, DirectoryEntry, Error, FileAttributes, FileHandle, FileMetadata,
    NodeId, OpenFlags, RemoteDirectoryEntry, RemoteFilesystem, RemotePath, Result, SetAttributes,
    VirtualFilesystem,
};

/// Longest a negative (name-not-found) cache entry is trusted, capped so a file
/// created elsewhere becomes visible quickly.
const NEGATIVE_TTL_CAP: Duration = Duration::from_secs(1);

/// State for one open file handle.
struct OpenFile {
    node: NodeId,
    path: RemotePath,
}

/// Per-session registry and freshness tracking (the durable cache lives in the
/// `Store`). `validated` records when a path's metadata was last confirmed
/// against the remote (for the online TTL); `negative` caches recent
/// not-found lookups.
struct State {
    next_node: u64,
    next_handle: u64,
    path_to_node: HashMap<RemotePath, NodeId>,
    node_to_path: HashMap<NodeId, RemotePath>,
    handles: HashMap<FileHandle, OpenFile>,
    validated: HashMap<RemotePath, Instant>,
    negative: HashMap<(NodeId, String), Instant>,
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
    /// node identity survives the move.
    fn rename_path(&mut self, from: &RemotePath, to: &RemotePath) {
        if let Some(node) = self.path_to_node.remove(from) {
            self.path_to_node.insert(to.clone(), node);
            self.node_to_path.insert(node, to.clone());
        }
    }
}

/// Persistent, cache-backed write-through virtual filesystem over a
/// [`RemoteFilesystem`].
pub struct CacheVfs {
    remote: Arc<dyn RemoteFilesystem>,
    read_only: bool,
    cache_mode: CacheMode,
    metadata_ttl: Duration,
    negative_ttl: Duration,
    store: Store,
    state: Mutex<State>,
}

impl CacheVfs {
    /// Create a VFS rooted at `root` on `remote`. The root maps to
    /// [`NodeId::ROOT`]. `read_only` rejects mutations; `cache_mode` and
    /// `metadata_ttl` control metadata caching; cached file contents live under
    /// `cache_dir`.
    pub fn new(
        remote: Arc<dyn RemoteFilesystem>,
        root: RemotePath,
        read_only: bool,
        cache_mode: CacheMode,
        metadata_ttl: Duration,
        cache_dir: PathBuf,
        cache_chunk_size: u64,
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
            store: Store::new(cache_dir, cache_chunk_size),
            state: Mutex::new(State {
                next_node: NodeId::ROOT.0 + 1,
                next_handle: 1,
                path_to_node,
                node_to_path,
                handles: HashMap::new(),
                validated: HashMap::new(),
                negative: HashMap::new(),
            }),
        }
    }

    /// Create an offline VFS backed only by the persistent cache at `cache_dir`.
    /// No remote is contacted, so a previously cached tree can be browsed and
    /// read without any connection; uncached paths report not-found and mutations
    /// report unavailable.
    pub fn new_offline(
        root: RemotePath,
        read_only: bool,
        metadata_ttl: Duration,
        cache_dir: PathBuf,
        cache_chunk_size: u64,
    ) -> Self {
        Self::new(
            Arc::new(DisconnectedRemote),
            root,
            read_only,
            CacheMode::Offline,
            metadata_ttl,
            cache_dir,
            cache_chunk_size,
        )
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

    /// Whether the store's metadata for `path` is trustworthy without contacting
    /// the remote: always in offline mode, otherwise within the TTL of the last
    /// online validation.
    fn metadata_fresh(&self, path: &RemotePath) -> bool {
        if self.offline() {
            return true;
        }
        self.lock()
            .validated
            .get(path)
            .is_some_and(|at| at.elapsed() < self.metadata_ttl)
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

    /// Build a directory listing from the persistent store, or `None` if the
    /// directory's listing was never cached. Reads the store first (its lock)
    /// then interns nodes (the state lock) — never holding both at once.
    fn readdir_from_cache(&self, path: &RemotePath) -> Option<Vec<DirectoryEntry>> {
        let children = self.store.get_children(path)?;
        let mut collected = Vec::with_capacity(children.len());
        for name in &children {
            let child_path = path.join(name).ok()?;
            if let Some(attributes) = self.store.get_attrs(&child_path) {
                collected.push((name.clone(), child_path, attributes));
            }
        }
        let mut state = self.lock();
        Some(
            collected
                .into_iter()
                .map(|(name, child_path, attributes)| {
                    let child = state.intern(child_path);
                    DirectoryEntry {
                        name,
                        metadata: FileMetadata {
                            node: child,
                            attributes,
                        },
                    }
                })
                .collect(),
        )
    }
}

/// Whether an error means the remote could not be reached (as opposed to a
/// definitive answer like not-found or permission-denied). Online cache modes
/// fall back to cached data on these so downloaded files keep working when the
/// connection drops; a reachable server always stays authoritative.
fn is_unreachable(error: &Error) -> bool {
    matches!(error, Error::Unavailable(_))
}

/// A remote that is never reachable, used by [`CacheVfs::offline`]. In offline
/// mode the VFS serves entirely from the persistent cache and only reaches the
/// transport for mutations, which correctly fail as unavailable.
struct DisconnectedRemote;

fn disconnected() -> Error {
    Error::Unavailable("the mount is offline; the remote is not connected".to_string())
}

impl RemoteFilesystem for DisconnectedRemote {
    fn stat(&self, _: &RemotePath) -> Result<FileAttributes> {
        Err(disconnected())
    }
    fn read_dir(&self, _: &RemotePath) -> Result<Vec<RemoteDirectoryEntry>> {
        Err(disconnected())
    }
    fn read(&self, _: &RemotePath, _: u64, _: u32) -> Result<Vec<u8>> {
        Err(disconnected())
    }
    fn write(&self, _: &RemotePath, _: u64, _: &[u8]) -> Result<u32> {
        Err(disconnected())
    }
    fn create(&self, _: &RemotePath, _: u32) -> Result<FileAttributes> {
        Err(disconnected())
    }
    fn mkdir(&self, _: &RemotePath, _: u32) -> Result<FileAttributes> {
        Err(disconnected())
    }
    fn unlink(&self, _: &RemotePath) -> Result<()> {
        Err(disconnected())
    }
    fn rmdir(&self, _: &RemotePath) -> Result<()> {
        Err(disconnected())
    }
    fn rename(&self, _: &RemotePath, _: &RemotePath) -> Result<()> {
        Err(disconnected())
    }
    fn setattr(&self, _: &RemotePath, _: SetAttributes) -> Result<FileAttributes> {
        Err(disconnected())
    }
}

impl VirtualFilesystem for CacheVfs {
    fn lookup(&self, parent: NodeId, name: &str) -> Result<FileMetadata> {
        let child_path = self.child_path(parent, name)?;

        // Recent negative (not-found) result?
        {
            let state = self.lock();
            if let Some(at) = state.negative.get(&(parent, name.to_string()))
                && self.fresh_negative(*at)
            {
                return Err(Error::NotFound);
            }
        }

        // Fresh (or offline) cached metadata from the persistent store.
        if self.metadata_fresh(&child_path)
            && let Some(attributes) = self.store.get_attrs(&child_path)
        {
            let node = self.lock().intern(child_path);
            return Ok(FileMetadata { node, attributes });
        }
        if self.offline() {
            return Err(Error::NotFound);
        }

        match self.remote.stat(&child_path) {
            Ok(attributes) => {
                self.store.put_attrs(&child_path, &attributes);
                let node = {
                    let mut state = self.lock();
                    state.validated.insert(child_path.clone(), Instant::now());
                    state.negative.remove(&(parent, name.to_string()));
                    state.intern(child_path)
                };
                Ok(FileMetadata { node, attributes })
            }
            Err(Error::NotFound) => {
                self.store.remove(&child_path);
                self.lock()
                    .negative
                    .insert((parent, name.to_string()), Instant::now());
                Err(Error::NotFound)
            }
            // Server unreachable: fall back to a cached copy of the child so
            // previously seen entries resolve offline.
            Err(error) => match self.store.get_attrs(&child_path) {
                Some(attributes) if is_unreachable(&error) => {
                    let node = self.lock().intern(child_path);
                    Ok(FileMetadata { node, attributes })
                }
                _ => Err(error),
            },
        }
    }

    fn getattr(&self, node: NodeId) -> Result<FileMetadata> {
        let path = self.lock().path_of(node)?;
        if self.metadata_fresh(&path)
            && let Some(attributes) = self.store.get_attrs(&path)
        {
            return Ok(FileMetadata { node, attributes });
        }
        if self.offline() {
            return Err(Error::NotFound);
        }
        match self.remote.stat(&path) {
            Ok(attributes) => {
                self.store.put_attrs(&path, &attributes);
                self.lock().validated.insert(path, Instant::now());
                Ok(FileMetadata { node, attributes })
            }
            // The server no longer has it; drop the cached copy (server wins).
            Err(Error::NotFound) => {
                self.store.remove(&path);
                Err(Error::NotFound)
            }
            // Server unreachable: serve the cached attributes if we have them so
            // previously seen files keep working offline. Server-wins still
            // applies whenever the server is actually reachable.
            Err(error) => match self.store.get_attrs(&path) {
                Some(attributes) if is_unreachable(&error) => Ok(FileMetadata { node, attributes }),
                _ => Err(error),
            },
        }
    }

    fn readdir(&self, node: NodeId) -> Result<Vec<DirectoryEntry>> {
        let path = self.lock().path_of(node)?;

        if self.metadata_fresh(&path)
            && let Some(result) = self.readdir_from_cache(&path)
        {
            return Ok(result);
        }
        if self.offline() {
            return Err(Error::NotFound);
        }

        let entries = match self.remote.read_dir(&path) {
            Ok(entries) => entries,
            // Server unreachable: serve the cached listing if we have one so a
            // previously listed directory stays browsable offline.
            Err(error) => {
                if is_unreachable(&error)
                    && let Some(result) = self.readdir_from_cache(&path)
                {
                    return Ok(result);
                }
                return Err(error);
            }
        };
        let now = Instant::now();
        let mut children = Vec::with_capacity(entries.len());
        for entry in entries {
            // The transport may surface "." / ".."; the VFS exposes only real
            // children and lets the platform adapter synthesize the dot entries.
            if entry.name == "." || entry.name == ".." {
                continue;
            }
            children.push((entry.name, entry.attributes));
        }
        // Persist the listing and every child's attributes in a single write.
        self.store.record_listing(&path, children.clone());

        let mut state = self.lock();
        let mut result = Vec::with_capacity(children.len());
        for (name, attributes) in children {
            let child_path = path.join(&name)?;
            state.validated.insert(child_path.clone(), now);
            let child = state.intern(child_path);
            result.push(DirectoryEntry {
                name,
                metadata: FileMetadata {
                    node: child,
                    attributes,
                },
            });
        }
        state.validated.insert(path, now);
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
            self.store.invalidate_content(&path);
            self.lock().validated.remove(&path);
        }
        Ok(self.lock().open_handle(node, path))
    }

    fn read(&self, handle: FileHandle, offset: u64, size: u32) -> Result<Vec<u8>> {
        let (node, path) = {
            let state = self.lock();
            let file = state
                .handles
                .get(&handle)
                .ok_or_else(|| Error::InvalidInput("unknown file handle".to_string()))?;
            (file.node, file.path.clone())
        };
        match self.cache_mode {
            // Prefer direct remote access; do not populate the content cache.
            CacheMode::Remote => self.remote.read(&path, offset, size),
            // Serve only what is already hydrated; never contact the remote.
            CacheMode::Offline => self.store.read_cached(&path, offset, size),
            // Hydrate on first read (revalidating against current metadata) and
            // serve locally thereafter.
            CacheMode::OnDemand | CacheMode::Pinned => {
                let attributes = self.getattr(node)?.attributes;
                self.store
                    .read(&path, self.remote.as_ref(), &attributes, offset, size)
            }
        }
    }

    fn write(&self, handle: FileHandle, offset: u64, data: &[u8]) -> Result<u32> {
        self.ensure_writable()?;
        let path = {
            let state = self.lock();
            state
                .handles
                .get(&handle)
                .map(|file| file.path.clone())
                .ok_or_else(|| Error::InvalidInput("unknown file handle".to_string()))?
        };
        let written = self.remote.write(&path, offset, data)?;
        // The write changed the contents and size/mtime: drop the cached content
        // and force a metadata re-validation on the next getattr.
        self.store.invalidate_content(&path);
        self.lock().validated.remove(&path);
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
        let parent_path = self.lock().path_of(parent)?;
        let child_path = parent_path.join(name)?;
        let attributes = self.remote.create(&child_path, mode)?;

        self.store.put_attrs(&child_path, &attributes);
        self.store.invalidate_children(&parent_path);
        let (node, handle) = {
            let mut state = self.lock();
            state.validated.insert(child_path.clone(), Instant::now());
            state.negative.remove(&(parent, name.to_string()));
            let node = state.intern(child_path.clone());
            let handle = state.open_handle(node, child_path);
            (node, handle)
        };
        Ok(CreatedFile {
            metadata: FileMetadata { node, attributes },
            handle,
        })
    }

    fn mkdir(&self, parent: NodeId, name: &str, mode: u32) -> Result<FileMetadata> {
        self.ensure_writable()?;
        let parent_path = self.lock().path_of(parent)?;
        let child_path = parent_path.join(name)?;
        let attributes = self.remote.mkdir(&child_path, mode)?;

        self.store.put_attrs(&child_path, &attributes);
        self.store.invalidate_children(&parent_path);
        let node = {
            let mut state = self.lock();
            state.validated.insert(child_path.clone(), Instant::now());
            state.negative.remove(&(parent, name.to_string()));
            state.intern(child_path)
        };
        Ok(FileMetadata { node, attributes })
    }

    fn unlink(&self, parent: NodeId, name: &str) -> Result<()> {
        self.ensure_writable()?;
        let parent_path = self.lock().path_of(parent)?;
        let child_path = parent_path.join(name)?;
        self.remote.unlink(&child_path)?;

        self.store.remove(&child_path);
        self.store.invalidate_children(&parent_path);
        self.lock()
            .negative
            .insert((parent, name.to_string()), Instant::now());
        Ok(())
    }

    fn rmdir(&self, parent: NodeId, name: &str) -> Result<()> {
        self.ensure_writable()?;
        let parent_path = self.lock().path_of(parent)?;
        let child_path = parent_path.join(name)?;
        self.remote.rmdir(&child_path)?;

        self.store.remove(&child_path);
        self.store.invalidate_children(&parent_path);
        self.lock()
            .negative
            .insert((parent, name.to_string()), Instant::now());
        Ok(())
    }

    fn rename(&self, parent: NodeId, name: &str, new_parent: NodeId, new_name: &str) -> Result<()> {
        self.ensure_writable()?;
        let (from, to, from_parent, to_parent) = {
            let state = self.lock();
            let from_parent = state.path_of(parent)?;
            let to_parent = state.path_of(new_parent)?;
            (
                from_parent.join(name)?,
                to_parent.join(new_name)?,
                from_parent,
                to_parent,
            )
        };
        self.remote.rename(&from, &to)?;

        self.store.rename(&from, &to);
        self.store.invalidate_children(&from_parent);
        self.store.invalidate_children(&to_parent);
        let mut state = self.lock();
        state.rename_path(&from, &to);
        state.validated.remove(&from);
        state
            .negative
            .insert((parent, name.to_string()), Instant::now());
        state.negative.remove(&(new_parent, new_name.to_string()));
        Ok(())
    }

    fn setattr(&self, node: NodeId, attributes: SetAttributes) -> Result<FileMetadata> {
        self.ensure_writable()?;
        let path = self.lock().path_of(node)?;
        let attributes = self.remote.setattr(&path, attributes)?;
        // Store the fresh attributes (this also drops cached content when the
        // version changed) and mark the metadata validated.
        self.store.put_attrs(&path, &attributes);
        self.lock().validated.insert(path, Instant::now());
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
        read_calls: usize,
        /// when set, reads fail as if the connection dropped
        unreachable: bool,
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
                    read_calls: 0,
                    unreachable: false,
                }),
            }
        }

        /// Simulate the connection dropping: subsequent stat/read_dir/read calls
        /// fail with `Error::Unavailable`, as the SFTP transport reports.
        fn set_unreachable(&self, unreachable: bool) {
            self.state.lock().unwrap().unreachable = unreachable;
        }

        fn stat_calls(&self) -> usize {
            self.state.lock().unwrap().stat_calls
        }

        fn read_dir_calls(&self) -> usize {
            self.state.lock().unwrap().read_dir_calls
        }

        fn read_calls(&self) -> usize {
            self.state.lock().unwrap().read_calls
        }

        /// Replace a file's contents (simulating an external change), bumping its
        /// size and modification time so cache revalidation notices.
        fn set_file(&self, path: &str, contents: &[u8]) {
            let mut state = self.state.lock().unwrap();
            let (attrs, data) = state.nodes.get_mut(path).expect("file exists");
            attrs.size = contents.len() as u64;
            attrs.modified_unix_seconds = Some(attrs.modified_unix_seconds.unwrap_or(0) + 1);
            *data = Some(contents.to_vec());
        }
    }

    impl RemoteFilesystem for MockRemote {
        fn stat(&self, path: &RemotePath) -> Result<FileAttributes> {
            let mut state = self.state.lock().unwrap();
            state.stat_calls += 1;
            if state.unreachable {
                return Err(Error::Unavailable("connection lost".into()));
            }
            state
                .nodes
                .get(path.as_str())
                .map(|(attrs, _)| attrs.clone())
                .ok_or(Error::NotFound)
        }

        fn read_dir(&self, path: &RemotePath) -> Result<Vec<RemoteDirectoryEntry>> {
            let mut state = self.state.lock().unwrap();
            state.read_dir_calls += 1;
            if state.unreachable {
                return Err(Error::Unavailable("connection lost".into()));
            }
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
            let mut state = self.state.lock().unwrap();
            state.read_calls += 1;
            if state.unreachable {
                return Err(Error::Unavailable("connection lost".into()));
            }
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

    /// A fresh, unique cache directory per VFS instance (the store persists into
    /// it, so tests must not share one). Left on disk; the OS temp is reaped.
    fn fresh_cache_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cacheshfs-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Count content objects currently in a cache dir.
    fn objects_count(cache_dir: &std::path::Path) -> usize {
        std::fs::read_dir(cache_dir.join("objects"))
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    // These general helpers use `Remote` mode so reads pass through the mock
    // (no on-disk content cache), while metadata caching stays active. Content
    // caching has its own helper (`content_vfs`).
    fn vfs() -> CacheVfs {
        CacheVfs::new(
            Arc::new(MockRemote::new()),
            RemotePath::root(),
            false,
            CacheMode::Remote,
            LONG_TTL,
            fresh_cache_dir(),
            crate::DEFAULT_CACHE_CHUNK_SIZE,
        )
    }

    fn read_only_vfs() -> CacheVfs {
        CacheVfs::new(
            Arc::new(MockRemote::new()),
            RemotePath::root(),
            true,
            CacheMode::Remote,
            LONG_TTL,
            fresh_cache_dir(),
            crate::DEFAULT_CACHE_CHUNK_SIZE,
        )
    }

    /// A VFS plus a handle to its mock remote, for asserting on call counts.
    fn vfs_with(mode: CacheMode, ttl: Duration) -> (CacheVfs, Arc<MockRemote>) {
        let remote = Arc::new(MockRemote::new());
        let vfs = CacheVfs::new(
            remote.clone(),
            RemotePath::root(),
            false,
            mode,
            ttl,
            fresh_cache_dir(),
            crate::DEFAULT_CACHE_CHUNK_SIZE,
        );
        (vfs, remote)
    }

    /// A content-caching VFS backed by a real temp cache dir, plus the mock
    /// remote and the `TempDir` guard (held so the dir isn't cleaned up).
    fn content_vfs(
        mode: CacheMode,
        ttl: Duration,
    ) -> (CacheVfs, Arc<MockRemote>, tempfile::TempDir) {
        let remote = Arc::new(MockRemote::new());
        let dir = tempfile::tempdir().unwrap();
        let vfs = CacheVfs::new(
            remote.clone(),
            RemotePath::root(),
            false,
            mode,
            ttl,
            dir.path().to_path_buf(),
            crate::DEFAULT_CACHE_CHUNK_SIZE,
        );
        (vfs, remote, dir)
    }

    /// A remote that panics on any call — used to prove offline mode never
    /// contacts the server.
    struct PanicRemote;
    impl RemoteFilesystem for PanicRemote {
        fn stat(&self, _: &RemotePath) -> Result<FileAttributes> {
            unreachable!("offline mode must not contact the remote")
        }
        fn read_dir(&self, _: &RemotePath) -> Result<Vec<RemoteDirectoryEntry>> {
            unreachable!("offline mode must not contact the remote")
        }
        fn read(&self, _: &RemotePath, _: u64, _: u32) -> Result<Vec<u8>> {
            unreachable!("offline mode must not contact the remote")
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
        fn setattr(&self, _: &RemotePath, _: SetAttributes) -> Result<FileAttributes> {
            unreachable!()
        }
    }

    /// Open `name` under the root for reading and return (node, handle).
    fn open_for_read(vfs: &CacheVfs, name: &str) -> (NodeId, FileHandle) {
        let node = vfs.lookup(NodeId::ROOT, name).unwrap().node;
        let handle = vfs
            .open(
                node,
                OpenFlags {
                    read: true,
                    ..Default::default()
                },
            )
            .unwrap();
        (node, handle)
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
        assert_eq!(
            remote.stat_calls(),
            1,
            "second getattr should be a cache hit"
        );
    }

    #[test]
    fn getattr_refetches_after_write_invalidation() {
        let (vfs, remote) = vfs_with(CacheMode::OnDemand, LONG_TTL);
        let file = vfs.lookup(NodeId::ROOT, "readme.txt").unwrap().node;
        vfs.getattr(file).unwrap();
        let before = remote.stat_calls();

        let handle = vfs
            .open(
                file,
                OpenFlags {
                    write: true,
                    ..Default::default()
                },
            )
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
        assert_eq!(
            remote.read_dir_calls(),
            1,
            "second readdir should be cached"
        );
        // The listing primed the lookup/attr caches, so this needs no remote stat.
        vfs.lookup(NodeId::ROOT, "readme.txt").unwrap();
        assert_eq!(
            remote.stat_calls(),
            0,
            "lookup after readdir should be cached"
        );
    }

    #[test]
    fn repeated_lookup_is_cached() {
        let (vfs, remote) = vfs_with(CacheMode::OnDemand, LONG_TTL);
        vfs.lookup(NodeId::ROOT, "readme.txt").unwrap();
        vfs.lookup(NodeId::ROOT, "readme.txt").unwrap();
        assert_eq!(
            remote.stat_calls(),
            1,
            "second lookup should be a cache hit"
        );
    }

    #[test]
    fn negative_lookup_is_cached() {
        let (vfs, remote) = vfs_with(CacheMode::OnDemand, LONG_TTL);
        assert!(matches!(
            vfs.lookup(NodeId::ROOT, "nope"),
            Err(Error::NotFound)
        ));
        assert!(matches!(
            vfs.lookup(NodeId::ROOT, "nope"),
            Err(Error::NotFound)
        ));
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
            vfs.lookup(NodeId::ROOT, "fresh.txt")
                .unwrap()
                .attributes
                .kind,
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

    #[test]
    fn read_hydrates_then_serves_from_cache() {
        let (vfs, remote, dir) = content_vfs(CacheMode::OnDemand, LONG_TTL);
        let (node, handle) = open_for_read(&vfs, "readme.txt");

        assert_eq!(vfs.read(handle, 0, 1024).unwrap(), b"hello");
        // The whole file was hydrated to an on-disk object.
        let _ = node;
        assert_eq!(objects_count(dir.path()), 1);

        let reads = remote.read_calls();
        // Further reads (including partial ones) are served locally.
        assert_eq!(vfs.read(handle, 0, 2).unwrap(), b"he");
        assert_eq!(vfs.read(handle, 1, 3).unwrap(), b"ell");
        assert_eq!(
            remote.read_calls(),
            reads,
            "cached reads must not hit the remote"
        );
    }

    #[test]
    fn write_evicts_cached_content_and_next_read_rehydrates() {
        let (vfs, _remote, _dir) = content_vfs(CacheMode::OnDemand, LONG_TTL);
        let node = vfs.lookup(NodeId::ROOT, "readme.txt").unwrap().node;
        let handle = vfs
            .open(
                node,
                OpenFlags {
                    read: true,
                    write: true,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(vfs.read(handle, 0, 1024).unwrap(), b"hello");
        vfs.write(handle, 0, b"J").unwrap(); // -> "Jello"; evicts cached content
        // The next read re-hydrates from the remote, which now has the new data.
        assert_eq!(vfs.read(handle, 0, 1024).unwrap(), b"Jello");
    }

    #[test]
    fn remote_mode_does_not_cache_content() {
        let (vfs, _remote, dir) = content_vfs(CacheMode::Remote, LONG_TTL);
        let (_node, handle) = open_for_read(&vfs, "readme.txt");
        assert_eq!(vfs.read(handle, 0, 1024).unwrap(), b"hello");
        assert_eq!(
            objects_count(dir.path()),
            0,
            "remote mode reads straight through and must not hydrate"
        );
    }

    #[test]
    fn stale_content_is_rehydrated_after_external_change() {
        // A zero TTL makes getattr always observe fresh metadata, so the content
        // cache can detect the change and re-hydrate.
        let (vfs, remote, _dir) = content_vfs(CacheMode::OnDemand, Duration::ZERO);
        let (_node, handle) = open_for_read(&vfs, "readme.txt");
        assert_eq!(vfs.read(handle, 0, 1024).unwrap(), b"hello");

        // Something else changes the file on the remote (new size + mtime).
        remote.set_file("/readme.txt", b"goodbye!");
        assert_eq!(vfs.read(handle, 0, 1024).unwrap(), b"goodbye!");
    }

    #[test]
    fn cache_persists_across_restart_for_offline_reads() {
        let dir = tempfile::tempdir().unwrap();

        // Session 1: online, hydrate a file (persists metadata + content).
        {
            let vfs = CacheVfs::new(
                Arc::new(MockRemote::new()),
                RemotePath::root(),
                false,
                CacheMode::OnDemand,
                LONG_TTL,
                dir.path().to_path_buf(),
                crate::DEFAULT_CACHE_CHUNK_SIZE,
            );
            let (_node, handle) = open_for_read(&vfs, "readme.txt");
            assert_eq!(vfs.read(handle, 0, 1024).unwrap(), b"hello");
        }

        // Session 2: a brand-new offline mount over the same cache dir, with a
        // remote that panics if touched. The file is resolved and read purely
        // from the persisted cache.
        let vfs = CacheVfs::new(
            Arc::new(PanicRemote),
            RemotePath::root(),
            false,
            CacheMode::Offline,
            LONG_TTL,
            dir.path().to_path_buf(),
            crate::DEFAULT_CACHE_CHUNK_SIZE,
        );
        let node = vfs.lookup(NodeId::ROOT, "readme.txt").unwrap().node;
        let handle = vfs
            .open(
                node,
                OpenFlags {
                    read: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(vfs.read(handle, 0, 1024).unwrap(), b"hello");
    }

    #[test]
    fn offline_read_of_uncached_file_is_unavailable() {
        // Offline, empty cache: metadata for an unknown name is NotFound and a
        // hypothetical read has nothing to serve.
        let dir = tempfile::tempdir().unwrap();
        let vfs = CacheVfs::new(
            Arc::new(PanicRemote),
            RemotePath::root(),
            false,
            CacheMode::Offline,
            LONG_TTL,
            dir.path().to_path_buf(),
            crate::DEFAULT_CACHE_CHUNK_SIZE,
        );
        assert!(matches!(
            vfs.lookup(NodeId::ROOT, "readme.txt"),
            Err(Error::NotFound)
        ));
    }

    #[test]
    fn getattr_falls_back_to_cache_when_remote_unreachable() {
        // Zero TTL forces a revalidation on every getattr.
        let (vfs, remote, _dir) = content_vfs(CacheMode::OnDemand, Duration::ZERO);
        let node = vfs.lookup(NodeId::ROOT, "readme.txt").unwrap().node;
        let online = vfs.getattr(node).unwrap().attributes;

        // The connection drops; getattr serves the cached attributes instead of
        // surfacing the connection error.
        remote.set_unreachable(true);
        let offline = vfs.getattr(node).unwrap().attributes;
        assert_eq!(offline, online);
    }

    #[test]
    fn lookup_falls_back_to_cache_but_uncached_paths_surface_the_error() {
        let (vfs, remote, _dir) = content_vfs(CacheMode::OnDemand, Duration::ZERO);
        let online = vfs.lookup(NodeId::ROOT, "readme.txt").unwrap().attributes;

        remote.set_unreachable(true);
        // A previously seen child still resolves from the cache.
        let offline = vfs.lookup(NodeId::ROOT, "readme.txt").unwrap().attributes;
        assert_eq!(offline, online);
        // A never-seen child has nothing cached, so the error is surfaced.
        assert!(matches!(
            vfs.lookup(NodeId::ROOT, "never-seen.txt"),
            Err(Error::Unavailable(_))
        ));
    }

    #[test]
    fn readdir_falls_back_to_cache_when_remote_unreachable() {
        let (vfs, remote, _dir) = content_vfs(CacheMode::OnDemand, Duration::ZERO);
        let names =
            |entries: &[DirectoryEntry]| entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        let online = names(&vfs.readdir(NodeId::ROOT).unwrap());

        remote.set_unreachable(true);
        let offline = names(&vfs.readdir(NodeId::ROOT).unwrap());
        assert_eq!(offline, online);
    }

    #[test]
    fn read_serves_cached_content_when_remote_unreachable() {
        // Zero TTL forces the read path's getattr to revalidate every time.
        let (vfs, remote, _dir) = content_vfs(CacheMode::OnDemand, Duration::ZERO);
        let (_node, handle) = open_for_read(&vfs, "readme.txt");
        // First read hydrates from the server.
        assert_eq!(vfs.read(handle, 0, 1024).unwrap(), b"hello");

        // Connection drops: the already-downloaded content is still served.
        remote.set_unreachable(true);
        assert_eq!(vfs.read(handle, 0, 1024).unwrap(), b"hello");
    }
}
