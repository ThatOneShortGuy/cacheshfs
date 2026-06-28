//! The shared cache-backed virtual filesystem.
//!
//! This is the layer platform adapters (`cacheshfs-linux`, `cacheshfs-windows`)
//! talk to. It owns the `NodeId` ⇄ remote-path mapping and the open-handle
//! table, and forwards operations to a [`RemoteFilesystem`].
//!
//! This implementation is **uncached write-through**: every metadata, read, and
//! write request goes straight to the remote. Writes are applied to the remote
//! immediately (there is no local buffering yet, so `flush` is a no-op and there
//! is no dirty state). Metadata and content caching layers will be added on top
//! of this structure. When the mount is read-only, mutating operations are
//! rejected with [`Error::PermissionDenied`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::{
    CreatedFile, DirectoryEntry, Error, FileHandle, FileMetadata, NodeId, OpenFlags,
    RemoteFilesystem, RemotePath, Result, SetAttributes, VirtualFilesystem,
};

/// State for one open file handle.
struct OpenFile {
    path: RemotePath,
}

/// Mutable registry shared behind a mutex.
struct State {
    next_node: u64,
    next_handle: u64,
    path_to_node: HashMap<RemotePath, NodeId>,
    node_to_path: HashMap<NodeId, RemotePath>,
    handles: HashMap<FileHandle, OpenFile>,
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

    /// Allocate a fresh file handle for `path`.
    fn open_handle(&mut self, path: RemotePath) -> FileHandle {
        let handle = FileHandle(self.next_handle);
        self.next_handle += 1;
        self.handles.insert(handle, OpenFile { path });
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
}

/// Cache-ready write-through virtual filesystem backed by a [`RemoteFilesystem`].
pub struct CacheVfs {
    remote: Arc<dyn RemoteFilesystem>,
    read_only: bool,
    state: Mutex<State>,
}

impl CacheVfs {
    /// Create a VFS rooted at `root` on `remote`. The root maps to
    /// [`NodeId::ROOT`]. When `read_only` is set, mutating operations are
    /// rejected.
    pub fn new(remote: Arc<dyn RemoteFilesystem>, root: RemotePath, read_only: bool) -> Self {
        let mut path_to_node = HashMap::new();
        let mut node_to_path = HashMap::new();
        path_to_node.insert(root.clone(), NodeId::ROOT);
        node_to_path.insert(NodeId::ROOT, root);

        CacheVfs {
            remote,
            read_only,
            state: Mutex::new(State {
                next_node: NodeId::ROOT.0 + 1,
                next_handle: 1,
                path_to_node,
                node_to_path,
                handles: HashMap::new(),
            }),
        }
    }

    /// Lock the state, recovering from a poisoned mutex rather than propagating
    /// a panic across the FFI boundary into a platform adapter.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
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
        // Resolve the child path under the lock, then hit the remote without
        // holding it.
        let child_path = self.lock().path_of(parent)?.join(name)?;
        let attributes = self.remote.stat(&child_path)?;
        let node = self.lock().intern(child_path);
        Ok(FileMetadata { node, attributes })
    }

    fn getattr(&self, node: NodeId) -> Result<FileMetadata> {
        let path = self.lock().path_of(node)?;
        let attributes = self.remote.stat(&path)?;
        Ok(FileMetadata { node, attributes })
    }

    fn readdir(&self, node: NodeId) -> Result<Vec<DirectoryEntry>> {
        let path = self.lock().path_of(node)?;
        let entries = self.remote.read_dir(&path)?;

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
            result.push(DirectoryEntry {
                name: entry.name,
                metadata: FileMetadata {
                    node: child,
                    attributes: entry.attributes,
                },
            });
        }
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
        }
        Ok(self.lock().open_handle(path))
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
        let path = {
            let state = self.lock();
            state
                .handles
                .get(&handle)
                .map(|file| file.path.clone())
                .ok_or_else(|| Error::InvalidInput("unknown file handle".to_string()))?
        };
        self.remote.write(&path, offset, data)
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

        let mut state = self.lock();
        let node = state.intern(child_path.clone());
        let handle = state.open_handle(child_path);
        Ok(CreatedFile {
            metadata: FileMetadata { node, attributes },
            handle,
        })
    }

    fn mkdir(&self, parent: NodeId, name: &str, mode: u32) -> Result<FileMetadata> {
        self.ensure_writable()?;
        let child_path = self.child_path(parent, name)?;
        let attributes = self.remote.mkdir(&child_path, mode)?;
        let node = self.lock().intern(child_path);
        Ok(FileMetadata { node, attributes })
    }

    fn unlink(&self, parent: NodeId, name: &str) -> Result<()> {
        self.ensure_writable()?;
        let child_path = self.child_path(parent, name)?;
        self.remote.unlink(&child_path)
    }

    fn rmdir(&self, parent: NodeId, name: &str) -> Result<()> {
        self.ensure_writable()?;
        let child_path = self.child_path(parent, name)?;
        self.remote.rmdir(&child_path)
    }

    fn rename(
        &self,
        parent: NodeId,
        name: &str,
        new_parent: NodeId,
        new_name: &str,
    ) -> Result<()> {
        self.ensure_writable()?;
        let from = self.child_path(parent, name)?;
        let to = self.child_path(new_parent, new_name)?;
        self.remote.rename(&from, &to)?;
        self.lock().rename_path(&from, &to);
        Ok(())
    }

    fn setattr(&self, node: NodeId, attributes: SetAttributes) -> Result<FileMetadata> {
        self.ensure_writable()?;
        let path = self.lock().path_of(node)?;
        let attributes = self.remote.setattr(&path, attributes)?;
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
                state: StdMutex::new(MockState { nodes, dirs }),
            }
        }
    }

    impl RemoteFilesystem for MockRemote {
        fn stat(&self, path: &RemotePath) -> Result<FileAttributes> {
            self.state
                .lock()
                .unwrap()
                .nodes
                .get(path.as_str())
                .map(|(attrs, _)| attrs.clone())
                .ok_or(Error::NotFound)
        }

        fn read_dir(&self, path: &RemotePath) -> Result<Vec<RemoteDirectoryEntry>> {
            let state = self.state.lock().unwrap();
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

    fn vfs() -> CacheVfs {
        CacheVfs::new(Arc::new(MockRemote::new()), RemotePath::root(), false)
    }

    fn read_only_vfs() -> CacheVfs {
        CacheVfs::new(Arc::new(MockRemote::new()), RemotePath::root(), true)
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
        let from_readdir = entries.iter().find(|e| e.name == "sub").unwrap().metadata.node;
        assert_eq!(from_lookup, from_readdir);
    }

    #[test]
    fn nested_lookup_and_read() {
        let vfs = vfs();
        let sub = vfs.lookup(NodeId::ROOT, "sub").unwrap().node;
        let file = vfs.lookup(sub, "a.txt").unwrap();
        assert_eq!(file.attributes.kind, FileKind::File);

        let handle = vfs.open(file.node, OpenFlags { read: true, ..Default::default() }).unwrap();
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
            .open(created.metadata.node, OpenFlags { read: true, ..Default::default() })
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
        vfs.open(file, OpenFlags { write: true, truncate: true, ..Default::default() })
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
        assert!(matches!(vfs.lookup(NodeId::ROOT, "d"), Err(Error::NotFound)));
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
            .open(file, OpenFlags { read: true, ..Default::default() })
            .unwrap();
        assert_eq!(vfs.read(handle, 0, 5).unwrap(), b"hello");

        // Every mutation is denied.
        assert!(matches!(
            vfs.open(file, OpenFlags { write: true, ..Default::default() }),
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
}
