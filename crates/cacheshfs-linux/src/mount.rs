use crate::attr::LinuxOwner;
use crate::fs::LinuxFilesystem;
use cacheshfs_core::{Error, MountConfig, Result, VirtualFilesystem};
use fuser::{Config, MountOption};
use std::sync::Arc;

pub fn mount(config: MountConfig, filesystem: Arc<dyn VirtualFilesystem>) -> Result<()> {
    // FUSE requires the mountpoint to already exist as a directory. Check it up
    // front so the failure is a clear message rather than a bare ENOENT from the
    // mount syscall.
    match std::fs::metadata(&config.mountpoint) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(Error::InvalidInput(format!(
                "mountpoint '{}' is not a directory",
                config.mountpoint.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::InvalidInput(format!(
                "mountpoint '{0}' does not exist; create it first (e.g. `mkdir -p {0}`)",
                config.mountpoint.display()
            )));
        }
        Err(error) => {
            return Err(Error::MountBackend(format!(
                "cannot access mountpoint '{}': {error}",
                config.mountpoint.display()
            )));
        }
    }

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
        LinuxFilesystem::new(filesystem, mount_owner()),
        &config.mountpoint,
        &options,
    )
    .map_err(|error| Error::MountBackend(error.to_string()))
}

fn mount_owner() -> LinuxOwner {
    LinuxOwner {
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_owner_uses_effective_process_identity() {
        let owner = mount_owner();

        assert_eq!(owner.uid, unsafe { libc::geteuid() });
        assert_eq!(owner.gid, unsafe { libc::getegid() });
    }
}
