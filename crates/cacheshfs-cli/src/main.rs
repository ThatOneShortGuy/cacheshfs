use cacheshfs_core::{MountBackend, MountConfig, Result};

fn main() {
    println!("cacheshfs workspace is ready; CLI parsing is not implemented yet");
}

#[allow(dead_code)]
fn mount(config: MountConfig) -> Result<()> {
    platform_backend().mount(config)
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
    fn mount(&self, _config: MountConfig) -> Result<()> {
        Err(cacheshfs_core::Error::UnsupportedPlatform(
            "cacheshfs does not have a mount backend for this platform yet",
        ))
    }
}
