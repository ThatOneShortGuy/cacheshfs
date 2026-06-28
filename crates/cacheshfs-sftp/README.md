# cacheshfs-sftp

SSH/SFTP transport crate.

Implement `RemoteFilesystem` here. This crate should expose remote files through platform-neutral core types and must not depend on Linux FUSE, Windows filesystem APIs, local cache layout, or CLI parsing.

## Current Behavior

- Built on the pure-Rust `russh` / `russh-sftp` stack with the `ring` crypto
  backend — no OpenSSL/WinCNG C dependency, so modern key types (notably
  ed25519) work on every platform, including Windows.
- Exposes a synchronous `RemoteFilesystem`. Async is internal: an embedded
  multi-threaded Tokio runtime drives russh, and the sync methods `block_on`.
  One SSH connection multiplexes concurrent SFTP requests (no global lock).
- Accepts targets in `user@host`, `user@host:port`, `host`, `host:port`, and
  `user@[ipv6]:port` forms.
- Verifies host keys against OpenSSH `known_hosts` files by default; rejects
  unknown or mismatched keys unless `SftpConnectOptions::accept_unknown_hosts(true)`.
- Authenticates against the SSH agent first when `use_agent` is set (the
  Unix `SSH_AUTH_SOCK` socket, or the Windows OpenSSH named pipe / Pageant), so
  keys never leave the agent and passphrase-protected keys work without a
  passphrase on the command line.
- Falls back to private key files from `SftpConnectOptions::identity_files`
  (defaults to standard `~/.ssh` key names), including ed25519.
- Implements `stat`, `read_dir`, `read`, `write`, `create`, `mkdir`, `unlink`,
  `rmdir`, `rename`, and `setattr` through SFTP.
- Maps SFTP/SSH errors into `cacheshfs_core::Error`.

## Next Transport Work

- Add tests against a disposable SSH/SFTP server.
- Decide whether Windows client paths need additional handling before passing
  SFTP paths through the server.
