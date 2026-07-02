# cacheshfs CLI

Command-line entrypoint.

Parses arguments, builds a `MountConfig`, connects the SFTP transport, wraps it
in the shared `CacheVfs`, and dispatches to the platform `MountBackend`.

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

- `CacheVfs` supports read and write-through, plus an in-memory metadata cache
  (`--metadata-ttl`). On-disk content caching is a later layer.
- On Windows the binary delay-loads WinFsp (see `build.rs`); the WinFsp runtime
  must be installed for an actual mount.
