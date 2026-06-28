# cacheshfs-sftp

SSH/SFTP transport crate.

Implement `RemoteFilesystem` here. This crate should expose remote files through platform-neutral core types and must not depend on Linux FUSE, Windows filesystem APIs, local cache layout, or CLI parsing.

## Current Behavior

- Uses the blocking `ssh2` crate.
- Accepts targets in `user@host`, `user@host:port`, `host`, `host:port`, and `user@[ipv6]:port` forms.
- Verifies host keys against OpenSSH `known_hosts` files by default.
- Rejects unknown or mismatched host keys unless `SftpConnectOptions::accept_unknown_hosts(true)` is set explicitly.
- Uses the local SSH agent for authentication first.
- Falls back to private key files from standard `~/.ssh` key names.
- Implements `stat`, `read_dir`, `read`, `write`, `create`, `mkdir`, `unlink`, `rmdir`, `rename`, and `setattr` through SFTP.
- Maps common SFTP errors into `cacheshfs_core::Error`.

## Next Transport Work

- Add tests against a disposable SSH/SFTP server.
- Add configurable host-key file paths and identity paths from the CLI once CLI parsing exists.
- Decide whether Windows client paths need additional handling before passing SFTP paths through `std::path::Path`.
