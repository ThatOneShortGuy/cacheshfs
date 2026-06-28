# cacheshfs-linux

Linux mount adapter crate.

Implement `MountBackend` with `fuser` here. This crate should translate FUSE callbacks into `VirtualFilesystem` calls and map core errors to Linux errno values.
