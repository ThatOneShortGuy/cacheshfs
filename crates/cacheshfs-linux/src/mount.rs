use crate::fs::LinuxFilesystem;
use cacheshfs_core::{Error, MountConfig, Result, VirtualFilesystem};
use fuser::{Config, MountOption};
use std::sync::Arc;

pub fn mount(config: MountConfig, filesystem: Arc<dyn VirtualFilesystem>) -> Result<()> {
    let mut mount_options = vec![
        MountOption::FSName("cacheshfs".to_string()),
        MountOption::Subtype("cacheshfs".to_string()),
    ];

    if config.read_only {
        mount_options.push(MountOption::RO);
    }

    let mut options = Config::default();
    options.mount_options = mount_options;

    fuser::mount2(
        LinuxFilesystem::new(filesystem),
        &config.mountpoint,
        &options,
    )
    .map_err(|error| Error::MountBackend(error.to_string()))
}
