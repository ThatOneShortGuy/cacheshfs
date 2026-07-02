//! `cacheshfs` command-line entrypoint.
//!
//! Parses arguments, connects the SFTP transport, builds the shared
//! [`CacheVfs`] over it, and dispatches to the platform [`MountBackend`].

mod cli;

use std::process::ExitCode;
use std::sync::Arc;

use cacheshfs_core::{CacheVfs, MountBackend, RemotePath, VirtualFilesystem};
use cacheshfs_sftp::SftpBackend;
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

    // Validate the remote root and cache settings before opening a connection.
    let root = RemotePath::new(config.remote.root.clone())
        .map_err(|error| format!("invalid remote path: {error}"))?;
    let metadata_ttl = cli.metadata_ttl_duration()?;

    // Connect the SFTP transport.
    let options = cli.connect_options(&config.remote.target)?;
    let remote = SftpBackend::connect_with_options(options)
        .map_err(|error| format!("failed to connect to {}: {error}", config.remote.target))?;

    // The shared cache-backed VFS sits between the platform mount backend and
    // the remote transport.
    let filesystem: Arc<dyn VirtualFilesystem> = Arc::new(CacheVfs::new(
        Arc::new(remote),
        root,
        config.read_only,
        config.cache_mode,
        metadata_ttl,
    ));

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
        _config: cacheshfs_core::MountConfig,
        _filesystem: Arc<dyn VirtualFilesystem>,
    ) -> cacheshfs_core::Result<()> {
        Err(cacheshfs_core::Error::UnsupportedPlatform(
            "cacheshfs does not have a mount backend for this platform yet",
        ))
    }
}
