use cacheshfs_core::{Error, MountBackend, MountConfig, Result, VirtualFilesystem};
use std::sync::Arc;

pub struct WindowsMountBackend;

impl MountBackend for WindowsMountBackend {
    fn mount(&self, _config: MountConfig, _filesystem: Arc<dyn VirtualFilesystem>) -> Result<()> {
        Err(Error::UnsupportedPlatform(
            "windows client support needs a filesystem backend such as WinFsp before mounting is available",
        ))
    }
}
