//! Command-line argument parsing and translation into a [`MountConfig`].

use std::env;
use std::path::{Path, PathBuf};

use cacheshfs_core::{CacheMode, MountConfig, RemoteConfig};
use cacheshfs_sftp::{SftpConnectOptions, SftpTarget};
use clap::{Parser, ValueEnum};

/// Mount a remote directory over SSH with an optional local cache.
#[derive(Debug, Parser)]
#[command(name = "cacheshfs", version, about, long_about = None)]
pub struct Cli {
    /// Remote SSH target and root path, in the form `[user@]host:/remote/path`.
    #[arg(value_name = "[USER@]HOST:/REMOTE/PATH")]
    pub remote: String,

    /// Local mountpoint (a drive letter or directory on Windows, a directory on
    /// Unix).
    #[arg(value_name = "MOUNTPOINT")]
    pub mountpoint: PathBuf,

    /// Directory for persistent cache state. Defaults to a per-user cache
    /// location. Must not live inside the mountpoint.
    #[arg(long, value_name = "PATH")]
    pub cache_dir: Option<PathBuf>,

    /// Cache mode.
    #[arg(long, value_name = "MODE", default_value = "on-demand")]
    pub cache_mode: CacheModeArg,

    /// Prevent all write operations.
    #[arg(long)]
    pub read_only: bool,

    // --- Options accepted but not yet wired into MountConfig. ---
    // These describe the SSH connection and cache policy. They round out the
    // documented interface and `--help`, but plumbing them through requires a
    // coordinated change to `cacheshfs-core`'s shared config types and the
    // SFTP transport, so for now providing one emits a warning.
    /// SSH port.
    #[arg(long, value_name = "PORT")]
    pub port: Option<u16>,

    /// Private key file.
    #[arg(long, value_name = "PATH")]
    pub identity_file: Option<PathBuf>,

    /// Connect even if the host key is not present in known_hosts. Insecure:
    /// this disables host-key verification for unknown hosts.
    #[arg(long)]
    pub accept_unknown_host_key: bool,

    /// SSH config file.
    #[arg(long, value_name = "PATH")]
    pub ssh_config: Option<PathBuf>,

    /// How long cached metadata is trusted (e.g. `30s`, `5m`).
    #[arg(long, value_name = "DURATION")]
    pub metadata_ttl: Option<String>,

    /// How long clean cached file contents are trusted before revalidation.
    #[arg(long, value_name = "DURATION")]
    pub content_ttl: Option<String>,

    /// Prefetch a remote path into the cache.
    #[arg(long, value_name = "PATH")]
    pub download: Option<String>,

    /// Pass through FUSE allow-other behavior when permitted by the system.
    #[arg(long)]
    pub allow_other: bool,
}

/// CLI spelling of [`CacheMode`]. Clap renders these as `remote`, `on-demand`,
/// `pinned`, `offline`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CacheModeArg {
    Remote,
    OnDemand,
    Pinned,
    Offline,
}

impl From<CacheModeArg> for CacheMode {
    fn from(value: CacheModeArg) -> Self {
        match value {
            CacheModeArg::Remote => CacheMode::Remote,
            CacheModeArg::OnDemand => CacheMode::OnDemand,
            CacheModeArg::Pinned => CacheMode::Pinned,
            CacheModeArg::Offline => CacheMode::Offline,
        }
    }
}

impl Cli {
    /// Names of the options that are parsed but not yet plumbed through, for
    /// which the user supplied a value. Used to warn rather than silently
    /// ignore them.
    pub fn unwired_options(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.ssh_config.is_some() {
            names.push("--ssh-config");
        }
        if self.metadata_ttl.is_some() {
            names.push("--metadata-ttl");
        }
        if self.content_ttl.is_some() {
            names.push("--content-ttl");
        }
        if self.download.is_some() {
            names.push("--download");
        }
        if self.allow_other {
            names.push("--allow-other");
        }
        names
    }

    /// Build a [`MountConfig`] from the parsed arguments.
    pub fn to_mount_config(&self) -> Result<MountConfig, String> {
        let remote = parse_remote(&self.remote)?;
        let cache_dir = self
            .cache_dir
            .clone()
            .unwrap_or_else(default_cache_dir);

        // Safety requirement: the cache directory must never live inside the
        // mounted filesystem tree, or cache writes would recurse through the
        // mount.
        if is_inside(&cache_dir, &self.mountpoint) {
            return Err(format!(
                "cache directory '{}' must not be inside the mountpoint '{}'",
                cache_dir.display(),
                self.mountpoint.display()
            ));
        }

        Ok(MountConfig {
            remote,
            mountpoint: self.mountpoint.clone(),
            cache_dir,
            cache_mode: self.cache_mode.into(),
            read_only: self.read_only,
        })
    }

    /// Build the SFTP connection options for `target` (the `[user@]host` part of
    /// the remote spec), applying the `--port`, `--identity-file`, and
    /// `--accept-unknown-host-key` flags on top of the transport defaults.
    pub fn connect_options(&self, target: &str) -> Result<SftpConnectOptions, String> {
        let mut sftp_target = SftpTarget::parse(target).map_err(|error| error.to_string())?;
        if let Some(port) = self.port {
            sftp_target.port = port;
        }

        let mut options = SftpConnectOptions::for_target(sftp_target);
        if let Some(identity_file) = &self.identity_file {
            options = options.with_identity_file(identity_file.clone());
        }
        if self.accept_unknown_host_key {
            options = options.accept_unknown_hosts(true);
        }
        Ok(options)
    }
}

/// Parse a `[user@]host:/remote/path` spec into a [`RemoteConfig`].
///
/// Splits on the first `:` so Windows remote roots that themselves contain a
/// drive colon (e.g. `host:/C:/Users/name`) are preserved in the root.
pub fn parse_remote(spec: &str) -> Result<RemoteConfig, String> {
    let (target, root) = spec.split_once(':').ok_or_else(|| {
        format!("invalid remote '{spec}': expected the form [user@]host:/remote/path")
    })?;

    if target.is_empty() {
        return Err(format!("invalid remote '{spec}': missing host before ':'"));
    }
    if root.is_empty() {
        return Err(format!("invalid remote '{spec}': missing remote path after ':'"));
    }

    Ok(RemoteConfig {
        target: target.to_string(),
        root: root.to_string(),
    })
}

/// Per-user default cache directory, derived from the environment.
fn default_cache_dir() -> PathBuf {
    if let Some(base) = env::var_os("LOCALAPPDATA") {
        return PathBuf::from(base).join("cacheshfs").join("cache");
    }
    if let Some(base) = env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(base).join("cacheshfs");
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".cache").join("cacheshfs");
    }
    PathBuf::from(".cacheshfs-cache")
}

/// Whether `path` is equal to or nested under `ancestor`, using a lexical
/// comparison of normalized forms (no filesystem access, since the mountpoint
/// may not exist yet).
fn is_inside(path: &Path, ancestor: &Path) -> bool {
    let path = normalize(path);
    let ancestor = normalize(ancestor);
    path.starts_with(&ancestor)
}

/// Collapse `.` components for a best-effort lexical comparison. `..` is left
/// intact (resolving it correctly needs the real filesystem).
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_user_host_path() {
        let remote = parse_remote("alice@example.com:/srv/data").unwrap();
        assert_eq!(remote.target, "alice@example.com");
        assert_eq!(remote.root, "/srv/data");
    }

    #[test]
    fn parses_host_without_user() {
        let remote = parse_remote("example.com:/srv").unwrap();
        assert_eq!(remote.target, "example.com");
        assert_eq!(remote.root, "/srv");
    }

    #[test]
    fn preserves_windows_drive_root() {
        // The first colon separates the host; the drive colon stays in the root.
        let remote = parse_remote("bob@winhost:/C:/Users/bob").unwrap();
        assert_eq!(remote.target, "bob@winhost");
        assert_eq!(remote.root, "/C:/Users/bob");
    }

    #[test]
    fn rejects_missing_colon() {
        assert!(parse_remote("example.com").is_err());
    }

    #[test]
    fn rejects_empty_sides() {
        assert!(parse_remote(":/srv").is_err());
        assert!(parse_remote("host:").is_err());
    }

    #[test]
    fn cache_mode_maps_to_core() {
        assert_eq!(CacheMode::from(CacheModeArg::OnDemand), CacheMode::OnDemand);
        assert_eq!(CacheMode::from(CacheModeArg::Offline), CacheMode::Offline);
    }

    #[test]
    fn rejects_cache_dir_inside_mountpoint() {
        let cli = Cli {
            remote: "host:/srv".to_string(),
            mountpoint: PathBuf::from("/mnt/remote"),
            cache_dir: Some(PathBuf::from("/mnt/remote/.cache")),
            cache_mode: CacheModeArg::OnDemand,
            read_only: false,
            port: None,
            identity_file: None,
            accept_unknown_host_key: false,
            ssh_config: None,
            metadata_ttl: None,
            content_ttl: None,
            download: None,
            allow_other: false,
        };
        assert!(cli.to_mount_config().is_err());
    }

    #[test]
    fn accepts_cache_dir_outside_mountpoint() {
        let cli = Cli {
            remote: "host:/srv".to_string(),
            mountpoint: PathBuf::from("/mnt/remote"),
            cache_dir: Some(PathBuf::from("/var/cache/cacheshfs")),
            cache_mode: CacheModeArg::OnDemand,
            read_only: true,
            port: None,
            identity_file: None,
            accept_unknown_host_key: false,
            ssh_config: None,
            metadata_ttl: None,
            content_ttl: None,
            download: None,
            allow_other: false,
        };
        let config = cli.to_mount_config().unwrap();
        assert!(config.read_only);
        assert_eq!(config.remote.root, "/srv");
    }
}

#[cfg(test)]
mod parse_tests {
    //! Tests for the `clap` argument-parsing surface, driven through
    //! [`Cli::try_parse_from`] exactly as the real command line would be.
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    #[test]
    fn applies_defaults() {
        let cli = parse(&["cacheshfs", "host:/srv", "/mnt"]).unwrap();
        assert_eq!(cli.remote, "host:/srv");
        assert_eq!(cli.mountpoint, PathBuf::from("/mnt"));
        // Defaults from the spec.
        assert_eq!(cli.cache_mode, CacheModeArg::OnDemand);
        assert!(!cli.read_only);
        assert!(cli.cache_dir.is_none());
        assert!(cli.port.is_none());
        assert!(!cli.allow_other);
    }

    #[test]
    fn parses_all_options() {
        let cli = parse(&[
            "cacheshfs",
            "alice@host:/srv/data",
            "/mnt/remote",
            "--cache-dir",
            "/var/cache/cacheshfs",
            "--cache-mode",
            "pinned",
            "--read-only",
            "--port",
            "2222",
            "--identity-file",
            "/home/alice/.ssh/id_ed25519",
            "--ssh-config",
            "/home/alice/.ssh/config",
            "--metadata-ttl",
            "30s",
            "--content-ttl",
            "5m",
            "--download",
            "/srv/data/big",
            "--allow-other",
        ])
        .unwrap();

        assert_eq!(cli.cache_dir, Some(PathBuf::from("/var/cache/cacheshfs")));
        assert_eq!(cli.cache_mode, CacheModeArg::Pinned);
        assert!(cli.read_only);
        assert_eq!(cli.port, Some(2222));
        assert_eq!(cli.metadata_ttl.as_deref(), Some("30s"));
        assert_eq!(cli.content_ttl.as_deref(), Some("5m"));
        assert_eq!(cli.download.as_deref(), Some("/srv/data/big"));
        assert!(cli.allow_other);
    }

    #[test]
    fn each_cache_mode_value_parses() {
        for (text, expected) in [
            ("remote", CacheModeArg::Remote),
            ("on-demand", CacheModeArg::OnDemand),
            ("pinned", CacheModeArg::Pinned),
            ("offline", CacheModeArg::Offline),
        ] {
            let cli = parse(&["cacheshfs", "host:/srv", "/mnt", "--cache-mode", text]).unwrap();
            assert_eq!(cli.cache_mode, expected, "mode {text}");
        }
    }

    #[test]
    fn rejects_unknown_cache_mode() {
        assert!(parse(&["cacheshfs", "host:/srv", "/mnt", "--cache-mode", "bogus"]).is_err());
    }

    #[test]
    fn rejects_non_numeric_port() {
        assert!(parse(&["cacheshfs", "host:/srv", "/mnt", "--port", "abc"]).is_err());
    }

    #[test]
    fn requires_both_positionals() {
        assert!(parse(&["cacheshfs"]).is_err());
        assert!(parse(&["cacheshfs", "host:/srv"]).is_err());
    }

    #[test]
    fn unwired_options_reports_only_supplied_flags() {
        let bare = parse(&["cacheshfs", "host:/srv", "/mnt"]).unwrap();
        assert!(bare.unwired_options().is_empty());

        let with_extras =
            parse(&["cacheshfs", "host:/srv", "/mnt", "--ssh-config", "/c", "--allow-other"])
                .unwrap();
        let reported = with_extras.unwired_options();
        assert!(reported.contains(&"--ssh-config"));
        assert!(reported.contains(&"--allow-other"));
        // --port and --identity-file are now wired into the SFTP connection.
        assert!(!reported.contains(&"--port"));
        assert!(!reported.contains(&"--identity-file"));
    }

    #[test]
    fn port_and_identity_are_not_reported_as_unwired() {
        let cli = parse(&[
            "cacheshfs",
            "host:/srv",
            "/mnt",
            "--port",
            "2222",
            "--identity-file",
            "/k/id",
        ])
        .unwrap();
        assert!(cli.unwired_options().is_empty());
    }

    #[test]
    fn connect_options_apply_port_and_identity() {
        let cli = parse(&[
            "cacheshfs",
            "alice@host:/srv",
            "/mnt",
            "--port",
            "2222",
            "--identity-file",
            "/keys/id_ed25519",
        ])
        .unwrap();
        let options = cli.connect_options("alice@host").unwrap();
        assert_eq!(options.target.username, "alice");
        assert_eq!(options.target.host, "host");
        assert_eq!(options.target.port, 2222);
        assert!(options.identity_files.contains(&PathBuf::from("/keys/id_ed25519")));
        // Secure default: unknown host keys are rejected unless opted in.
        assert!(!options.accept_unknown_hosts);
    }

    #[test]
    fn connect_options_accept_unknown_host_key() {
        let cli = parse(&[
            "cacheshfs",
            "alice@host:/srv",
            "/mnt",
            "--accept-unknown-host-key",
        ])
        .unwrap();
        let options = cli.connect_options("alice@host").unwrap();
        assert!(options.accept_unknown_hosts);
    }

    #[test]
    fn end_to_end_builds_mount_config() {
        let cli = parse(&[
            "cacheshfs",
            "bob@winhost:/C:/Users/bob",
            "/mnt/win",
            "--cache-dir",
            "/var/cache/cacheshfs",
            "--cache-mode",
            "offline",
        ])
        .unwrap();
        let config = cli.to_mount_config().unwrap();
        assert_eq!(config.remote.target, "bob@winhost");
        assert_eq!(config.remote.root, "/C:/Users/bob");
        assert_eq!(config.cache_mode, CacheMode::Offline);
        assert_eq!(config.cache_dir, PathBuf::from("/var/cache/cacheshfs"));
        assert!(!config.read_only);
    }
}
