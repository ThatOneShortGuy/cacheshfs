# cacheshfs-core

Shared contracts and platform-neutral filesystem/cache types.

Platform crates call `VirtualFilesystem`; transport crates implement
`RemoteFilesystem`.

## `CacheVfs`

`CacheVfs` is the shared `VirtualFilesystem` implementation that platform
adapters mount. It owns the `NodeId` ⇄ `RemotePath` mapping (root is
`NodeId::ROOT`, with stable node identity for any path looked up or listed), the
open file-handle table, an in-memory metadata cache, and an on-disk content
cache. It forwards operations to an `Arc<dyn RemoteFilesystem>` (e.g. the SFTP
transport). Path components are validated so lookup names cannot escape the root.

### Metadata cache

`getattr`, `lookup` (including negative/not-found results), and `readdir` are
cached with a TTL, and a listing primes the per-child caches. Mutations
invalidate the affected entries. `CacheMode::Offline` serves cached metadata
regardless of age and never contacts the remote.

### Content cache

In `OnDemand`/`Pinned` modes the first read of a file hydrates the whole file
into `cache_dir/objects` (crash-safe: stream to a temp file, `fsync`, atomic
rename) and later reads are served locally until the file changes (revalidated
by size/mtime) or is written. `Remote` mode reads straight through without
caching; `Offline` mode serves only already-hydrated content. The hydration
index is in memory, so content caching is per-session — cross-restart
persistence and range-based caching are later refinements (and are what full
offline reads, spec milestone 8, will build on).

### Writes

Writes are write-through to the remote and evict the cached content and
metadata. `flush` is a no-op and there is no dirty state yet. In read-only mode
every mutation is rejected with `Error::PermissionDenied`.

## Wiring

`CacheVfs::new(remote, root, read_only, cache_mode, metadata_ttl, cache_dir)` —
the CLI constructs this over the SFTP transport, passing the parsed mount
options through.
