# cacheshfs-sftp

SSH/SFTP transport crate.

Implement `RemoteFilesystem` here. This crate should expose remote files through platform-neutral core types and must not depend on Linux FUSE, Windows filesystem APIs, local cache layout, or CLI parsing.

## Current Behavior

- Uses the blocking `ssh2` crate.
- Accepts targets in `user@host`, `user@host:port`, `host`, `host:port`, and `user@[ipv6]:port` forms.
- Uses the local SSH agent for authentication.
- Implements `stat`, `read_dir`, `read`, `write`, `create`, `mkdir`, `unlink`, `rmdir`, `rename`, and `setattr` through SFTP.
- Maps common SFTP errors into `cacheshfs_core::Error`.

## Next Transport Work

- Add host-key verification using OpenSSH `known_hosts` files before this is used outside local development.
- Add private-key authentication fallback when no SSH agent identity works.
- Add tests against a disposable SSH/SFTP server.
- Decide whether Windows client paths need additional handling before passing SFTP paths through `std::path::Path`.
