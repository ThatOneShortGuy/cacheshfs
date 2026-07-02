//! Bridge between WinFsp's path-based callbacks and the inode/`NodeId`-based
//! [`VirtualFilesystem`] interface.
//!
//! WinFsp identifies files by a wide-string path such as `\a\b\c`, while the
//! shared VFS resolves a child by `(parent NodeId, name)` starting from
//! [`NodeId::ROOT`]. These helpers walk the path one component at a time.
//!
//! Windows filesystems are case-insensitive but the SFTP remote is
//! case-sensitive, so lookups fall back to a case-insensitive directory scan
//! when an exact-case match is missing. `resolve` returns the canonical
//! (real-case) path so the adapter can report it to WinFsp as the normalized
//! name; this keeps later operations (delete, rename, display) using the real
//! case that exists on the remote.

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

/// Look up `name` under `parent`, first by exact case, then case-insensitively.
///
/// Returns the child metadata and its *real* (case-preserved) name. The
/// case-insensitive comparison is ASCII-only, which covers essentially all real
/// file names; full Unicode case folding is a known simplification.
fn lookup_component(
    vfs: &dyn VirtualFilesystem,
    parent: NodeId,
    name: &str,
) -> Result<(FileMetadata, String)> {
    match vfs.lookup(parent, name) {
        Ok(metadata) => Ok((metadata, name.to_string())),
        Err(Error::NotFound) => {
            for entry in vfs.readdir(parent)? {
                if entry.name.eq_ignore_ascii_case(name) {
                    return Ok((entry.metadata, entry.name));
                }
            }
            Err(Error::NotFound)
        }
        Err(other) => Err(other),
    }
}

/// Resolve a full WinFsp path to its [`FileMetadata`] and canonical (real-case)
/// path, walking from the root. The root path (`\`) resolves to the root node.
pub fn resolve(vfs: &dyn VirtualFilesystem, file_name: &U16CStr) -> Result<(FileMetadata, String)> {
    let mut node = NodeId::ROOT;
    let mut metadata = vfs.getattr(NodeId::ROOT)?;
    let mut canonical = String::new();
    for component in components(file_name) {
        let (found, real_name) = lookup_component(vfs, node, &component)?;
        metadata = found;
        node = metadata.node;
        canonical.push('\\');
        canonical.push_str(&real_name);
    }
    if canonical.is_empty() {
        canonical.push('\\');
    }
    Ok((metadata, canonical))
}

/// Resolve the parent directory of a WinFsp path and return `(parent, leaf)`.
///
/// Parent components are matched case-insensitively; the leaf is returned as
/// given (callers use it either for a new name in `create` or, for operations
/// on an existing file, receive an already case-corrected normalized name from
/// WinFsp). Returns [`Error::InvalidInput`] for the root, which has no parent.
pub fn resolve_parent(vfs: &dyn VirtualFilesystem, file_name: &U16CStr) -> Result<(NodeId, String)> {
    let components = components(file_name);
    let (leaf, parents) = components
        .split_last()
        .ok_or_else(|| Error::InvalidInput("the root has no parent".to_string()))?;

    let mut node = NodeId::ROOT;
    for component in parents {
        let (metadata, _) = lookup_component(vfs, node, component)?;
        node = metadata.node;
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

        fn readdir(&self, node: NodeId) -> Result<Vec<DirectoryEntry>> {
            let children = match node.0 {
                1 => vec![("dir", meta(2, FileKind::Directory))],
                2 => vec![("file.txt", meta(3, FileKind::File))],
                _ => vec![],
            };
            Ok(children
                .into_iter()
                .map(|(name, metadata)| DirectoryEntry {
                    name: name.to_string(),
                    metadata,
                })
                .collect())
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
        assert_eq!(resolve(&vfs, w("\\").as_ucstr()).unwrap().0.node, NodeId(1));
        assert_eq!(
            resolve(&vfs, w("\\dir").as_ucstr()).unwrap().0.node,
            NodeId(2)
        );
        assert_eq!(
            resolve(&vfs, w("\\dir\\file.txt").as_ucstr()).unwrap().0.node,
            NodeId(3)
        );
    }

    #[test]
    fn resolve_returns_canonical_path() {
        let vfs = TestVfs;
        assert_eq!(resolve(&vfs, w("\\").as_ucstr()).unwrap().1, "\\");
        assert_eq!(
            resolve(&vfs, w("\\dir\\file.txt").as_ucstr()).unwrap().1,
            "\\dir\\file.txt"
        );
    }

    #[test]
    fn resolves_case_insensitively_with_real_case_canonical() {
        let vfs = TestVfs;
        // Wrong case at every component still resolves to the real node...
        let (metadata, canonical) = resolve(&vfs, w("\\DIR\\FILE.TXT").as_ucstr()).unwrap();
        assert_eq!(metadata.node, NodeId(3));
        // ...and reports the real case that exists on the remote.
        assert_eq!(canonical, "\\dir\\file.txt");
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
    fn resolve_parent_matches_parent_case_insensitively() {
        let vfs = TestVfs;
        // "\DIR\new.txt": parent "DIR" matches "dir"; the new leaf is kept as-is.
        let (parent, leaf) = resolve_parent(&vfs, w("\\DIR\\new.txt").as_ucstr()).unwrap();
        assert_eq!(parent, NodeId(2));
        assert_eq!(leaf, "new.txt");
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
