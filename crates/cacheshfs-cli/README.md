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
- `--metadata-ttl`, `--content-ttl` (metadata cache — not yet implemented)
- `--download` (prefetch — not yet implemented)
- `--allow-other` (FUSE passthrough)

## Notes

- The mounted VFS is currently read-only (see `cacheshfs-core`'s `CacheVfs`),
  so writes fail until write-through lands.
- On Windows the binary delay-loads WinFsp (see `build.rs`); the WinFsp runtime
  must be installed for an actual mount.
