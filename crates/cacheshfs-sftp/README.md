# cacheshfs-sftp

SSH/SFTP transport crate.

Implement `RemoteFilesystem` here. This crate should expose remote files through platform-neutral core types and must not depend on Linux FUSE, Windows filesystem APIs, local cache layout, or CLI parsing.
