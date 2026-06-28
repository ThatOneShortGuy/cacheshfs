use cacheshfs_core::{Error, MountBackend, MountConfig, Result};

pub struct WindowsMountBackend;

impl MountBackend for WindowsMountBackend {
    fn mount(&self, _config: MountConfig) -> Result<()> {
        Err(Error::UnsupportedPlatform(
            "windows client support needs a filesystem backend such as WinFsp before mounting is available",
        ))
    }
}
