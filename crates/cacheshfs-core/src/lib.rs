use std::path::PathBuf;
use std::sync::Arc;

mod store;
mod vfs;

pub use vfs::CacheVfs;

#[derive(Debug, Clone)]
pub struct MountConfig {
    pub remote: RemoteConfig,
    pub mountpoint: PathBuf,
    pub cache_dir: PathBuf,
    pub cache_mode: CacheMode,
    pub cache_chunk_size: u64,
    pub read_only: bool,
}

pub const DEFAULT_CACHE_CHUNK_SIZE: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct RemoteConfig {
    pub target: String,
    pub root: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    Remote,
    OnDemand,
    Pinned,
    Offline,
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    UnsupportedPlatform(&'static str),
    MountBackend(String),
    RemoteBackend(String),
    NotFound,
    AlreadyExists,
    PermissionDenied,
    InvalidInput(String),
    UnsupportedOperation(&'static str),
    Unavailable(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform(message) => f.write_str(message),
            Self::MountBackend(message) => f.write_str(message),
            Self::RemoteBackend(message) => f.write_str(message),
            Self::NotFound => f.write_str("not found"),
            Self::AlreadyExists => f.write_str("already exists"),
            Self::PermissionDenied => f.write_str("permission denied"),
            Self::InvalidInput(message) => f.write_str(message),
            Self::UnsupportedOperation(message) => f.write_str(message),
            Self::Unavailable(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

pub trait MountBackend {
    fn mount(&self, config: MountConfig, filesystem: Arc<dyn VirtualFilesystem>) -> Result<()>;
}

pub trait VirtualFilesystem: Send + Sync {
    fn lookup(&self, parent: NodeId, name: &str) -> Result<FileMetadata>;

    fn getattr(&self, node: NodeId) -> Result<FileMetadata>;

    fn readdir(&self, node: NodeId) -> Result<Vec<DirectoryEntry>>;

    fn open(&self, node: NodeId, flags: OpenFlags) -> Result<FileHandle>;

    fn read(&self, handle: FileHandle, offset: u64, size: u32) -> Result<Vec<u8>>;

    fn write(&self, handle: FileHandle, offset: u64, data: &[u8]) -> Result<u32>;

    fn flush(&self, handle: FileHandle) -> Result<()>;

    fn release(&self, handle: FileHandle) -> Result<()>;

    fn create(
        &self,
        parent: NodeId,
        name: &str,
        mode: u32,
        flags: OpenFlags,
    ) -> Result<CreatedFile>;

    fn mkdir(&self, parent: NodeId, name: &str, mode: u32) -> Result<FileMetadata>;

    fn unlink(&self, parent: NodeId, name: &str) -> Result<()>;

    fn rmdir(&self, parent: NodeId, name: &str) -> Result<()>;

    fn rename(&self, parent: NodeId, name: &str, new_parent: NodeId, new_name: &str) -> Result<()>;

    fn setattr(&self, node: NodeId, attributes: SetAttributes) -> Result<FileMetadata>;
}

pub trait RemoteFilesystem: Send + Sync {
    fn stat(&self, path: &RemotePath) -> Result<FileAttributes>;

    fn read_dir(&self, path: &RemotePath) -> Result<Vec<RemoteDirectoryEntry>>;

    fn read(&self, path: &RemotePath, offset: u64, size: u32) -> Result<Vec<u8>>;

    fn write(&self, path: &RemotePath, offset: u64, data: &[u8]) -> Result<u32>;

    fn create(&self, path: &RemotePath, mode: u32) -> Result<FileAttributes>;

    fn mkdir(&self, path: &RemotePath, mode: u32) -> Result<FileAttributes>;

    fn unlink(&self, path: &RemotePath) -> Result<()>;

    fn rmdir(&self, path: &RemotePath) -> Result<()>;

    fn rename(&self, from: &RemotePath, to: &RemotePath) -> Result<()>;

    fn setattr(&self, path: &RemotePath, attributes: SetAttributes) -> Result<FileAttributes>;
}

pub struct UnimplementedVirtualFilesystem;

impl VirtualFilesystem for UnimplementedVirtualFilesystem {
    fn lookup(&self, _parent: NodeId, _name: &str) -> Result<FileMetadata> {
        Err(unimplemented_vfs_error())
    }

    fn getattr(&self, _node: NodeId) -> Result<FileMetadata> {
        Err(unimplemented_vfs_error())
    }

    fn readdir(&self, _node: NodeId) -> Result<Vec<DirectoryEntry>> {
        Err(unimplemented_vfs_error())
    }

    fn open(&self, _node: NodeId, _flags: OpenFlags) -> Result<FileHandle> {
        Err(unimplemented_vfs_error())
    }

    fn read(&self, _handle: FileHandle, _offset: u64, _size: u32) -> Result<Vec<u8>> {
        Err(unimplemented_vfs_error())
    }

    fn write(&self, _handle: FileHandle, _offset: u64, _data: &[u8]) -> Result<u32> {
        Err(unimplemented_vfs_error())
    }

    fn flush(&self, _handle: FileHandle) -> Result<()> {
        Err(unimplemented_vfs_error())
    }

    fn release(&self, _handle: FileHandle) -> Result<()> {
        Err(unimplemented_vfs_error())
    }

    fn create(
        &self,
        _parent: NodeId,
        _name: &str,
        _mode: u32,
        _flags: OpenFlags,
    ) -> Result<CreatedFile> {
        Err(unimplemented_vfs_error())
    }

    fn mkdir(&self, _parent: NodeId, _name: &str, _mode: u32) -> Result<FileMetadata> {
        Err(unimplemented_vfs_error())
    }

    fn unlink(&self, _parent: NodeId, _name: &str) -> Result<()> {
        Err(unimplemented_vfs_error())
    }

    fn rmdir(&self, _parent: NodeId, _name: &str) -> Result<()> {
        Err(unimplemented_vfs_error())
    }

    fn rename(
        &self,
        _parent: NodeId,
        _name: &str,
        _new_parent: NodeId,
        _new_name: &str,
    ) -> Result<()> {
        Err(unimplemented_vfs_error())
    }

    fn setattr(&self, _node: NodeId, _attributes: SetAttributes) -> Result<FileMetadata> {
        Err(unimplemented_vfs_error())
    }
}

fn unimplemented_vfs_error() -> Error {
    Error::UnsupportedOperation("shared VFS is not implemented yet")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u64);

impl NodeId {
    pub const ROOT: Self = Self(1);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileHandle(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemotePath(String);

impl RemotePath {
    pub fn root() -> Self {
        Self("/".to_string())
    }

    pub fn new(path: impl Into<String>) -> Result<Self> {
        let path = path.into();

        if path.is_empty() || !path.starts_with('/') || path.split('/').any(|part| part == "..") {
            return Err(Error::InvalidInput(
                "remote paths must be absolute and must not contain '..'".to_string(),
            ));
        }

        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Append a single path component to this path, returning the child path.
    ///
    /// `name` must be a single component: empty names, `.`/`..`, and names
    /// containing a path separator are rejected so callers cannot escape the
    /// remote root through crafted lookup names.
    pub fn join(&self, name: &str) -> Result<RemotePath> {
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('/')
            || name.contains('\\')
        {
            return Err(Error::InvalidInput(format!(
                "'{name}' is not a valid path component"
            )));
        }

        let base = self.0.trim_end_matches('/');
        RemotePath::new(format!("{base}/{name}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub name: String,
    pub metadata: FileMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDirectoryEntry {
    pub name: String,
    pub attributes: FileAttributes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedFile {
    pub metadata: FileMetadata,
    pub handle: FileHandle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMetadata {
    pub node: NodeId,
    pub attributes: FileAttributes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAttributes {
    pub kind: FileKind,
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub modified_unix_seconds: Option<i64>,
    pub accessed_unix_seconds: Option<i64>,
    pub changed_unix_seconds: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenFlags {
    pub read: bool,
    pub write: bool,
    pub append: bool,
    pub truncate: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetAttributes {
    pub size: Option<u64>,
    pub mode: Option<u32>,
    pub modified_unix_seconds: Option<i64>,
    pub accessed_unix_seconds: Option<i64>,
}
