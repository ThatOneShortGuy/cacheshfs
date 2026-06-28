//! `cacheshfs` command-line entrypoint.
//!
//! Parses arguments, builds a [`MountConfig`], constructs the shared VFS, and
//! dispatches to the platform [`MountBackend`]. The cache-backed VFS is not yet
//! implemented, so for now an [`UnimplementedVirtualFilesystem`] placeholder is
//! handed to the backend.

mod cli;

use std::process::ExitCode;
use std::sync::Arc;

use cacheshfs_core::{MountBackend, MountConfig, UnimplementedVirtualFilesystem, VirtualFilesystem};
use clap::Parser;

use crate::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("cacheshfs: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let config = cli.to_mount_config()?;

    for option in cli.unwired_options() {
        eprintln!(
            "cacheshfs: warning: {option} is accepted but not yet applied in this build"
        );
    }

    mount(config)
}

fn mount(config: MountConfig) -> Result<(), String> {
    // TODO: replace with the real cache-backed VFS from cacheshfs-core once it
    // exists; until then the backend receives a placeholder that reports
    // unimplemented for every operation.
    let filesystem: Arc<dyn VirtualFilesystem> = Arc::new(UnimplementedVirtualFilesystem);
    platform_backend()
        .mount(config, filesystem)
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "linux")]
fn platform_backend() -> impl MountBackend {
    cacheshfs_linux::LinuxMountBackend
}

#[cfg(target_os = "windows")]
fn platform_backend() -> impl MountBackend {
    cacheshfs_windows::WindowsMountBackend
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn platform_backend() -> impl MountBackend {
    UnsupportedMountBackend
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
struct UnsupportedMountBackend;

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
impl MountBackend for UnsupportedMountBackend {
    fn mount(
        &self,
        _config: MountConfig,
        _filesystem: Arc<dyn VirtualFilesystem>,
    ) -> cacheshfs_core::Result<()> {
        Err(cacheshfs_core::Error::UnsupportedPlatform(
            "cacheshfs does not have a mount backend for this platform yet",
        ))
    }
}
