use crate::attr::LinuxOwner;
use crate::fs::LinuxFilesystem;
use cacheshfs_core::{Error, MountConfig, Result, VirtualFilesystem};
use fuser::{Config, MountOption};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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

    // Run the FUSE session on a background thread so this thread stays free to
    // wait for a shutdown signal and unmount cleanly. Blocking in `mount2`
    // instead would mean a Ctrl-C kills the process without unmounting, leaving
    // a stale "Transport endpoint is not connected" mountpoint behind.
    let session = fuser::spawn_mount2(
        LinuxFilesystem::new(filesystem, mount_owner()),
        &config.mountpoint,
        &options,
    )
    .map_err(|error| Error::MountBackend(error.to_string()))?;

    // SIGINT (Ctrl-C) / SIGTERM flip this flag; the wait loop below then
    // triggers a clean unmount.
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        ctrlc::set_handler(move || shutdown.store(true, Ordering::SeqCst)).map_err(|error| {
            Error::MountBackend(format!("failed to install signal handler: {error}"))
        })?;
    }

    // Wait until we are asked to stop, or the session ends on its own (e.g. an
    // external `fusermount -u`).
    while !shutdown.load(Ordering::SeqCst) && !session.guard.is_finished() {
        std::thread::sleep(Duration::from_millis(150));
    }

    if session.guard.is_finished() {
        // The mount was already torn down elsewhere; just join the thread.
        session
            .join()
            .map_err(|error| Error::MountBackend(error.to_string()))
    } else {
        // A signal arrived: unmount cleanly and wait for the session thread.
        eprintln!("cacheshfs: unmounting {}", config.mountpoint.display());
        session
            .umount_and_join()
            .map_err(|error| Error::MountBackend(error.to_string()))
    }
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
