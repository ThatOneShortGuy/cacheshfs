//! Windows mount adapter for `cacheshfs`.
//!
//! Implements [`cacheshfs_core::MountBackend`] on top of WinFsp. The adapter
//! translates WinFsp's path/handle callbacks into [`VirtualFilesystem`] calls
//! and maps shared-core errors to Windows `NTSTATUS` values. SSH, cache policy,
//! and remote path normalization stay out of this crate, behind the VFS.
//!
//! Everything WinFsp-specific is gated behind `#[cfg(windows)]`; on other
//! platforms the backend compiles to a stub that reports an unsupported
//! platform, so the workspace still builds everywhere.

use cacheshfs_core::{MountBackend, MountConfig, Result, VirtualFilesystem};
use std::sync::Arc;

#[cfg(windows)]
mod attr;
#[cfg(windows)]
mod error;
#[cfg(windows)]
mod fs;
#[cfg(windows)]
mod mount;
#[cfg(windows)]
mod path;

/// Mount backend that exposes the shared filesystem through WinFsp on Windows.
pub struct WindowsMountBackend;

#[cfg(windows)]
impl MountBackend for WindowsMountBackend {
    fn mount(&self, config: MountConfig, filesystem: Arc<dyn VirtualFilesystem>) -> Result<()> {
        mount::mount(config, filesystem)
    }
}

#[cfg(not(windows))]
impl MountBackend for WindowsMountBackend {
    fn mount(&self, _config: MountConfig, _filesystem: Arc<dyn VirtualFilesystem>) -> Result<()> {
        Err(cacheshfs_core::Error::UnsupportedPlatform(
            "the cacheshfs Windows backend must be built on Windows with the WinFsp runtime installed",
        ))
    }
}
