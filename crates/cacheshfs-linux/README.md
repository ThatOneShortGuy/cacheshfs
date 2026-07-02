# cacheshfs-linux

Linux mount adapter crate.

Implement `MountBackend` with `fuser` here. This crate should translate FUSE callbacks into `VirtualFilesystem` calls and map core errors to Linux errno values.

## Current Behavior

- Mounts through `fuser::spawn_mount2` on Linux, running the session on a
  background thread. The foreground thread installs a SIGINT/SIGTERM handler and
  unmounts cleanly on signal (`umount_and_join`), so `Ctrl-C` no longer leaves a
  stale "Transport endpoint is not connected" mountpoint. An external
  `fusermount -u` is detected and also ends the process.
- Rejects a mountpoint that does not exist (or is not a directory) up front with
  a clear message instead of a bare `ENOENT` from the mount syscall.
- Compiles to an unsupported-platform stub on non-Linux targets.
- Forwards common inode operations to `VirtualFilesystem`: `lookup`, `getattr`, `setattr`, `mkdir`, `unlink`, `rmdir`, `rename`, `open`, `read`, `write`, `flush`, `release`, `readdir`, and `create`.
- Converts shared `FileMetadata` into `fuser::FileAttr`.
- Maps shared `cacheshfs_core::Error` values into Linux errno values.

## Remaining Linux Work

- Add integration tests once a test VFS exists.
- Decide whether the Linux adapter should synthesize `.` and `..` entries or require the VFS to provide them.
- Add mount-option support from CLI configuration, including `allow_other` once represented in `MountConfig`.
- Add symlink read/create support after the core VFS trait exposes symlink operations.
