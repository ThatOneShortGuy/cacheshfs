//! The shared cache-backed virtual filesystem.
//!
//! This is the layer platform adapters (`cacheshfs-linux`, `cacheshfs-windows`)
//! talk to. It owns the `NodeId` ⇄ remote-path mapping and the open-handle
//! table, and forwards operations to a [`RemoteFilesystem`].
//!
//! This first implementation is **read-only and uncached**: every metadata and
//! read request goes straight to the remote. Metadata and content caching layers
//! will be added on top of this structure; mutating operations currently return
//! [`Error::UnsupportedOperation`].

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
}

/// Read-only, cache-ready virtual filesystem backed by a [`RemoteFilesystem`].
pub struct CacheVfs {
    remote: Arc<dyn RemoteFilesystem>,
    state: Mutex<State>,
}

impl CacheVfs {
    /// Create a VFS rooted at `root` on `remote`. The root maps to
    /// [`NodeId::ROOT`].
    pub fn new(remote: Arc<dyn RemoteFilesystem>, root: RemotePath) -> Self {
        let mut path_to_node = HashMap::new();
        let mut node_to_path = HashMap::new();
        path_to_node.insert(root.clone(), NodeId::ROOT);
        node_to_path.insert(NodeId::ROOT, root);

        CacheVfs {
            remote,
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
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Error returned for operations not supported by the read-only filesystem.
fn read_only() -> Error {
    Error::UnsupportedOperation("the filesystem is read-only in this build")
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
            return Err(read_only());
        }
        let mut state = self.lock();
        let path = state.path_of(node)?;
        Ok(state.open_handle(path))
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

    fn write(&self, _handle: FileHandle, _offset: u64, _data: &[u8]) -> Result<u32> {
        Err(read_only())
    }

    fn flush(&self, _handle: FileHandle) -> Result<()> {
        // Nothing is buffered locally yet, so flush is a no-op.
        Ok(())
    }

    fn release(&self, handle: FileHandle) -> Result<()> {
        self.lock().handles.remove(&handle);
        Ok(())
    }

    fn create(
        &self,
        _parent: NodeId,
        _name: &str,
        _mode: u32,
        _flags: OpenFlags,
    ) -> Result<CreatedFile> {
        Err(read_only())
    }

    fn mkdir(&self, _parent: NodeId, _name: &str, _mode: u32) -> Result<FileMetadata> {
        Err(read_only())
    }

    fn unlink(&self, _parent: NodeId, _name: &str) -> Result<()> {
        Err(read_only())
    }

    fn rmdir(&self, _parent: NodeId, _name: &str) -> Result<()> {
        Err(read_only())
    }

    fn rename(
        &self,
        _parent: NodeId,
        _name: &str,
        _new_parent: NodeId,
        _new_name: &str,
    ) -> Result<()> {
        Err(read_only())
    }

    fn setattr(&self, _node: NodeId, _attributes: SetAttributes) -> Result<FileMetadata> {
        Err(read_only())
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
    struct MockRemote {
        /// path -> (attributes, optional file contents)
        nodes: HashMap<String, (FileAttributes, Option<Vec<u8>>)>,
        /// dir path -> child names
        dirs: HashMap<String, Vec<String>>,
        /// records calls for assertions
        stat_calls: StdMutex<usize>,
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
                nodes,
                dirs,
                stat_calls: StdMutex::new(0),
            }
        }
    }

    impl RemoteFilesystem for MockRemote {
        fn stat(&self, path: &RemotePath) -> Result<FileAttributes> {
            *self.stat_calls.lock().unwrap() += 1;
            self.nodes
                .get(path.as_str())
                .map(|(attrs, _)| attrs.clone())
                .ok_or(Error::NotFound)
        }

        fn read_dir(&self, path: &RemotePath) -> Result<Vec<RemoteDirectoryEntry>> {
            let names = self.dirs.get(path.as_str()).ok_or(Error::NotFound)?;
            Ok(names
                .iter()
                .map(|name| {
                    let child = path.join(name).unwrap();
                    let (attrs, _) = self.nodes.get(child.as_str()).unwrap();
                    RemoteDirectoryEntry {
                        name: name.clone(),
                        attributes: attrs.clone(),
                    }
                })
                .collect())
        }

        fn read(&self, path: &RemotePath, offset: u64, size: u32) -> Result<Vec<u8>> {
            let (_, contents) = self.nodes.get(path.as_str()).ok_or(Error::NotFound)?;
            let contents = contents
                .as_ref()
                .ok_or(Error::InvalidInput("is a directory".into()))?;
            let start = (offset as usize).min(contents.len());
            let end = (start + size as usize).min(contents.len());
            Ok(contents[start..end].to_vec())
        }

        fn write(&self, _: &RemotePath, _: u64, _: &[u8]) -> Result<u32> {
            Err(Error::UnsupportedOperation("mock"))
        }
        fn create(&self, _: &RemotePath, _: u32) -> Result<FileAttributes> {
            Err(Error::UnsupportedOperation("mock"))
        }
        fn mkdir(&self, _: &RemotePath, _: u32) -> Result<FileAttributes> {
            Err(Error::UnsupportedOperation("mock"))
        }
        fn unlink(&self, _: &RemotePath) -> Result<()> {
            Err(Error::UnsupportedOperation("mock"))
        }
        fn rmdir(&self, _: &RemotePath) -> Result<()> {
            Err(Error::UnsupportedOperation("mock"))
        }
        fn rename(&self, _: &RemotePath, _: &RemotePath) -> Result<()> {
            Err(Error::UnsupportedOperation("mock"))
        }
        fn setattr(&self, _: &RemotePath, _: SetAttributes) -> Result<FileAttributes> {
            Err(Error::UnsupportedOperation("mock"))
        }
    }

    fn vfs() -> CacheVfs {
        CacheVfs::new(Arc::new(MockRemote::new()), RemotePath::root())
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
    fn opening_for_write_is_rejected() {
        let vfs = vfs();
        let file = vfs.lookup(NodeId::ROOT, "readme.txt").unwrap().node;
        let err = vfs
            .open(
                file,
                OpenFlags {
                    read: true,
                    write: true,
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, Error::UnsupportedOperation(_)));
    }

    #[test]
    fn mutating_operations_are_unsupported() {
        let vfs = vfs();
        assert!(matches!(
            vfs.mkdir(NodeId::ROOT, "x", 0o755),
            Err(Error::UnsupportedOperation(_))
        ));
        assert!(matches!(
            vfs.unlink(NodeId::ROOT, "readme.txt"),
            Err(Error::UnsupportedOperation(_))
        ));
        assert!(matches!(
            vfs.create(NodeId::ROOT, "x", 0o644, OpenFlags::default()),
            Err(Error::UnsupportedOperation(_))
        ));
        assert!(matches!(
            vfs.setattr(NodeId::ROOT, SetAttributes::default()),
            Err(Error::UnsupportedOperation(_))
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

    struct WritableRemote {
        nodes: StdMutex<HashMap<String, (FileAttributes, Option<Vec<u8>>)>>,
        dirs: StdMutex<HashMap<String, Vec<String>>>,
    }

    impl WritableRemote {
        fn new() -> Self {
            let mut nodes = HashMap::new();
            nodes.insert("/".to_string(), (dir_attrs(), None));
            nodes.insert(
                "/readme.txt".to_string(),
                (file_attrs(5), Some(b"hello".to_vec())),
            );

            let mut dirs = HashMap::new();
            dirs.insert("/".to_string(), vec!["readme.txt".to_string()]);

            Self {
                nodes: StdMutex::new(nodes),
                dirs: StdMutex::new(dirs),
            }
        }
    }

    impl RemoteFilesystem for WritableRemote {
        fn stat(&self, path: &RemotePath) -> Result<FileAttributes> {
            self.nodes
                .lock()
                .unwrap()
                .get(path.as_str())
                .map(|(attrs, _)| attrs.clone())
                .ok_or(Error::NotFound)
        }

        fn read_dir(&self, path: &RemotePath) -> Result<Vec<RemoteDirectoryEntry>> {
            let dirs = self.dirs.lock().unwrap();
            let nodes = self.nodes.lock().unwrap();
            let names = dirs.get(path.as_str()).ok_or(Error::NotFound)?;
            Ok(names
                .iter()
                .map(|name| {
                    let child = path.join(name).unwrap();
                    let (attrs, _) = nodes.get(child.as_str()).unwrap();
                    RemoteDirectoryEntry {
                        name: name.clone(),
                        attributes: attrs.clone(),
                    }
                })
                .collect())
        }

        fn read(&self, path: &RemotePath, offset: u64, size: u32) -> Result<Vec<u8>> {
            let nodes = self.nodes.lock().unwrap();
            let (_, contents) = nodes.get(path.as_str()).ok_or(Error::NotFound)?;
            let contents = contents
                .as_ref()
                .ok_or(Error::InvalidInput("is a directory".into()))?;
            let start = (offset as usize).min(contents.len());
            let end = (start + size as usize).min(contents.len());
            Ok(contents[start..end].to_vec())
        }

        fn write(&self, path: &RemotePath, offset: u64, data: &[u8]) -> Result<u32> {
            let mut nodes = self.nodes.lock().unwrap();
            let (attrs, contents) = nodes.get_mut(path.as_str()).ok_or(Error::NotFound)?;
            let contents = contents
                .as_mut()
                .ok_or(Error::InvalidInput("is a directory".into()))?;
            let start = offset as usize;
            if contents.len() < start {
                contents.resize(start, 0);
            }
            let end = start + data.len();
            if contents.len() < end {
                contents.resize(end, 0);
            }
            contents[start..end].copy_from_slice(data);
            attrs.size = contents.len() as u64;
            Ok(data.len() as u32)
        }

        fn create(&self, path: &RemotePath, mode: u32) -> Result<FileAttributes> {
            let name = path
                .as_str()
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| Error::InvalidInput("missing file name".to_string()))?;
            let parent = path
                .as_str()
                .rsplit_once('/')
                .map(|(parent, _)| if parent.is_empty() { "/" } else { parent })
                .unwrap_or("/");

            let attrs = FileAttributes {
                kind: FileKind::File,
                size: 0,
                mode,
                uid: 0,
                gid: 0,
                modified_unix_seconds: None,
                accessed_unix_seconds: None,
                changed_unix_seconds: None,
            };

            let mut nodes = self.nodes.lock().unwrap();
            if nodes.contains_key(path.as_str()) {
                return Err(Error::AlreadyExists);
            }
            nodes.insert(path.as_str().to_string(), (attrs.clone(), Some(Vec::new())));
            self.dirs
                .lock()
                .unwrap()
                .entry(parent.to_string())
                .or_default()
                .push(name.to_string());
            Ok(attrs)
        }

        fn mkdir(&self, _: &RemotePath, _: u32) -> Result<FileAttributes> {
            Err(Error::UnsupportedOperation("mock"))
        }
        fn unlink(&self, _: &RemotePath) -> Result<()> {
            Err(Error::UnsupportedOperation("mock"))
        }
        fn rmdir(&self, _: &RemotePath) -> Result<()> {
            Err(Error::UnsupportedOperation("mock"))
        }
        fn rename(&self, _: &RemotePath, _: &RemotePath) -> Result<()> {
            Err(Error::UnsupportedOperation("mock"))
        }
        fn setattr(&self, _: &RemotePath, _: SetAttributes) -> Result<FileAttributes> {
            Err(Error::UnsupportedOperation("mock"))
        }
    }

    fn writable_vfs() -> CacheVfs {
        CacheVfs::new(Arc::new(WritableRemote::new()), RemotePath::root())
    }

    #[test]
    #[ignore = "write support is not implemented in CacheVfs yet"]
    fn create_write_flush_release_and_read_round_trip() {
        let vfs = writable_vfs();
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
    #[ignore = "write support is not implemented in CacheVfs yet"]
    fn write_updates_existing_file_at_offset_and_refreshes_size() {
        let vfs = writable_vfs();
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
}
