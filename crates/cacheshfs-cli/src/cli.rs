//! Command-line argument parsing and translation into a [`MountConfig`].

use std::env;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cacheshfs_core::{CacheMode, DEFAULT_CACHE_CHUNK_SIZE, MountConfig, RemoteConfig};
use cacheshfs_sftp::{SftpConnectOptions, SftpTarget};
use clap::{Parser, ValueEnum};

use crate::ssh_config;

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

    /// Content cache chunk size. Accepts bytes or K/M/G suffixes, e.g. 4M.
    #[arg(long, value_name = "SIZE")]
    pub cache_chunk_size: Option<String>,

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

    /// Blindly trust an unknown host key without the interactive prompt.
    /// Insecure: skips trust-on-first-use confirmation. By default an unknown
    /// host prompts for confirmation (and a changed key is always rejected).
    #[arg(long)]
    pub accept_unknown_host_key: bool,

    /// OpenSSH client config to resolve the host alias against (its `HostName`,
    /// `User`, `Port`, and `IdentityFile`). Defaults to `~/.ssh/config` if that
    /// file exists; explicit command-line values still take precedence.
    #[arg(long, value_name = "PATH")]
    pub ssh_config: Option<PathBuf>,

    /// How long cached metadata is trusted (e.g. `30s`, `5m`).
    #[arg(long, value_name = "DURATION")]
    pub metadata_ttl: Option<String>,

    /// How long clean cached file contents are trusted before revalidation.
    /// [not yet implemented]
    #[arg(long, value_name = "DURATION")]
    pub content_ttl: Option<String>,

    /// Prefetch a remote path into the cache. [not yet implemented]
    #[arg(long, value_name = "PATH")]
    pub download: Option<String>,

    /// Pass through FUSE allow-other behavior when permitted by the system.
    /// [not yet implemented]
    #[arg(long)]
    pub allow_other: bool,
}

/// CLI spelling of [`CacheMode`]. Clap renders these as `remote`, `on-demand`,
/// `pinned`, `offline`. The per-variant doc comments below become the value
/// descriptions shown in the long `--help` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CacheModeArg {
    /// Pass-through: every read and stat goes straight to the server and
    /// nothing is written to the local content cache. Lowest memory/disk use,
    /// no offline reads, and no speedup on repeated access.
    Remote,
    /// Cache file chunks and metadata as they are accessed. The first read of a
    /// chunk downloads and stores it; later reads are served locally until the
    /// server copy changes (revalidated by size/mtime) or the file is written.
    /// If the server becomes unreachable, already-cached chunks and listings
    /// keep being served instead of erroring. Best general-purpose mode.
    OnDemand,
    /// Like on-demand today (a distinct keep-resident/prefetch policy is not yet
    /// implemented, so it currently behaves the same as `on-demand`).
    Pinned,
    /// Serve entirely from the persistent cache and never open a connection.
    /// Previously cached files and directories are readable offline; uncached
    /// paths report not-found and writes are rejected.
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

        let cache_chunk_size = match &self.cache_chunk_size {
            Some(text) => parse_byte_size(text)?,
            None => DEFAULT_CACHE_CHUNK_SIZE,
        };

        Ok(MountConfig {
            remote,
            mountpoint: self.mountpoint.clone(),
            cache_dir,
            cache_mode: self.cache_mode.into(),
            cache_chunk_size,
            read_only: self.read_only,
        })
    }

    /// The metadata cache TTL, from `--metadata-ttl` or a default.
    pub fn metadata_ttl_duration(&self) -> Result<Duration, String> {
        match &self.metadata_ttl {
            Some(text) => parse_duration(text),
            None => Ok(DEFAULT_METADATA_TTL),
        }
    }

    /// Build the SFTP connection options for `target` (the `[user@]host` part of
    /// the remote spec).
    ///
    /// The matching `~/.ssh/config` block (or `--ssh-config` file) supplies
    /// defaults for the host alias — `HostName`, `User`, `Port`, and
    /// `IdentityFile` — and the `--port`, `--identity-file`, and an explicit
    /// `user@` in the target override them, matching OpenSSH precedence.
    pub fn connect_options(&self, target: &str) -> Result<SftpConnectOptions, String> {
        // The alias is the host as written on the command line, before ssh
        // config rewrites it; an explicit `user@` pins the username.
        let (explicit_user, alias) = match target.rsplit_once('@') {
            Some((user, host)) if !user.is_empty() => (Some(user), host),
            _ => (None, target),
        };
        let host_config = self.resolve_ssh_config(alias)?;

        let mut sftp_target = SftpTarget::parse(target).map_err(|error| error.to_string())?;
        // HostName from ssh config points the alias at a real host.
        if let Some(hostname) = &host_config.hostname {
            sftp_target.host = hostname.clone();
        }
        // ssh config User applies only when the target did not pin one.
        if explicit_user.is_none()
            && let Some(user) = &host_config.user
        {
            sftp_target.username = user.clone();
        }
        // Port precedence: --port, then ssh config Port, then the default.
        if let Some(port) = self.port {
            sftp_target.port = port;
        } else if let Some(port) = host_config.port {
            sftp_target.port = port;
        }

        let mut options = SftpConnectOptions::for_target(sftp_target);
        // Identity precedence: --identity-file first, then any from ssh config.
        if let Some(identity_file) = &self.identity_file {
            options = options.with_identity_file(identity_file.clone());
        }
        for identity_file in host_config.identity_files {
            options = options.with_identity_file(identity_file);
        }
        if self.accept_unknown_host_key {
            options = options.accept_unknown_hosts(true);
        }
        Ok(options)
    }

    /// Resolve the ssh-config settings for `alias`. Reads `--ssh-config` when
    /// given (a missing file is an error), otherwise `~/.ssh/config` when it
    /// exists (a missing default file is silently ignored).
    fn resolve_ssh_config(&self, alias: &str) -> Result<ssh_config::SshHostConfig, String> {
        let (path, required) = match &self.ssh_config {
            Some(path) => (path.clone(), true),
            None => match ssh_config::default_config_path() {
                Some(path) => (path, false),
                None => return Ok(ssh_config::SshHostConfig::default()),
            },
        };

        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(ssh_config::resolve(&text, alias)),
            Err(error) if error.kind() == ErrorKind::NotFound && !required => {
                Ok(ssh_config::SshHostConfig::default())
            }
            Err(error) => Err(format!(
                "failed to read ssh config '{}': {error}",
                path.display()
            )),
        }
    }
}

/// Default metadata cache TTL when `--metadata-ttl` is not given.
const DEFAULT_METADATA_TTL: Duration = Duration::from_secs(5);

/// Parse a duration like `500ms`, `30s`, `5m`, `1h`, or a bare number of
/// seconds.
fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let invalid = || format!("invalid duration '{text}' (use e.g. 30s, 5m, 1h, 500ms)");

    let (value, unit): (&str, &str) = if let Some(rest) = text.strip_suffix("ms") {
        (rest, "ms")
    } else if let Some(rest) = text.strip_suffix('s') {
        (rest, "s")
    } else if let Some(rest) = text.strip_suffix('m') {
        (rest, "m")
    } else if let Some(rest) = text.strip_suffix('h') {
        (rest, "h")
    } else {
        (text, "s")
    };

    let value: u64 = value.trim().parse().map_err(|_| invalid())?;
    let duration = match unit {
        "ms" => Duration::from_millis(value),
        "s" => Duration::from_secs(value),
        "m" => Duration::from_secs(value * 60),
        "h" => Duration::from_secs(value * 3600),
        _ => return Err(invalid()),
    };
    Ok(duration)
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

fn parse_byte_size(text: &str) -> Result<u64, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("byte size must not be empty".to_string());
    }

    let digits = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    if digits == 0 {
        return Err(format!("invalid byte size '{text}'"));
    }

    let value: u64 = trimmed[..digits]
        .parse()
        .map_err(|_| format!("invalid byte size '{text}'"))?;
    if value == 0 {
        return Err("byte size must be greater than zero".to_string());
    }

    let suffix = trimmed[digits..].trim().to_ascii_lowercase();
    let multiplier = match suffix.as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        _ => return Err(format!("invalid byte size suffix in '{text}'")),
    };

    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("byte size '{text}' is too large"))
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
            cache_chunk_size: None,
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
            cache_chunk_size: None,
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
        assert!(cli.cache_chunk_size.is_none());
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
            "--cache-chunk-size",
            "8MiB",
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
        assert_eq!(cli.cache_chunk_size.as_deref(), Some("8MiB"));
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
            parse(&["cacheshfs", "host:/srv", "/mnt", "--download", "/d", "--allow-other"])
                .unwrap();
        let reported = with_extras.unwired_options();
        assert!(reported.contains(&"--download"));
        assert!(reported.contains(&"--allow-other"));
        // --port, --identity-file, and --ssh-config are now wired into the
        // SFTP connection.
        assert!(!reported.contains(&"--port"));
        assert!(!reported.contains(&"--identity-file"));
        assert!(!reported.contains(&"--ssh-config"));
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

    /// Write ssh-config `contents` to a temp file and return the dir (kept
    /// alive by the caller) plus the path to pass as `--ssh-config`. Tests use
    /// this so they never read the developer's real `~/.ssh/config`.
    fn config_file(contents: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, contents).unwrap();
        let path = path.to_str().unwrap().to_string();
        (dir, path)
    }

    #[test]
    fn connect_options_apply_port_and_identity() {
        let (_dir, config) = config_file("");
        let cli = parse(&[
            "cacheshfs",
            "alice@host:/srv",
            "/mnt",
            "--port",
            "2222",
            "--identity-file",
            "/keys/id_ed25519",
            "--ssh-config",
            &config,
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
        let (_dir, config) = config_file("");
        let cli = parse(&[
            "cacheshfs",
            "alice@host:/srv",
            "/mnt",
            "--accept-unknown-host-key",
            "--ssh-config",
            &config,
        ])
        .unwrap();
        let options = cli.connect_options("alice@host").unwrap();
        assert!(options.accept_unknown_hosts);
    }

    #[test]
    fn ssh_config_resolves_host_alias() {
        let (_dir, config) = config_file(
            "Host server\n    HostName home.example.com\n    User braxton\n    Port 2222\n    IdentityFile /keys/server_ed25519\n",
        );
        let cli = parse(&["cacheshfs", "server:/srv", "/mnt", "--ssh-config", &config]).unwrap();
        let options = cli.connect_options("server").unwrap();
        assert_eq!(options.target.host, "home.example.com");
        assert_eq!(options.target.username, "braxton");
        assert_eq!(options.target.port, 2222);
        assert!(options
            .identity_files
            .contains(&PathBuf::from("/keys/server_ed25519")));
    }

    #[test]
    fn explicit_values_override_ssh_config() {
        let (_dir, config) = config_file(
            "Host server\n    HostName home.example.com\n    User braxton\n    Port 2222\n",
        );
        // An explicit user@ and --port beat the config; HostName still applies.
        let cli = parse(&[
            "cacheshfs",
            "root@server:/srv",
            "/mnt",
            "--port",
            "2200",
            "--ssh-config",
            &config,
        ])
        .unwrap();
        let options = cli.connect_options("root@server").unwrap();
        assert_eq!(options.target.host, "home.example.com");
        assert_eq!(options.target.username, "root");
        assert_eq!(options.target.port, 2200);
    }

    #[test]
    fn missing_explicit_ssh_config_is_an_error() {
        let cli = parse(&[
            "cacheshfs",
            "server:/srv",
            "/mnt",
            "--ssh-config",
            "/no/such/ssh/config",
        ])
        .unwrap();
        assert!(cli.connect_options("server").is_err());
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
            "--cache-chunk-size",
            "2M",
        ])
        .unwrap();
        let config = cli.to_mount_config().unwrap();
        assert_eq!(config.remote.target, "bob@winhost");
        assert_eq!(config.remote.root, "/C:/Users/bob");
        assert_eq!(config.cache_mode, CacheMode::Offline);
        assert_eq!(config.cache_chunk_size, 2 * 1024 * 1024);
        assert_eq!(config.cache_dir, PathBuf::from("/var/cache/cacheshfs"));
        assert!(!config.read_only);
    }

    #[test]
    fn cache_chunk_size_defaults_and_parses() {
        let default = parse(&["cacheshfs", "host:/srv", "/mnt"]).unwrap();
        assert_eq!(
            default.to_mount_config().unwrap().cache_chunk_size,
            DEFAULT_CACHE_CHUNK_SIZE
        );

        for (text, expected) in [
            ("4096", 4096),
            ("4K", 4 * 1024),
            ("4MB", 4 * 1024 * 1024),
            ("4MiB", 4 * 1024 * 1024),
            ("1G", 1024 * 1024 * 1024),
        ] {
            let cli = parse(&[
                "cacheshfs",
                "host:/srv",
                "/mnt",
                "--cache-chunk-size",
                text,
            ])
            .unwrap();
            assert_eq!(cli.to_mount_config().unwrap().cache_chunk_size, expected);
        }
    }

    #[test]
    fn cache_chunk_size_rejects_invalid_values() {
        for text in ["0", "", "abc", "4XB"] {
            let cli = parse(&[
                "cacheshfs",
                "host:/srv",
                "/mnt",
                "--cache-chunk-size",
                text,
            ]);
            if let Ok(cli) = cli {
                assert!(cli.to_mount_config().is_err(), "value {text} should fail");
            }
        }
    }

    #[test]
    fn parse_duration_accepts_units_and_bare_seconds() {
        use super::parse_duration;
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        // A bare number is seconds; surrounding whitespace is ignored.
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration("  10s ").unwrap(), Duration::from_secs(10));
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        use super::parse_duration;
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("").is_err());
        assert!(parse_duration("s").is_err()); // unit with no number
        assert!(parse_duration("10x").is_err()); // unknown unit
    }

    #[test]
    fn metadata_ttl_defaults_and_parses() {
        let default = parse(&["cacheshfs", "host:/srv", "/mnt"]).unwrap();
        assert_eq!(
            default.metadata_ttl_duration().unwrap(),
            Duration::from_secs(5)
        );

        let custom = parse(&["cacheshfs", "host:/srv", "/mnt", "--metadata-ttl", "2m"]).unwrap();
        assert_eq!(
            custom.metadata_ttl_duration().unwrap(),
            Duration::from_secs(120)
        );

        let bad = parse(&["cacheshfs", "host:/srv", "/mnt", "--metadata-ttl", "nope"]).unwrap();
        assert!(bad.metadata_ttl_duration().is_err());
    }
}
