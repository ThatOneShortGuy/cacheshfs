use cacheshfs_core::{
    Error, FileAttributes, RemoteDirectoryEntry, RemoteFilesystem, RemotePath, Result,
    SetAttributes,
};

pub struct SftpBackend;

impl SftpBackend {
    pub fn connect(_target: &str) -> Result<Self> {
        Err(Error::RemoteBackend(
            "sftp backend is not implemented yet".to_string(),
        ))
    }
}

impl RemoteFilesystem for SftpBackend {
    fn stat(&self, _path: &RemotePath) -> Result<FileAttributes> {
        Err(not_implemented())
    }

    fn read_dir(&self, _path: &RemotePath) -> Result<Vec<RemoteDirectoryEntry>> {
        Err(not_implemented())
    }

    fn read(&self, _path: &RemotePath, _offset: u64, _size: u32) -> Result<Vec<u8>> {
        Err(not_implemented())
    }

    fn write(&self, _path: &RemotePath, _offset: u64, _data: &[u8]) -> Result<u32> {
        Err(not_implemented())
    }

    fn create(&self, _path: &RemotePath, _mode: u32) -> Result<FileAttributes> {
        Err(not_implemented())
    }

    fn mkdir(&self, _path: &RemotePath, _mode: u32) -> Result<FileAttributes> {
        Err(not_implemented())
    }

    fn unlink(&self, _path: &RemotePath) -> Result<()> {
        Err(not_implemented())
    }

    fn rmdir(&self, _path: &RemotePath) -> Result<()> {
        Err(not_implemented())
    }

    fn rename(&self, _from: &RemotePath, _to: &RemotePath) -> Result<()> {
        Err(not_implemented())
    }

    fn setattr(&self, _path: &RemotePath, _attributes: SetAttributes) -> Result<FileAttributes> {
        Err(not_implemented())
    }
}

fn not_implemented() -> Error {
    Error::RemoteBackend("sftp backend is not implemented yet".to_string())
}
