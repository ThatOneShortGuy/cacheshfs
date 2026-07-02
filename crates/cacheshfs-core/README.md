# cacheshfs-core

Shared contracts and platform-neutral filesystem/cache types.

Platform crates call `VirtualFilesystem`; transport crates implement
`RemoteFilesystem`.

## `CacheVfs`

`CacheVfs` is the shared `VirtualFilesystem` implementation that platform
adapters mount. It owns the `NodeId` ⇄ `RemotePath` mapping (root is
`NodeId::ROOT`, with stable node identity for any path looked up or listed), the
open file-handle table, and a **persistent** path-keyed cache store. It forwards
operations to an `Arc<dyn RemoteFilesystem>` (e.g. the SFTP transport). Path
components are validated so lookup names cannot escape the root.

### Persistent store

The store (`store.rs`) keeps a path-keyed index at `cache_dir/index.json` plus
one content object per cached file under `cache_dir/objects/<id>`. It records
file/directory attributes and directory listings so a previously seen tree can
be navigated and `stat`ed, and holds hydrated file content. Every mutation of
the index is written crash-safely (temp file → `fsync` → atomic rename), so the
cache survives process restart: relaunching against the same `cache_dir` reuses
whatever was cached in an earlier run.

### Metadata cache

Attributes and listings live in the store; freshness is tracked separately with
a TTL (a `validated` timestamp per path) plus a negative-lookup TTL for
not-found results. Within the TTL, `getattr`/`lookup`/`readdir` are served from
the store without contacting the remote; past it, the remote is revalidated.
`CacheMode::Offline` serves cached metadata regardless of age and never contacts
the remote.

### Content cache

In `OnDemand`/`Pinned` modes the first read of a file hydrates the whole file
into `cache_dir/objects` (crash-safe: stream to a temp file, `fsync`, atomic
rename) and later reads are served locally until the file changes or is written.
`Remote` mode reads straight through without caching.

### Server wins

On reconnect the server is authoritative. Revalidation compares the cached
version against the remote by size/mtime; if they differ, the stale content is
dropped and the file is re-hydrated from the server. `getattr`/`lookup` that hit
`NotFound` remove the entry from the store. There is no local dirty state to
reconcile — writes are write-through (below) — so a divergence always resolves
in the server's favour.

### Offline

`CacheMode::Offline` (and the `CacheVfs::new_offline` constructor, which takes no
remote at all) serves entirely from the persistent store: cached metadata and
content are returned regardless of age, uncached paths report `NotFound`, and
mutations report `Unavailable`. This lets a mount come up and serve a previously
cached tree with no connection.

### Writes

Writes are write-through to the remote and evict the cached content and
metadata. `flush` is a no-op and there is no dirty state yet. In read-only mode
every mutation is rejected with `Error::PermissionDenied`.

## Wiring

`CacheVfs::new(remote, root, read_only, cache_mode, metadata_ttl, cache_dir)` —
the CLI constructs this over the SFTP transport, passing the parsed mount
options through. In `CacheMode::Offline` the CLI instead uses
`CacheVfs::new_offline(root, read_only, metadata_ttl, cache_dir)`, which opens no
connection and serves only from the persistent store at `cache_dir`.
