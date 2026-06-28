# cacheshfs CLI

Command-line entrypoint.

This crate should parse arguments, build `MountConfig`, construct the shared cache-backed VFS, and dispatch to the platform `MountBackend`.
