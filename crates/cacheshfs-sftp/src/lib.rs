//! SSH/SFTP transport implementing [`cacheshfs_core::RemoteFilesystem`].
//!
//! Built on the pure-Rust [`russh`]/[`russh_sftp`] stack (no OpenSSL/WinCNG C
//! crypto backend), so modern key types — notably ed25519 — work consistently
//! on every platform. The crate exposes a synchronous `RemoteFilesystem`; async
//! is confined here, driven by an embedded multi-threaded Tokio runtime that the
//! synchronous methods `block_on`. A single SSH connection multiplexes
//! concurrent SFTP requests, so the previous global SFTP mutex is gone.

use std::future::Future;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cacheshfs_core::{
    Error, FileAttributes, FileKind, RemoteDirectoryEntry, RemoteFilesystem, RemotePath, Result,
    SetAttributes,
};
use russh::client::{self, Handle};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::known_hosts::{learn_known_hosts, learn_known_hosts_path};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, check_known_hosts_path, load_secret_key};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes as SftpAttributes, FileType, OpenFlags, StatusCode};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::runtime::Runtime;

pub struct SftpBackend {
    // Kept alive to hold the SSH connection open; field-drop order is
    // declaration order, so the runtime (last) outlives the session/handle.
    _handle: Handle<ClientHandler>,
    sftp: SftpSession,
    runtime: Runtime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SftpTarget {
    pub username: String,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SftpConnectOptions {
    pub target: SftpTarget,
    pub known_hosts_files: Vec<PathBuf>,
    pub accept_unknown_hosts: bool,
    /// Reserved: SSH-agent auth is not yet wired in the russh transport, which
    /// authenticates with identity files. Kept for API/source compatibility.
    pub use_agent: bool,
    pub identity_files: Vec<PathBuf>,
    pub passphrase: Option<String>,
}

impl SftpBackend {
    pub fn connect(target: &str) -> Result<Self> {
        Self::connect_target(SftpTarget::parse(target)?)
    }

    pub fn connect_target(target: SftpTarget) -> Result<Self> {
        Self::connect_with_options(SftpConnectOptions::for_target(target))
    }

    pub fn connect_with_options(options: SftpConnectOptions) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                Error::Unavailable(format!("failed to start ssh transport runtime: {error}"))
            })?;

        let (handle, sftp) = runtime.block_on(connect_async(&options))?;

        Ok(Self {
            _handle: handle,
            sftp,
            runtime,
        })
    }
}

impl SftpTarget {
    pub fn parse(target: &str) -> Result<Self> {
        let (username, host_and_port) = match target.rsplit_once('@') {
            Some((username, host_and_port)) if !username.is_empty() => {
                (username.to_string(), host_and_port)
            }
            _ => (default_username()?, target),
        };

        let (host, port) = parse_host_and_port(host_and_port)?;

        Ok(Self {
            username,
            host,
            port,
        })
    }
}

impl SftpConnectOptions {
    pub fn for_target(target: SftpTarget) -> Self {
        Self {
            target,
            known_hosts_files: default_known_hosts_files(),
            accept_unknown_hosts: false,
            use_agent: true,
            identity_files: default_identity_files(),
            passphrase: None,
        }
    }

    pub fn accept_unknown_hosts(mut self, accept_unknown_hosts: bool) -> Self {
        self.accept_unknown_hosts = accept_unknown_hosts;
        self
    }

    pub fn with_known_hosts_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.known_hosts_files.push(path.into());
        self
    }

    pub fn with_identity_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.identity_files.push(path.into());
        self
    }

    pub fn with_passphrase(mut self, passphrase: impl Into<String>) -> Self {
        self.passphrase = Some(passphrase.into());
        self
    }
}

impl RemoteFilesystem for SftpBackend {
    fn stat(&self, path: &RemotePath) -> Result<FileAttributes> {
        let path = path.as_str().to_string();
        self.runtime.block_on(async {
            let metadata = self.sftp.metadata(path).await.map_err(map_sftp_error)?;
            Ok(file_attributes(&metadata))
        })
    }

    fn read_dir(&self, path: &RemotePath) -> Result<Vec<RemoteDirectoryEntry>> {
        let path = path.as_str().to_string();
        self.runtime.block_on(async {
            let entries = self.sftp.read_dir(path).await.map_err(map_sftp_error)?;
            Ok(entries
                .map(|entry| RemoteDirectoryEntry {
                    name: entry.file_name(),
                    attributes: file_attributes(&entry.metadata()),
                })
                .collect())
        })
    }

    fn read(&self, path: &RemotePath, offset: u64, size: u32) -> Result<Vec<u8>> {
        let path = path.as_str().to_string();
        self.runtime.block_on(async {
            let mut file = self.sftp.open(path).await.map_err(map_sftp_error)?;
            file.seek(SeekFrom::Start(offset))
                .await
                .map_err(map_io_error)?;

            let mut buffer = vec![0u8; size as usize];
            let mut filled = 0;
            while filled < buffer.len() {
                let read = file.read(&mut buffer[filled..]).await.map_err(map_io_error)?;
                if read == 0 {
                    break;
                }
                filled += read;
            }
            buffer.truncate(filled);
            Ok(buffer)
        })
    }

    fn write(&self, path: &RemotePath, offset: u64, data: &[u8]) -> Result<u32> {
        let path = path.as_str().to_string();
        let data = data.to_vec();
        self.runtime.block_on(async {
            let mut file = self
                .sftp
                .open_with_flags(path, OpenFlags::WRITE)
                .await
                .map_err(map_sftp_error)?;
            file.seek(SeekFrom::Start(offset))
                .await
                .map_err(map_io_error)?;
            file.write_all(&data).await.map_err(map_io_error)?;
            file.flush().await.map_err(map_io_error)?;
            Ok(data.len() as u32)
        })
    }

    fn create(&self, path: &RemotePath, mode: u32) -> Result<FileAttributes> {
        let path = path.as_str().to_string();
        self.runtime.block_on(async {
            let attributes = SftpAttributes {
                permissions: Some(mode),
                ..SftpAttributes::empty()
            };
            let _file = self
                .sftp
                .open_with_flags_and_attributes(
                    path.clone(),
                    OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
                    attributes,
                )
                .await
                .map_err(map_sftp_error)?;
            let metadata = self.sftp.metadata(path).await.map_err(map_sftp_error)?;
            Ok(file_attributes(&metadata))
        })
    }

    fn mkdir(&self, path: &RemotePath, mode: u32) -> Result<FileAttributes> {
        let path = path.as_str().to_string();
        self.runtime.block_on(async {
            self.sftp
                .create_dir(path.clone())
                .await
                .map_err(map_sftp_error)?;
            // create_dir takes no mode, so apply permissions best-effort after.
            let attributes = SftpAttributes {
                permissions: Some(mode),
                ..SftpAttributes::empty()
            };
            let _ = self.sftp.set_metadata(path.clone(), attributes).await;
            let metadata = self.sftp.metadata(path).await.map_err(map_sftp_error)?;
            Ok(file_attributes(&metadata))
        })
    }

    fn unlink(&self, path: &RemotePath) -> Result<()> {
        let path = path.as_str().to_string();
        self.runtime
            .block_on(async { self.sftp.remove_file(path).await.map_err(map_sftp_error) })
    }

    fn rmdir(&self, path: &RemotePath) -> Result<()> {
        let path = path.as_str().to_string();
        self.runtime
            .block_on(async { self.sftp.remove_dir(path).await.map_err(map_sftp_error) })
    }

    fn rename(&self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        let from = from.as_str().to_string();
        let to = to.as_str().to_string();
        self.runtime
            .block_on(async { self.sftp.rename(from, to).await.map_err(map_sftp_error) })
    }

    fn setattr(&self, path: &RemotePath, attributes: SetAttributes) -> Result<FileAttributes> {
        let path = path.as_str().to_string();
        self.runtime.block_on(async {
            self.sftp
                .set_metadata(path.clone(), set_attributes(&attributes))
                .await
                .map_err(map_sftp_error)?;
            let metadata = self.sftp.metadata(path).await.map_err(map_sftp_error)?;
            Ok(file_attributes(&metadata))
        })
    }
}

/// russh client handler. Its only job is host-key verification.
struct ClientHandler {
    known_hosts_files: Vec<PathBuf>,
    accept_unknown_hosts: bool,
    host: String,
    port: u16,
}

/// Result of looking a server's host key up in `known_hosts`.
enum HostKeyStatus {
    /// The key is recorded and matches.
    Known,
    /// The host is recorded with a *different* key (possible MITM) — reject.
    Changed,
    /// The host isn't recorded yet.
    Unknown,
}

impl ClientHandler {
    fn classify_host_key(&self, key: &PublicKey) -> HostKeyStatus {
        for path in &self.known_hosts_files {
            if !path.exists() {
                continue;
            }
            match check_known_hosts_path(&self.host, self.port, key, path) {
                Ok(true) => return HostKeyStatus::Known,
                Ok(false) => continue,
                Err(_) => return HostKeyStatus::Changed,
            }
        }
        HostKeyStatus::Unknown
    }
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> impl Future<Output = std::result::Result<bool, Self::Error>> + Send {
        let status = self.classify_host_key(server_public_key);
        let accept_unknown = self.accept_unknown_hosts;
        let host = self.host.clone();
        let port = self.port;
        // Record into the first configured known_hosts file (the user's
        // ~/.ssh/known_hosts by default).
        let known_hosts_path = self.known_hosts_files.first().cloned();
        let key = server_public_key.clone();
        let fingerprint = server_public_key.fingerprint(HashAlg::Sha256).to_string();
        let key_type = server_public_key.algorithm().as_str().to_string();

        async move {
            match status {
                HostKeyStatus::Known => Ok(true),
                HostKeyStatus::Changed => Ok(false),
                HostKeyStatus::Unknown => {
                    // Blind opt-in skips the prompt entirely.
                    if accept_unknown {
                        return Ok(true);
                    }
                    // Prompt off the async worker so the blocking stdin read
                    // doesn't stall the runtime.
                    let decision = tokio::task::spawn_blocking(move || {
                        prompt_unknown_host(
                            &host,
                            port,
                            &key,
                            &fingerprint,
                            &key_type,
                            known_hosts_path.as_deref(),
                        )
                    })
                    .await
                    .unwrap_or(false);
                    Ok(decision)
                }
            }
        }
    }
}

/// Trust-on-first-use prompt for an unknown host key, mirroring the OpenSSH
/// client. Only prompts when stdin is a terminal; otherwise refuses (safe for
/// non-interactive/daemon use). On acceptance the key is recorded in
/// `known_hosts` so later connections verify silently.
///
/// This console interaction lives in the transport for now; a future refactor
/// could inject the policy as a callback so the CLI owns the UI.
fn prompt_unknown_host(
    host: &str,
    port: u16,
    key: &PublicKey,
    fingerprint: &str,
    key_type: &str,
    known_hosts_path: Option<&Path>,
) -> bool {
    use std::io::{BufRead, IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        eprintln!(
            "cacheshfs: host '{host}' is not in known_hosts and no terminal is available to \
             confirm its key ({key_type} {fingerprint}); refusing. Pass \
             --accept-unknown-host-key to connect without verification."
        );
        return false;
    }

    let mut stderr = std::io::stderr();
    let _ = writeln!(
        stderr,
        "The authenticity of host '{host}' can't be established.\n\
         {key_type} key fingerprint is {fingerprint}."
    );
    let _ = write!(
        stderr,
        "Are you sure you want to continue connecting (yes/no)? "
    );
    let _ = stderr.flush();

    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer).is_err() {
        return false;
    }
    if !answer.trim().eq_ignore_ascii_case("yes") {
        eprintln!("Host key verification declined.");
        return false;
    }

    // Approved: record the key so future connects verify without a prompt.
    let recorded = match known_hosts_path {
        Some(path) => learn_known_hosts_path(host, port, key, path),
        None => learn_known_hosts(host, port, key),
    };
    match recorded {
        Ok(()) => eprintln!(
            "Warning: Permanently added '{host}' ({key_type}) to the list of known hosts."
        ),
        Err(error) => {
            eprintln!("cacheshfs: warning: could not record host key in known_hosts: {error}")
        }
    }
    true
}

async fn connect_async(
    options: &SftpConnectOptions,
) -> Result<(Handle<ClientHandler>, SftpSession)> {
    let config = Arc::new(client::Config::default());
    let handler = ClientHandler {
        known_hosts_files: options.known_hosts_files.clone(),
        accept_unknown_hosts: options.accept_unknown_hosts,
        host: options.target.host.clone(),
        port: options.target.port,
    };

    let mut handle = client::connect(
        config,
        (options.target.host.as_str(), options.target.port),
        handler,
    )
    .await
    .map_err(map_russh_error)?;

    authenticate(&mut handle, options).await?;

    let channel = handle
        .channel_open_session()
        .await
        .map_err(map_russh_error)?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(map_russh_error)?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(map_sftp_error)?;

    Ok((handle, sftp))
}

async fn authenticate(handle: &mut Handle<ClientHandler>, options: &SftpConnectOptions) -> Result<()> {
    // The hash algorithm only matters for RSA keys; ed25519/ecdsa ignore it.
    let rsa_hash = handle
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten();

    // Prefer the SSH agent (lets passphrase-protected keys be used without the
    // passphrase on the command line), then fall back to identity files.
    if options.use_agent && try_agent_auth(handle, &options.target.username, rsa_hash).await {
        return Ok(());
    }

    for identity_file in &options.identity_files {
        if !identity_file.exists() {
            continue;
        }
        let key = match load_secret_key(identity_file, options.passphrase.as_deref()) {
            Ok(key) => Arc::new(key),
            // Unreadable or wrong passphrase: try the next key.
            Err(_) => continue,
        };

        let key = PrivateKeyWithHashAlg::new(key, rsa_hash);
        match handle.authenticate_publickey(&options.target.username, key).await {
            Ok(result) if result.success() => return Ok(()),
            _ => continue,
        }
    }

    Err(Error::PermissionDenied)
}

/// Connect to the platform's SSH agent and try to authenticate with each of its
/// identities. Returns `true` on success, `false` if no agent is reachable or no
/// identity authenticated.
#[cfg(unix)]
async fn try_agent_auth(
    handle: &mut Handle<ClientHandler>,
    user: &str,
    rsa_hash: Option<HashAlg>,
) -> bool {
    match AgentClient::connect_env().await {
        Ok(agent) => authenticate_with_agent(handle, user, rsa_hash, agent).await,
        Err(_) => false,
    }
}

#[cfg(windows)]
async fn try_agent_auth(
    handle: &mut Handle<ClientHandler>,
    user: &str,
    rsa_hash: Option<HashAlg>,
) -> bool {
    // Try named-pipe agents in order: an explicit `SSH_AUTH_SOCK` (if set), then
    // the default Windows OpenSSH agent pipe — so a stray/foreign `SSH_AUTH_SOCK`
    // can't hide the real service. Finally fall back to Pageant.
    let mut pipes: Vec<std::ffi::OsString> = Vec::new();
    if let Some(sock) = std::env::var_os("SSH_AUTH_SOCK") {
        pipes.push(sock);
    }
    pipes.push(std::ffi::OsString::from(r"\\.\pipe\openssh-ssh-agent"));

    for pipe in pipes {
        if let Ok(agent) = AgentClient::connect_named_pipe(&pipe).await
            && authenticate_with_agent(handle, user, rsa_hash, agent).await
        {
            return true;
        }
    }

    match AgentClient::connect_pageant().await {
        Ok(agent) => authenticate_with_agent(handle, user, rsa_hash, agent).await,
        Err(_) => false,
    }
}

#[cfg(not(any(unix, windows)))]
async fn try_agent_auth(
    _handle: &mut Handle<ClientHandler>,
    _user: &str,
    _rsa_hash: Option<HashAlg>,
) -> bool {
    false
}

/// Try every identity offered by `agent` against the server, signing through the
/// agent (the private key never leaves it). Generic over the agent's transport
/// so concrete stream types satisfy the `Signer` bounds without type erasure.
async fn authenticate_with_agent<S>(
    handle: &mut Handle<ClientHandler>,
    user: &str,
    rsa_hash: Option<HashAlg>,
    mut agent: AgentClient<S>,
) -> bool
where
    S: AgentStream + Unpin + Send,
{
    let identities = match agent.request_identities().await {
        Ok(identities) => identities,
        Err(_) => return false,
    };

    for identity in identities {
        let AgentIdentity::PublicKey { key, .. } = identity else {
            // Certificates aren't handled here.
            continue;
        };
        let hash_alg = if key.algorithm().is_rsa() {
            rsa_hash
        } else {
            None
        };
        if let Ok(result) = handle
            .authenticate_publickey_with(user, key, hash_alg, &mut agent)
            .await
            && result.success()
        {
            return true;
        }
    }

    false
}

fn default_username() -> Result<String> {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .map_err(|_| Error::InvalidInput("sftp target must include a username".to_string()))
}

fn default_known_hosts_files() -> Vec<PathBuf> {
    home_dir()
        .map(|home| vec![home.join(".ssh").join("known_hosts")])
        .unwrap_or_default()
}

fn default_identity_files() -> Vec<PathBuf> {
    home_dir()
        .map(|home| {
            let ssh_dir = home.join(".ssh");
            ["id_ed25519", "id_ecdsa", "id_rsa"]
                .into_iter()
                .map(|name| ssh_dir.join(name))
                .collect()
        })
        .unwrap_or_default()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn parse_host_and_port(host_and_port: &str) -> Result<(String, u16)> {
    if host_and_port.is_empty() {
        return Err(Error::InvalidInput("sftp target host is empty".to_string()));
    }

    if let Some(rest) = host_and_port.strip_prefix('[') {
        let (host, port) = rest
            .split_once(']')
            .ok_or_else(|| Error::InvalidInput("bracketed IPv6 host is missing ']'".to_string()))?;
        let port = match port.strip_prefix(':') {
            Some(port) if !port.is_empty() => parse_port(port)?,
            Some("") => 22,
            _ => {
                return Err(Error::InvalidInput(
                    "bracketed host port must follow ']' with ':'".to_string(),
                ));
            }
        };

        return Ok((host.to_string(), port));
    }

    match host_and_port.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => Ok((host.to_string(), parse_port(port)?)),
        _ => Ok((host_and_port.to_string(), 22)),
    }
}

fn parse_port(port: &str) -> Result<u16> {
    port.parse::<u16>()
        .map_err(|_| Error::InvalidInput(format!("invalid sftp target port: {port}")))
}

/// Convert russh-sftp file attributes into platform-neutral [`FileAttributes`].
fn file_attributes(metadata: &SftpAttributes) -> FileAttributes {
    let mode = metadata.permissions.unwrap_or(0);
    FileAttributes {
        kind: match metadata.file_type() {
            FileType::Dir => FileKind::Directory,
            FileType::Symlink => FileKind::Symlink,
            _ => FileKind::File,
        },
        size: metadata.size.unwrap_or(0),
        mode,
        uid: metadata.uid.unwrap_or(0),
        gid: metadata.gid.unwrap_or(0),
        modified_unix_seconds: metadata.mtime.map(|mtime| mtime as i64),
        accessed_unix_seconds: metadata.atime.map(|atime| atime as i64),
        changed_unix_seconds: None,
    }
}

/// Convert a [`SetAttributes`] request into russh-sftp file attributes.
fn set_attributes(attributes: &SetAttributes) -> SftpAttributes {
    SftpAttributes {
        size: attributes.size,
        permissions: attributes.mode,
        atime: attributes.accessed_unix_seconds.map(|atime| atime as u32),
        mtime: attributes.modified_unix_seconds.map(|mtime| mtime as u32),
        ..SftpAttributes::empty()
    }
}

fn map_io_error(error: std::io::Error) -> Error {
    match error.kind() {
        std::io::ErrorKind::NotFound => Error::NotFound,
        std::io::ErrorKind::AlreadyExists => Error::AlreadyExists,
        std::io::ErrorKind::PermissionDenied => Error::PermissionDenied,
        _ => Error::RemoteBackend(error.to_string()),
    }
}

fn map_sftp_error(error: russh_sftp::client::error::Error) -> Error {
    use russh_sftp::client::error::Error as SftpError;
    match error {
        SftpError::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => Error::NotFound,
            StatusCode::PermissionDenied => Error::PermissionDenied,
            StatusCode::OpUnsupported => {
                Error::UnsupportedOperation("remote sftp server does not support this operation")
            }
            _ => Error::RemoteBackend(format!("sftp error: {}", status.error_message)),
        },
        other => Error::RemoteBackend(format!("sftp error: {other}")),
    }
}

fn map_russh_error(error: russh::Error) -> Error {
    use russh::Error as SshError;
    match &error {
        SshError::IO(_) => Error::Unavailable(format!("ssh connection error: {error}")),
        SshError::ConnectionTimeout
        | SshError::KeepaliveTimeout
        | SshError::InactivityTimeout
        | SshError::HUP
        | SshError::Disconnect => Error::Unavailable(format!("ssh connection lost: {error}")),
        SshError::UnknownKey | SshError::KeyChanged { .. } => Error::PermissionDenied,
        SshError::NotAuthenticated | SshError::NoAuthMethod => Error::PermissionDenied,
        _ => Error::RemoteBackend(format!("ssh error: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{SftpConnectOptions, SftpTarget};

    #[test]
    fn parses_user_host_target() {
        let target = SftpTarget::parse("alice@example.com").unwrap();

        assert_eq!(target.username, "alice");
        assert_eq!(target.host, "example.com");
        assert_eq!(target.port, 22);
    }

    #[test]
    fn parses_user_host_port_target() {
        let target = SftpTarget::parse("alice@example.com:2222").unwrap();

        assert_eq!(target.username, "alice");
        assert_eq!(target.host, "example.com");
        assert_eq!(target.port, 2222);
    }

    #[test]
    fn parses_bracketed_ipv6_target() {
        let target = SftpTarget::parse("alice@[2001:db8::1]:2222").unwrap();

        assert_eq!(target.username, "alice");
        assert_eq!(target.host, "2001:db8::1");
        assert_eq!(target.port, 2222);
    }

    #[test]
    fn connection_options_default_to_strict_host_keys() {
        let target = SftpTarget::parse("alice@example.com").unwrap();
        let options = SftpConnectOptions::for_target(target);

        assert!(!options.accept_unknown_hosts);
        assert_eq!(options.target.username, "alice");
    }

    #[test]
    fn connection_options_can_enable_unknown_host_acceptance() {
        let target = SftpTarget::parse("alice@example.com").unwrap();
        let options = SftpConnectOptions::for_target(target).accept_unknown_hosts(true);

        assert!(options.accept_unknown_hosts);
    }
}
