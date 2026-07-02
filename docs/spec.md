# cacheshfs Specification

## Purpose

`cacheshfs` is a FUSE filesystem for mounting a remote directory over SSH with an optional local cache. It should feel similar to `sshfs`, while giving the user explicit control over whether files are read directly from the remote host, cached locally on demand, or proactively downloaded for offline or low-latency access.

The first complete implementation target is Linux using the Rust `fuser` crate. The codebase should still keep a platform boundary from the start so a Windows client backend can be added without rewriting cache, SSH, path, or policy logic.

## Goals

- Mount a remote directory over SSH as a local filesystem.
- Support remote Linux, Unix-like, and Windows hosts running an SSH server.
- Use SFTP as the primary portable file transport unless a later design requires host-specific shell commands.
- Provide a local persistent cache for file contents and metadata.
- Let users choose whether files are downloaded, accessed remotely, or cached only after first access.
- Preserve normal filesystem behavior where practical for common tools: editors, shells, compilers, file managers, and backup tools.
- Keep cache state recoverable after crashes or interrupted transfers.

## Non-Goals For The Initial Version

- Full POSIX correctness across all remote operating systems.
- Distributed multi-writer conflict resolution.
- Kernel-level cache coherence beyond what FUSE and our own invalidation policy provide.
- Transparent support for remote filesystems that do not expose stable paths or basic file metadata through SFTP.
- Complete Windows client support. Windows may be a remote server, and the code should expose a Windows mount backend boundary, but the first production-quality client implementation is Linux.

## Terminology

- Remote: the directory tree exposed by the SSH server.
- Mount: the local FUSE mountpoint where users access the remote tree.
- Cache root: the local directory where cached data and metadata are stored.
- Online mode: the remote server is reachable and operations may use SSH.
- Offline mode: the remote server is unreachable or the user has requested no remote access.
- Hydrated file: a file whose contents are present in the local cache.
- Sparse cache entry: metadata exists locally, but file contents are not fully downloaded.

## High-Level Architecture

`cacheshfs` should contain these major layers:

- CLI frontend: parses commands and dispatches to the platform mount backend.
- Linux mount frontend: implements filesystem operations through `fuser`.
- Windows mount frontend: implements filesystem operations through a Windows filesystem backend such as WinFsp.
- Virtual filesystem model: maps FUSE inode operations to normalized remote paths and cache records.
- SSH/SFTP backend: handles connection setup, authentication, directory listing, stat, read, write, rename, mkdir, unlink, and similar operations.
- Cache manager: stores file data, metadata, validity information, dirty state, and temporary transfer files.
- Policy engine: decides when to read from remote, when to populate the cache, when cached data is valid, and when eviction is allowed.

The platform mount layers should not directly know transport details. They should ask the virtual filesystem layer for path operations, and that layer should coordinate remote access and cache behavior.

The repository should be organized as a Cargo workspace:

- `cacheshfs`: CLI binary crate.
- `cacheshfs-core`: shared VFS model, cache policy, common configuration, and platform-neutral errors.
- `cacheshfs-sftp`: SSH/SFTP transport implementation behind core-facing abstractions.
- `cacheshfs-linux`: Linux mount adapter using `fuser`.
- `cacheshfs-windows`: Windows mount adapter, initially stubbed until the Windows filesystem backend is chosen.

## Platform Client Framework

The implementation should separate shared filesystem behavior from operating-system-specific mounting APIs.

Shared core modules should contain:

- Remote path normalization.
- Inode or file identity mapping independent of the OS mount API.
- Metadata cache.
- Content cache.
- Cache policy decisions.
- Conflict detection.
- Error classification before final OS-specific error mapping.

Platform modules should contain only the adapter code required to expose the shared core as a mounted filesystem on that OS.

Expected platform modules:

- Linux: use `fuser` as the mount backend.
- Windows: reserve a backend boundary for a Windows filesystem layer, likely WinFsp or a Rust crate built on top of WinFsp.
- Unsupported platforms: compile a backend that reports a clear unsupported-platform error.

The public mount entrypoint should accept a platform-neutral mount configuration and dispatch to the current OS backend at compile time. Platform-specific dependencies should be target-gated so Linux FUSE dependencies are not required for Windows builds and Windows filesystem dependencies are not required for Linux builds.

Linux adapter responsibilities:

- Translate `fuser` callbacks into shared core operations.
- Convert shared errors into Linux `errno` values.
- Manage FUSE mount options such as `allow_other`, read-only mode, and kernel attribute TTLs.
- Avoid embedding SSH or cache policy decisions in callback code.

Windows adapter responsibilities:

- Translate the chosen Windows filesystem backend callbacks into shared core operations.
- Convert shared errors into Windows filesystem status values.
- Handle Windows-specific path and case behavior at the adapter boundary where required.
- Preserve the same cache semantics as the Linux backend where the host filesystem API allows it.

Windows client support should not force Windows semantics into the shared core. The shared core should use normalized logical paths and explicit metadata fields, while adapters handle OS-specific details such as separators, case sensitivity expectations, handle lifecycle, and error code translation.

## Mount Configuration

The command-line interface should eventually support at least:

```text
cacheshfs [options] user@host:/remote/path /local/mountpoint
```

Required inputs:

- Remote SSH target.
- Remote root path.
- Local mountpoint.

Important options:

- `--cache-dir PATH`: directory for persistent cache state.
- `--cache-mode MODE`: one of `remote`, `on-demand`, `pinned`, or `offline`.
- `--cache-chunk-size SIZE`: content cache chunk size; defaults to `4 MiB`.
- `--metadata-ttl DURATION`: how long cached metadata is trusted.
- `--content-ttl DURATION`: how long clean cached file contents are trusted before revalidation.
- `--download PATH`: prefetch a remote path into the cache before or after mounting.
- `--read-only`: prevent write operations.
- `--ssh-config PATH`: optional SSH config file.
- `--identity-file PATH`: optional private key.
- `--port PORT`: SSH port.
- `--allow-other`: pass through FUSE allow-other behavior when permitted by the system.

## Cache Modes

### `remote`

Prefer direct remote access. File contents do not need to be stored persistently except for short-lived buffers required to satisfy reads. Metadata may still be cached briefly according to `--metadata-ttl`.

Expected behavior:

- Reads are served from the remote host when online.
- Cached file contents are not created unless needed internally.
- Offline access to non-hydrated content fails.

### `on-demand`

Cache file contents as they are read. This is the default target behavior.

Expected behavior:

- Directory entries and metadata are fetched remotely and cached.
- Reading a file downloads the requested ranges or the whole file, depending on implementation maturity.
- Once hydrated, subsequent reads may be served locally until invalidated.
- Offline access works for hydrated files whose metadata is available locally.

### `pinned`

Keep selected files or directories available locally.

Expected behavior:

- Pinned paths are proactively downloaded.
- Pinned paths are not evicted automatically.
- Updates should be synchronized when online.
- Offline reads should work for fully hydrated pinned files.

### `offline`

Do not contact the remote host after startup, or do not require the remote host at all if enough cache state exists.

Expected behavior:

- Operations use only local cache state.
- Non-hydrated files fail with an appropriate I/O error.
- Mutating operations should either be rejected or recorded as dirty pending changes, depending on the write strategy chosen for the implementation.

## File Content Caching

The cache must store file contents outside the mountpoint to avoid recursive filesystem behavior.

Cache entries should track:

- Normalized remote path.
- File type.
- Size.
- Modification time if available.
- Permissions or best-effort mode bits.
- Remote identity hints if available, such as inode, file ID, or SFTP attributes.
- Hydration status: none, partial, complete.
- Dirty status for local changes not yet uploaded.
- Last validation time.

Downloads must be crash-safe:

- Write incoming data to a temporary file.
- Verify expected size or transfer completion when possible.
- Atomically rename into the cache when complete.
- Never mark an entry fully hydrated before the content file is durable enough to be reused.

Partial range caching may be added later. The first implementation may download whole files on first read for simplicity, but the spec should not prevent range-based caching for large files.

## Metadata Caching

Directory listings and file attributes should be cached separately from file contents.

Metadata cache behavior:

- `lookup`, `getattr`, and `readdir` may use cached metadata until the metadata TTL expires.
- Expired metadata should be revalidated against the remote when online.
- Offline mode may use stale metadata, but should expose that state through logs or diagnostics.
- Negative lookup results may be cached briefly to reduce repeated remote calls, but must use a short TTL.

## Read Behavior

Read behavior depends on cache mode and hydration state:

- If valid content is fully hydrated, read locally.
- If content is missing and online access is allowed, fetch from remote according to policy.
- If content is missing and remote access is not allowed or unavailable, return an I/O error such as `EIO` or `ENODATA` depending on the operation context.
- If cached metadata says a path is a directory, reads as a file must fail with the appropriate filesystem error.

Large file reads should avoid loading entire files into memory. Transfers should stream to disk or directly into read buffers.

## Write Behavior

The project should support a conservative write strategy first.

Initial recommended behavior:

- Default to write-through when online: local writes update the cache and are uploaded to the remote before reporting final success for close or flush.
- Mark files dirty while writes are in progress.
- If upload fails, keep the dirty cached version and report an error where FUSE semantics allow.
- In read-only mode, reject all mutating operations.

Open design decision:

- Whether offline writes are supported initially. If supported, they require a dirty queue and conflict handling on reconnect. If not supported, offline mutating operations should fail clearly.

Minimum mutating operations to specify before implementation:

- `create`
- `write`
- `truncate`
- `unlink`
- `rename`
- `mkdir`
- `rmdir`
- `chmod` or best-effort permission update
- `utimens` or best-effort timestamp update

## Consistency Model

The filesystem should use close-to-open style consistency:

- A file opened after TTL expiry should revalidate metadata before trusting cached contents.
- A file already open may continue reading the version selected at open time.
- Local writes should update local cache state immediately.
- Remote changes made by other clients are detected only through TTL expiry, explicit refresh, or failed remote operations.

If remote metadata changes but cached content is present:

- If size or modification time changed, cached content should be considered stale.
- Stale content may be retained on disk for possible reuse but must not be served as fresh unless policy explicitly permits stale offline reads.

## Conflict Handling

For the initial version, conflict handling should be simple and conservative:

- If a clean cached file becomes stale, invalidate it and fetch the remote version on next read.
- If a dirty cached file conflicts with a changed remote file, do not overwrite remote data silently.
- Preserve the dirty local version in the cache and return an error or expose a conflict state.
- Future versions may add conflict files, manual resolution commands, or versioned cache records.

## Remote Path Handling

Remote paths must be normalized internally.

Requirements:

- Treat the configured remote root as the filesystem root.
- Prevent `..` traversal from escaping the remote root.
- Use SFTP path behavior as the transport-level source of truth.
- Avoid assuming Unix path semantics beyond what SFTP requires.
- Handle Windows SSH servers where paths may use drive roots, case-insensitive lookup, or different permission semantics.

Windows remote considerations:

- Remote root examples may include `/C:/Users/name` or another path format exposed by the server's SFTP subsystem.
- Permission bits may be synthetic or incomplete.
- Symlink support may be unavailable or require elevated permissions.
- File locking and rename semantics may differ from Unix servers.
- Case-only renames and case-insensitive collisions need explicit testing before claiming full support.

## Permissions And Ownership

SFTP may not provide full Unix ownership and permission data for all servers.

Behavior should be best-effort:

- Expose remote mode bits when available.
- Synthesize reasonable file and directory permissions when unavailable.
- Map all files to the mounting user's UID/GID by default unless a later option supports another policy.
- Do not depend on remote numeric UID/GID matching local users.

## Symlinks

Symlink support depends on the remote server.

Expected behavior:

- If SFTP reports symlinks and supports `readlink`, expose them as symlinks.
- Symlink targets should be returned as reported by the server.
- Symlinks that escape the remote root are allowed as links, but following them depends on server behavior and mount policy.
- On servers without symlink support, symlink operations should fail with `ENOSYS` or an appropriate remote error mapping.

## Error Mapping

Remote errors must be translated to stable filesystem errors.

Examples:

- Missing path: `ENOENT`.
- Permission denied: `EACCES`.
- Existing destination where not allowed: `EEXIST`.
- Directory not empty: `ENOTEMPTY`.
- Unsupported remote operation: `ENOSYS` or `EOPNOTSUPP`.
- Lost SSH connection during required remote access: `EIO`.
- Missing cached content in offline mode: `EIO` initially, with a more specific mapping if practical.

## Connection Management

The SSH backend should:

- Reuse connections where possible.
- Reconnect automatically after transient failures when an operation can be safely retried.
- Avoid retrying non-idempotent writes unless the implementation can determine whether the remote operation completed.
- Surface persistent connection failures through FUSE errors and logs.

Authentication should rely on standard SSH mechanisms where possible:

- SSH agent.
- Private key files.
- Password or keyboard-interactive auth if explicitly supported.
- OpenSSH config compatibility where practical.

## Download And Prefetch

The project should include a way to explicitly hydrate cache entries.

Expected behaviors:

- Download a single file into the cache.
- Recursively download a directory into the cache.
- Continue serving the mount while background downloads run.
- Report progress through logs or a future status command.
- Skip unchanged cached files after metadata validation.
- Avoid unbounded parallel downloads by using a configurable concurrency limit.

## Eviction

Automatic eviction can be added after basic caching is reliable.

Expected future policy options:

- Maximum cache size.
- Least-recently-used eviction.
- Minimum free disk space.
- Never evict pinned paths.
- Never evict dirty files.

Initial implementation may omit automatic eviction, but cache records should be structured so eviction can be added later.

## Observability

The implementation should provide useful diagnostics:

- Log SSH connection events.
- Log cache hits, misses, invalidations, and dirty-file preservation.
- Log remote operation failures with enough path context to debug issues.
- Provide a future `status` or `cache inspect` command to report hydration and dirty state.

Logs must not print private keys, passwords, or sensitive SSH material.

## Safety Requirements

- The cache directory must never be inside the mounted filesystem tree.
- Temporary download files must not be exposed as valid cached content.
- Dirty local data must not be deleted by eviction.
- Remote overwrite after conflict must require an explicit policy or user action.
- Path normalization must prevent escaping the configured remote root through client-side path manipulation.

## Initial Milestones

1. Minimal read-only FUSE mount with static local test data.
2. Path and inode model for lookup, getattr, and readdir.
3. SFTP connection and remote directory listing.
4. Remote read-only mount without persistent content cache.
5. Metadata cache with TTL.
6. On-demand whole-file content cache for reads.
7. Explicit file and directory download command.
8. Offline read support for hydrated files.
9. Conservative online write-through support.
10. Dirty-file preservation and conflict detection.

## Open Questions

- Should offline writes be supported in the first write-capable release, or should they fail until conflict handling is mature?
- Should first-read caching download whole files only, or should large files use range-based caching from the start?
- Which SSH/SFTP Rust library should be used, and how much OpenSSH config compatibility is required initially?
- Should the cache database use plain files plus metadata sidecars, SQLite, or another embedded format?
- What command interface should manage pinned paths and cache inspection?
