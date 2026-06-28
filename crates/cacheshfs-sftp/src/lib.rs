use cacheshfs_core::{
    Error, FileAttributes, FileKind, RemoteDirectoryEntry, RemoteFilesystem, RemotePath, Result,
    SetAttributes,
};
use ssh2::{
    CheckResult, FileStat, KnownHostFileKind, OpenFlags as SftpOpenFlags, OpenType, Session, Sftp,
};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;

pub struct SftpBackend {
    _session: Session,
    sftp: Mutex<Sftp>,
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
        let tcp = TcpStream::connect((options.target.host.as_str(), options.target.port)).map_err(
            |error| {
                Error::Unavailable(format!(
                    "failed to connect to {}:{}: {error}",
                    options.target.host, options.target.port
                ))
            },
        )?;

        let mut session = Session::new().map_err(map_ssh_error)?;
        session.set_tcp_stream(tcp);
        session.handshake().map_err(map_ssh_error)?;

        verify_host_key(&session, &options)?;
        authenticate(&session, &options)?;

        let sftp = session.sftp().map_err(map_ssh_error)?;

        Ok(Self {
            _session: session,
            sftp: Mutex::new(sftp),
        })
    }

    fn with_sftp<T>(&self, operation: impl FnOnce(&Sftp) -> Result<T>) -> Result<T> {
        let sftp = self
            .sftp
            .lock()
            .map_err(|_| Error::RemoteBackend("sftp client lock was poisoned".to_string()))?;

        operation(&sftp)
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
        self.with_sftp(|sftp| {
            let stat = sftp.stat(to_path(path)).map_err(map_ssh_error)?;
            file_attributes(stat)
        })
    }

    fn read_dir(&self, path: &RemotePath) -> Result<Vec<RemoteDirectoryEntry>> {
        self.with_sftp(|sftp| {
            sftp.readdir(to_path(path))
                .map_err(map_ssh_error)?
                .into_iter()
                .map(|(path, stat)| {
                    Ok(RemoteDirectoryEntry {
                        name: file_name(path)?,
                        attributes: file_attributes(stat)?,
                    })
                })
                .collect()
        })
    }

    fn read(&self, path: &RemotePath, offset: u64, size: u32) -> Result<Vec<u8>> {
        self.with_sftp(|sftp| {
            let mut file = sftp.open(to_path(path)).map_err(map_ssh_error)?;
            file.seek(SeekFrom::Start(offset)).map_err(map_io_error)?;

            let mut buffer = vec![0; size as usize];
            let bytes_read = file.read(&mut buffer).map_err(map_io_error)?;
            buffer.truncate(bytes_read);

            Ok(buffer)
        })
    }

    fn write(&self, path: &RemotePath, offset: u64, data: &[u8]) -> Result<u32> {
        self.with_sftp(|sftp| {
            let mut file = sftp
                .open_mode(to_path(path), SftpOpenFlags::WRITE, 0, OpenType::File)
                .map_err(map_ssh_error)?;
            file.seek(SeekFrom::Start(offset)).map_err(map_io_error)?;
            file.write_all(data).map_err(map_io_error)?;

            Ok(data.len() as u32)
        })
    }

    fn create(&self, path: &RemotePath, mode: u32) -> Result<FileAttributes> {
        self.with_sftp(|sftp| {
            let file = sftp
                .open_mode(
                    to_path(path),
                    SftpOpenFlags::CREATE | SftpOpenFlags::EXCLUSIVE | SftpOpenFlags::WRITE,
                    mode_i32(mode)?,
                    OpenType::File,
                )
                .map_err(map_ssh_error)?;
            drop(file);

            let stat = sftp.stat(to_path(path)).map_err(map_ssh_error)?;
            file_attributes(stat)
        })
    }

    fn mkdir(&self, path: &RemotePath, mode: u32) -> Result<FileAttributes> {
        self.with_sftp(|sftp| {
            sftp.mkdir(to_path(path), mode_i32(mode)?)
                .map_err(map_ssh_error)?;
            let stat = sftp.stat(to_path(path)).map_err(map_ssh_error)?;
            file_attributes(stat)
        })
    }

    fn unlink(&self, path: &RemotePath) -> Result<()> {
        self.with_sftp(|sftp| sftp.unlink(to_path(path)).map_err(map_ssh_error))
    }

    fn rmdir(&self, path: &RemotePath) -> Result<()> {
        self.with_sftp(|sftp| sftp.rmdir(to_path(path)).map_err(map_ssh_error))
    }

    fn rename(&self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        self.with_sftp(|sftp| {
            sftp.rename(to_path(from), to_path(to), None)
                .map_err(map_ssh_error)
        })
    }

    fn setattr(&self, path: &RemotePath, attributes: SetAttributes) -> Result<FileAttributes> {
        self.with_sftp(|sftp| {
            sftp.setstat(to_path(path), file_stat(attributes))
                .map_err(map_ssh_error)?;

            let stat = sftp.stat(to_path(path)).map_err(map_ssh_error)?;
            file_attributes(stat)
        })
    }
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

fn verify_host_key(session: &Session, options: &SftpConnectOptions) -> Result<()> {
    let (host_key, _) = session
        .host_key()
        .ok_or_else(|| Error::RemoteBackend("ssh server did not provide a host key".to_string()))?;

    let mut known_hosts = session.known_hosts().map_err(map_ssh_error)?;
    let mut loaded_files = 0;

    for path in &options.known_hosts_files {
        if !path.exists() {
            continue;
        }

        known_hosts
            .read_file(path, KnownHostFileKind::OpenSSH)
            .map_err(map_ssh_error)?;
        loaded_files += 1;
    }

    match known_hosts.check_port(&options.target.host, options.target.port, host_key) {
        CheckResult::Match => Ok(()),
        CheckResult::Mismatch => Err(Error::PermissionDenied),
        CheckResult::NotFound if options.accept_unknown_hosts => Ok(()),
        CheckResult::NotFound if loaded_files == 0 => Err(Error::PermissionDenied),
        CheckResult::NotFound => Err(Error::PermissionDenied),
        CheckResult::Failure => Err(Error::RemoteBackend(
            "failed to verify ssh host key against known_hosts".to_string(),
        )),
    }
}

fn authenticate(session: &Session, options: &SftpConnectOptions) -> Result<()> {
    if options.use_agent && session.userauth_agent(&options.target.username).is_ok() {
        return Ok(());
    }

    for identity_file in &options.identity_files {
        if !identity_file.exists() {
            continue;
        }

        if session
            .userauth_pubkey_file(
                &options.target.username,
                None,
                identity_file,
                options.passphrase.as_deref(),
            )
            .is_ok()
        {
            return Ok(());
        }
    }

    Err(Error::PermissionDenied)
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

fn mode_i32(mode: u32) -> Result<i32> {
    mode.try_into()
        .map_err(|_| Error::InvalidInput(format!("file mode is too large: {mode}")))
}

fn to_path(path: &RemotePath) -> &Path {
    Path::new(path.as_str())
}

fn file_name(path: PathBuf) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            Error::RemoteBackend(format!("remote path has no valid file name: {path:?}"))
        })
}

fn file_attributes(stat: FileStat) -> Result<FileAttributes> {
    let mode = stat.perm.unwrap_or(0);

    Ok(FileAttributes {
        kind: file_kind(mode),
        size: stat.size.unwrap_or(0),
        mode,
        uid: stat.uid.unwrap_or(0),
        gid: stat.gid.unwrap_or(0),
        modified_unix_seconds: stat.mtime.map(|mtime| mtime as i64),
        accessed_unix_seconds: stat.atime.map(|atime| atime as i64),
        changed_unix_seconds: None,
    })
}

fn file_kind(mode: u32) -> FileKind {
    match mode & S_IFMT {
        S_IFDIR => FileKind::Directory,
        S_IFLNK => FileKind::Symlink,
        _ => FileKind::File,
    }
}

fn file_stat(attributes: SetAttributes) -> FileStat {
    FileStat {
        size: attributes.size,
        uid: None,
        gid: None,
        perm: attributes.mode,
        atime: attributes.accessed_unix_seconds.map(|atime| atime as u64),
        mtime: attributes.modified_unix_seconds.map(|mtime| mtime as u64),
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

fn map_ssh_error(error: ssh2::Error) -> Error {
    match error.code() {
        ssh2::ErrorCode::SFTP(2) => Error::NotFound,
        ssh2::ErrorCode::SFTP(3) => Error::PermissionDenied,
        ssh2::ErrorCode::SFTP(8) => {
            Error::UnsupportedOperation("remote sftp server does not support this operation")
        }
        _ => Error::RemoteBackend(error.to_string()),
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
    fn connection_options_default_to_strict_host_keys_and_agent_auth() {
        let target = SftpTarget::parse("alice@example.com").unwrap();
        let options = SftpConnectOptions::for_target(target);

        assert!(!options.accept_unknown_hosts);
        assert!(options.use_agent);
        assert_eq!(options.target.username, "alice");
    }

    #[test]
    fn connection_options_can_enable_unknown_host_acceptance() {
        let target = SftpTarget::parse("alice@example.com").unwrap();
        let options = SftpConnectOptions::for_target(target).accept_unknown_hosts(true);

        assert!(options.accept_unknown_hosts);
    }
}
