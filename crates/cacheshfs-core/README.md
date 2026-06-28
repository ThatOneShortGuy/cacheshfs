# cacheshfs-core

Shared contracts and platform-neutral filesystem/cache types.

Platform crates call `VirtualFilesystem`; transport crates implement
`RemoteFilesystem`.

## `CacheVfs`

`CacheVfs` is the shared `VirtualFilesystem` implementation that platform
adapters mount. It owns:

- the `NodeId` ⇄ `RemotePath` mapping (root is `NodeId::ROOT`), with stable node
  identity for any path that has been looked up or listed, and
- the open file-handle table.

It forwards operations to an `Arc<dyn RemoteFilesystem>` (e.g. the SFTP
transport).

**Current status: read-only, uncached.** `lookup`, `getattr`, `readdir`,
`open`, `read`, `flush`, and `release` work and go straight to the remote.
Mutating operations (`create`, `write`, `mkdir`, `unlink`, `rmdir`, `rename`,
`setattr`) and opening a file for write return `Error::UnsupportedOperation`.
Path components are validated so lookup names cannot escape the remote root.

Planned layers to build on this structure: a metadata cache with TTL
(spec milestone 5), on-demand content caching for reads (milestone 6), then
write-through (milestone 9).

## Wiring

`CacheVfs::new(remote, root)` takes the remote backend and the root
`RemotePath`. The CLI is not yet wired to construct it: doing so means adding
the SFTP transport as a CLI dependency and connecting (`SftpBackend::connect`),
plus carrying the SSH connection options through the shared config. That is a
follow-up on the CLI/wiring side.
