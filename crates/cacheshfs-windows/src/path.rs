//! Bridge between WinFsp's path-based callbacks and the inode/`NodeId`-based
//! [`VirtualFilesystem`] interface.
//!
//! WinFsp identifies files by a wide-string path such as `\a\b\c`, while the
//! shared VFS resolves a child by `(parent NodeId, name)` starting from
//! [`NodeId::ROOT`]. These helpers walk the path one component at a time.

use cacheshfs_core::{Error, FileMetadata, NodeId, Result, VirtualFilesystem};
use winfsp::U16CStr;

/// Split a WinFsp path into its non-empty components, accepting either Windows
/// (`\`) or forward-slash separators. The UTF-16 path is converted to UTF-8;
/// names not representable in UTF-8 are lossily converted (a known limitation).
fn components(file_name: &U16CStr) -> Vec<String> {
    file_name
        .to_string_lossy()
        .split(['\\', '/'])
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect()
}

/// Resolve a full WinFsp path to its [`FileMetadata`] by walking `lookup` from
/// the root. The root path (`\`) resolves to the root node's attributes.
pub fn resolve(vfs: &dyn VirtualFilesystem, file_name: &U16CStr) -> Result<FileMetadata> {
    let mut node = NodeId::ROOT;
    let mut metadata = vfs.getattr(NodeId::ROOT)?;
    for component in components(file_name) {
        metadata = vfs.lookup(node, &component)?;
        node = metadata.node;
    }
    Ok(metadata)
}

/// Resolve the parent directory of a WinFsp path and return `(parent, leaf)`.
///
/// Used by operations that act on a name within a directory (`create`,
/// `rename`, `unlink`/`rmdir` at cleanup time). Returns
/// [`Error::InvalidInput`] for the root, which has no parent.
pub fn resolve_parent(
    vfs: &dyn VirtualFilesystem,
    file_name: &U16CStr,
) -> Result<(NodeId, String)> {
    let components = components(file_name);
    let (leaf, parents) = components
        .split_last()
        .ok_or_else(|| Error::InvalidInput("the root has no parent".to_string()))?;

    let mut node = NodeId::ROOT;
    for component in parents {
        node = vfs.lookup(node, component)?.node;
    }
    Ok((node, leaf.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cacheshfs_core::{
        CreatedFile, DirectoryEntry, FileAttributes, FileHandle, FileKind, OpenFlags, SetAttributes,
    };
    use winfsp::U16CString;

    /// Fixed tree: `/`(1, dir) -> `dir`(2, dir) -> `file.txt`(3, file).
    struct TestVfs;

    fn attrs(kind: FileKind) -> FileAttributes {
        FileAttributes {
            kind,
            size: 0,
            mode: 0o644,
            uid: 0,
            gid: 0,
            modified_unix_seconds: None,
            accessed_unix_seconds: None,
            changed_unix_seconds: None,
        }
    }

    fn meta(node: u64, kind: FileKind) -> FileMetadata {
        FileMetadata {
            node: NodeId(node),
            attributes: attrs(kind),
        }
    }

    impl VirtualFilesystem for TestVfs {
        fn lookup(&self, parent: NodeId, name: &str) -> Result<FileMetadata> {
            match (parent.0, name) {
                (1, "dir") => Ok(meta(2, FileKind::Directory)),
                (2, "file.txt") => Ok(meta(3, FileKind::File)),
                _ => Err(Error::NotFound),
            }
        }

        fn getattr(&self, node: NodeId) -> Result<FileMetadata> {
            match node.0 {
                1 | 2 => Ok(meta(node.0, FileKind::Directory)),
                3 => Ok(meta(3, FileKind::File)),
                _ => Err(Error::NotFound),
            }
        }

        fn readdir(&self, _node: NodeId) -> Result<Vec<DirectoryEntry>> {
            Err(Error::UnsupportedOperation("test"))
        }
        fn open(&self, _node: NodeId, _flags: OpenFlags) -> Result<FileHandle> {
            Err(Error::UnsupportedOperation("test"))
        }
        fn read(&self, _h: FileHandle, _o: u64, _s: u32) -> Result<Vec<u8>> {
            Err(Error::UnsupportedOperation("test"))
        }
        fn write(&self, _h: FileHandle, _o: u64, _d: &[u8]) -> Result<u32> {
            Err(Error::UnsupportedOperation("test"))
        }
        fn flush(&self, _h: FileHandle) -> Result<()> {
            Err(Error::UnsupportedOperation("test"))
        }
        fn release(&self, _h: FileHandle) -> Result<()> {
            Err(Error::UnsupportedOperation("test"))
        }
        fn create(&self, _p: NodeId, _n: &str, _m: u32, _f: OpenFlags) -> Result<CreatedFile> {
            Err(Error::UnsupportedOperation("test"))
        }
        fn mkdir(&self, _p: NodeId, _n: &str, _m: u32) -> Result<FileMetadata> {
            Err(Error::UnsupportedOperation("test"))
        }
        fn unlink(&self, _p: NodeId, _n: &str) -> Result<()> {
            Err(Error::UnsupportedOperation("test"))
        }
        fn rmdir(&self, _p: NodeId, _n: &str) -> Result<()> {
            Err(Error::UnsupportedOperation("test"))
        }
        fn rename(&self, _p: NodeId, _n: &str, _np: NodeId, _nn: &str) -> Result<()> {
            Err(Error::UnsupportedOperation("test"))
        }
        fn setattr(&self, _node: NodeId, _a: SetAttributes) -> Result<FileMetadata> {
            Err(Error::UnsupportedOperation("test"))
        }
    }

    fn w(s: &str) -> U16CString {
        U16CString::from_str(s).unwrap()
    }

    #[test]
    fn resolves_root_and_nested_paths() {
        let vfs = TestVfs;
        assert_eq!(resolve(&vfs, w("\\").as_ucstr()).unwrap().node, NodeId(1));
        assert_eq!(resolve(&vfs, w("\\dir").as_ucstr()).unwrap().node, NodeId(2));
        assert_eq!(
            resolve(&vfs, w("\\dir\\file.txt").as_ucstr()).unwrap().node,
            NodeId(3)
        );
    }

    #[test]
    fn missing_component_is_not_found() {
        let vfs = TestVfs;
        assert!(matches!(
            resolve(&vfs, w("\\nope").as_ucstr()),
            Err(Error::NotFound)
        ));
    }

    #[test]
    fn resolve_parent_splits_leaf() {
        let vfs = TestVfs;
        let (parent, leaf) = resolve_parent(&vfs, w("\\dir\\file.txt").as_ucstr()).unwrap();
        assert_eq!(parent, NodeId(2));
        assert_eq!(leaf, "file.txt");
    }

    #[test]
    fn resolve_parent_of_root_is_invalid() {
        let vfs = TestVfs;
        assert!(matches!(
            resolve_parent(&vfs, w("\\").as_ucstr()),
            Err(Error::InvalidInput(_))
        ));
    }
}
