# cacheshfs CLI

Command-line entrypoint.

Parses arguments, builds a `MountConfig`, connects the SFTP transport, wraps it
in the shared `CacheVfs`, and dispatches to the platform `MountBackend`. With
`--cache-mode offline` it skips the connection entirely and serves from the
persistent cache (see below).

```text
cacheshfs [OPTIONS] [user@]host:/remote/path <mountpoint>
```

## Wired options

- positional remote `[user@]host:/remote/path` and `mountpoint`
- `--cache-dir`, `--cache-mode`, `--read-only` → `MountConfig`
- `--metadata-ttl` (e.g. `30s`, `5m`) → metadata cache TTL in `CacheVfs`
- `--port`, `--identity-file`, `--accept-unknown-host-key` → SFTP connection
  (`SftpConnectOptions`)

By default, connecting to a host absent from `known_hosts` shows an OpenSSH-style
trust-on-first-use prompt (displays the key fingerprint; on `yes` the key is
recorded so later connections verify silently). A *changed* host key is always
rejected. With no terminal available the connection is refused rather than
hanging. `--accept-unknown-host-key` skips the prompt and blindly trusts unknown
hosts (insecure).

## Accepted but not yet applied

These are parsed (so `--help` is complete and forward-compatible) but warn when
supplied, pending support in the core/transport layers:

- `--ssh-config` (OpenSSH config parsing)
- `--content-ttl` (content cache — not yet implemented)
- `--download` (prefetch — not yet implemented)
- `--allow-other` (FUSE passthrough)

## Notes

- `CacheVfs` supports read and write-through, a metadata cache
  (`--metadata-ttl`), and an on-disk content cache under `--cache-dir`. The cache
  is persistent: metadata, directory listings, and hydrated file content survive
  a restart, so relaunching against the same `--cache-dir` reuses it.
- `--cache-mode offline` mounts without any connection and serves the previously
  cached tree; uncached paths report not-found and writes are rejected. On
  reconnect (any online mode) the server is authoritative — content that differs
  from the remote is dropped and re-fetched.
- On Windows the binary delay-loads WinFsp (see `build.rs`); the WinFsp runtime
  must be installed for an actual mount.
