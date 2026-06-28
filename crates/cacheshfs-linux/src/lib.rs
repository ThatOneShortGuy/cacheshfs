//! Linux mount adapter for `cacheshfs`.
//!
//! Implements [`cacheshfs_core::MountBackend`] on top of `fuser`. The adapter
//! translates FUSE inode and handle callbacks into [`VirtualFilesystem`] calls
//! and maps shared-core errors to Linux errno values. SSH, cache policy, and
//! remote path normalization stay behind the VFS boundary.
//!
//! Everything `fuser`-specific is gated behind `#[cfg(target_os = "linux")]`;
//! on other platforms the backend compiles to a stub so the workspace still
//! builds everywhere.

use cacheshfs_core::{MountBackend, MountConfig, Result, VirtualFilesystem};
use std::sync::Arc;

#[cfg(target_os = "linux")]
mod attr;
#[cfg(target_os = "linux")]
mod error;
#[cfg(target_os = "linux")]
mod fs;
#[cfg(target_os = "linux")]
mod mount;

/// Mount backend that exposes the shared filesystem through FUSE on Linux.
pub struct LinuxMountBackend;

#[cfg(target_os = "linux")]
impl MountBackend for LinuxMountBackend {
    fn mount(&self, config: MountConfig, filesystem: Arc<dyn VirtualFilesystem>) -> Result<()> {
        mount::mount(config, filesystem)
    }
}

#[cfg(not(target_os = "linux"))]
impl MountBackend for LinuxMountBackend {
    fn mount(&self, _config: MountConfig, _filesystem: Arc<dyn VirtualFilesystem>) -> Result<()> {
        Err(cacheshfs_core::Error::UnsupportedPlatform(
            "the cacheshfs Linux backend must be built on Linux with FUSE support available",
        ))
    }
}
