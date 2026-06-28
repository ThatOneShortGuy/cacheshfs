use cacheshfs_core::{Error, Result};

pub struct SftpBackend;

impl SftpBackend {
    pub fn connect(_target: &str) -> Result<Self> {
        Err(Error::RemoteBackend(
            "sftp backend is not implemented yet".to_string(),
        ))
    }
}
