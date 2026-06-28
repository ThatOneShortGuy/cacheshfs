# cacheshfs-windows

Windows mount adapter crate.

Implement `MountBackend` with the chosen Windows filesystem backend, likely WinFsp. This crate should translate Windows filesystem callbacks into `VirtualFilesystem` calls and map core errors to Windows status values.
