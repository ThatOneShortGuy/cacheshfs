use cacheshfs_core::{Error, MountBackend, MountConfig, Result, VirtualFilesystem};
use std::sync::Arc;

pub struct LinuxMountBackend;

impl MountBackend for LinuxMountBackend {
    fn mount(&self, _config: MountConfig, _filesystem: Arc<dyn VirtualFilesystem>) -> Result<()> {
        Err(Error::MountBackend(
            "linux fuser mount backend is not implemented yet".to_string(),
        ))
    }
}
