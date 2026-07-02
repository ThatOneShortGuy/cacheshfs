//! Mounting entry point: initialize WinFsp, build the volume, and run the
//! dispatcher.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cacheshfs_core::{Error, MountConfig, Result, VirtualFilesystem};
use winfsp::host::{FileSystemHost, VolumeParams};
use winfsp::winfsp_init;

use crate::fs::CacheFs;

/// Mount the shared filesystem at `config.mountpoint` using WinFsp.
///
/// This blocks for the lifetime of the mount, mirroring the blocking `fuser`
/// mount on Linux. WinFsp services callbacks on its own dispatcher threads.
pub fn mount(config: MountConfig, filesystem: Arc<dyn VirtualFilesystem>) -> Result<()> {
    // Loads the WinFsp runtime DLL. Fails cleanly if WinFsp is not installed.
    let _init = winfsp_init().map_err(|e| {
        Error::Unavailable(format!(
            "could not initialize WinFsp (is the WinFsp runtime installed?): {e}"
        ))
    })?;

    let mut volume_params = VolumeParams::new();
    volume_params
        .filesystem_name("cacheshfs")
        .sector_size(4096)
        .sectors_per_allocation_unit(1)
        // A non-MAX timeout keeps WinFsp re-querying us so cache invalidation in
        // the core layer stays authoritative.
        .file_info_timeout(1000)
        // Windows tools expect case-insensitive, case-preserving behavior.
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(false)
        .post_cleanup_when_modified_only(true)
        .read_only_volume(config.read_only);

    let context = CacheFs::new(filesystem);

    // The explicit type pins the default `FineGuard` locking strategy so the
    // later `start()` call is unambiguous.
    let mut host: FileSystemHost<CacheFs> = FileSystemHost::new(volume_params, context)
        .map_err(|e| Error::MountBackend(format!("failed to create WinFsp host: {e}")))?;

    host.mount(&config.mountpoint).map_err(|e| {
        Error::MountBackend(format!(
            "failed to set mount point '{}': {e}",
            config.mountpoint.display()
        ))
    })?;

    host.start()
        .map_err(|e| Error::MountBackend(format!("failed to start WinFsp dispatcher: {e}")))?;

    // `start` returns immediately; the dispatcher runs on its own threads. Wait
    // here until Ctrl-C / termination flips the flag, then fall out of the
    // function so `host` drops — its Drop calls `unmount()` + `stop()`, tearing
    // the mount down gracefully instead of leaving the process to be killed.
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        ctrlc::set_handler(move || shutdown.store(true, Ordering::SeqCst))
            .map_err(|e| Error::MountBackend(format!("failed to install signal handler: {e}")))?;
    }

    while !shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(150));
    }

    eprintln!("cacheshfs: unmounting {}", config.mountpoint.display());
    Ok(())
}
