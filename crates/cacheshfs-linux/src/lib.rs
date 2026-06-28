use cacheshfs_core::{Error, MountBackend, MountConfig, Result};

pub struct LinuxMountBackend;

impl MountBackend for LinuxMountBackend {
    fn mount(&self, _config: MountConfig) -> Result<()> {
        Err(Error::MountBackend(
            "linux fuser mount backend is not implemented yet".to_string(),
        ))
    }
}
