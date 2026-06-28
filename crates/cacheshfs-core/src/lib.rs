use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct MountConfig {
    pub remote: RemoteConfig,
    pub mountpoint: PathBuf,
    pub cache_dir: PathBuf,
    pub cache_mode: CacheMode,
    pub read_only: bool,
}

#[derive(Debug, Clone)]
pub struct RemoteConfig {
    pub target: String,
    pub root: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    Remote,
    OnDemand,
    Pinned,
    Offline,
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    UnsupportedPlatform(&'static str),
    MountBackend(String),
    RemoteBackend(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform(message) => f.write_str(message),
            Self::MountBackend(message) => f.write_str(message),
            Self::RemoteBackend(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

pub trait MountBackend {
    fn mount(&self, config: MountConfig) -> Result<()>;
}
