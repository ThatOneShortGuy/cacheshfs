# Implementation Contracts

This document defines the branch boundaries for parallel implementation work.

## Ownership

- Linux implementation: `crates/cacheshfs-linux`.
- Windows implementation: `crates/cacheshfs-windows`.
- Transport implementation: `crates/cacheshfs-sftp`.
- Shared filesystem/cache implementation: `crates/cacheshfs-core`.
- CLI wiring: `crates/cacheshfs-cli`.

Avoid changing another team's crate unless the shared trait contract is missing required data. If a shared trait needs to change, make the smallest possible change in `cacheshfs-core` and update all stub implementations in the same commit.

## Platform Contract

Platform crates implement `cacheshfs_core::MountBackend`.

```rust
fn mount(
    &self,
    config: MountConfig,
    filesystem: Arc<dyn VirtualFilesystem>,
) -> Result<()>;
```

Responsibilities:

- Mount the configured local mountpoint using the OS-specific filesystem API.
- Translate OS callback operations into `VirtualFilesystem` calls.
- Translate `cacheshfs_core::Error` into the platform's filesystem error representation.
- Keep SSH, cache policy, and remote path normalization out of the platform crate.

Linux-specific work should stay behind the `fuser` adapter in `cacheshfs-linux`.

Windows-specific work should stay behind the future WinFsp or equivalent adapter in `cacheshfs-windows`.

## VFS Contract

The shared filesystem/cache layer implements `cacheshfs_core::VirtualFilesystem`.

Responsibilities:

- Maintain the inode/node mapping exposed to platform adapters.
- Apply cache mode behavior.
- Hydrate file contents when policy requires it.
- Validate metadata and cached content.
- Track open file handles.
- Preserve dirty local state when remote writes fail.
- Call a `RemoteFilesystem` implementation for remote operations.

Platform crates should treat `NodeId` and `FileHandle` as opaque identifiers.

## Transport Contract

Transport crates implement `cacheshfs_core::RemoteFilesystem`.

Responsibilities:

- Connect to the remote host.
- Implement SFTP operations for metadata, directory listing, reading, writing, creation, deletion, rename, and attribute updates.
- Map transport/library errors into `cacheshfs_core::Error`.
- Return platform-neutral remote attributes in `FileAttributes`.
- Use `RemotePath` values and avoid accepting unnormalized path strings in operation methods.

The SFTP crate should not know about FUSE, WinFsp, local cache layout, mountpoints, or CLI parsing.

## Stable Shared Types

These types are intended to be stable enough for branch work:

- `MountConfig`
- `RemoteConfig`
- `CacheMode`
- `MountBackend`
- `VirtualFilesystem`
- `RemoteFilesystem`
- `NodeId`
- `FileHandle`
- `RemotePath`
- `FileMetadata`
- `FileAttributes`
- `DirectoryEntry`
- `RemoteDirectoryEntry`
- `OpenFlags`
- `SetAttributes`
- `Error`

They may still evolve, but changes should be coordinated because they affect all implementation branches.

## Suggested Branches

- `feature/linux-fuser-adapter`: implement `cacheshfs-linux` using `fuser` callbacks.
- `feature/windows-mount-adapter`: choose and wire the Windows filesystem backend.
- `feature/sftp-transport`: implement `cacheshfs-sftp` against `RemoteFilesystem`.
- `feature/core-cache-vfs`: implement the cache-backed `VirtualFilesystem`.

## Merge Strategy

Land shared trait changes first. Platform and transport branches should compile against the latest `main` before merging.

Every branch should keep `cargo check --workspace` passing on its primary development platform. Platform-specific functionality can remain stubbed on unsupported hosts as long as the workspace compiles.
