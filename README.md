# cacheshfs

Mount a remote directory over SSH as a local filesystem, with an optional
persistent local cache. It feels like `sshfs`, but gives you explicit control
over whether files are read straight from the server, cached on first access, or
served entirely from a local cache while offline.

- **SFTP transport** on a pure-Rust SSH stack (`russh` + `ring`) — no OpenSSL/C
  dependency, and modern key types including **ed25519** work everywhere.
- **Persistent cache** — metadata, directory listings, and file contents survive
  a restart, so a previously browsed tree can be served with no connection.
- **Server-authoritative** — on reconnect, anything that differs from the server
  is dropped and re-fetched; there is no local dirty state to reconcile.
- **Cross-platform boundary** — Linux (FUSE) and Windows (WinFsp) mount backends
  sit behind a shared, platform-neutral filesystem model.

> Status: early. The read path, write-through, metadata + content caching,
> persistence, and offline mode are implemented and unit-tested. End-to-end mount
> testing is ongoing. Not yet production-ready.

## Install / build

Requires a recent Rust toolchain (edition 2024).

```sh
cargo build --release
# the binary is target/release/cacheshfs
```

Platform mount backends have extra build/runtime requirements:

- **Linux** — libfuse (the `fuser` crate links against it).
- **Windows** — the [WinFsp](https://winfsp.dev/) runtime installed, plus
  LLVM/libclang and the MSVC/Windows SDK headers on `PATH` to build (easiest from
  a *Developer PowerShell for VS*). See
  [`crates/cacheshfs-windows/README.md`](crates/cacheshfs-windows/README.md).

A bare `cargo build`/`cargo run` at the workspace root works on any platform: the
mount crate for the current OS is built and the other is compiled as an inert
stub.

## Usage

```text
cacheshfs [OPTIONS] <[user@]host:/remote/path> <mountpoint>
```

The `mountpoint` is a drive letter or directory on Windows, or a directory on
Linux.

```sh
# Cache files on first access (the default mode)
cacheshfs alice@example.com:/srv/data /mnt/data

# Use a host configured in ~/.ssh/config
cacheshfs server:/home/me/projects Z:

# Read-only, and trust cached data without a connection
cacheshfs --read-only --cache-mode offline server:/home/me/projects Z:

# From Linux, mount a drive on a Windows host running OpenSSH
cacheshfs me@winbox:/C:/Users/me/Documents /mnt/winbox
```

### Mounting a Windows host's drive from Linux

A Windows machine running an SSH server (e.g. the built-in OpenSSH Server)
exposes its drives through SFTP with forward slashes and a drive-letter prefix,
so a path looks like `/C:/Users/me/Documents`. Only the **first** colon separates
the host from the remote path, so the drive colon is preserved:

```sh
cacheshfs me@winbox:/C:/Users/me/Documents /mnt/winbox
```

- `me@winbox` — the SSH target (also resolvable via `~/.ssh/config`).
- `/C:/Users/me/Documents` — the remote root on the Windows host; use `/D:/...`
  for another drive, or `/C:/` for the whole `C:` drive.
- `/mnt/winbox` — the local Linux mountpoint (must exist).

The Windows host only needs an SSH/SFTP server; it does **not** need cacheshfs or
WinFsp installed — those are for mounting *on* Windows.

The host alias is resolved against your OpenSSH config (`~/.ssh/config` by
default, or `--ssh-config <path>`): a matching `Host` block supplies `HostName`,
`User`, `Port`, and `IdentityFile`. An explicit `user@`, `--port`, or
`--identity-file` overrides it.

Unknown SSH host keys prompt for trust-on-first-use (OpenSSH style, showing the
SHA256 fingerprint); a *changed* key is always rejected. `--accept-unknown-host-key`
skips the prompt (insecure).

Run `cacheshfs --help` for full option descriptions.

### Cache modes (`--cache-mode`)

| Mode | Behavior |
| --- | --- |
| `remote` | Pass-through; nothing is cached locally. |
| `on-demand` *(default)* | Cache files/metadata on access; revalidate and re-fetch when the server copy changes. |
| `pinned` | Currently behaves like `on-demand` (a keep-resident/prefetch policy is not yet implemented). |
| `offline` | Serve only from the persistent cache; never connect. Uncached paths report not-found and writes are rejected. |

## Workspace layout

| Crate | Role |
| --- | --- |
| [`cacheshfs`](crates/cacheshfs-cli) | CLI binary: argument parsing, wiring the transport and cache to the mount backend. |
| [`cacheshfs-core`](crates/cacheshfs-core) | Shared, platform-neutral VFS model, the persistent cache store, and common config/error types. |
| [`cacheshfs-sftp`](crates/cacheshfs-sftp) | SSH/SFTP transport implementing `RemoteFilesystem`. |
| [`cacheshfs-linux`](crates/cacheshfs-linux) | Linux mount adapter over `fuser` (FUSE). |
| [`cacheshfs-windows`](crates/cacheshfs-windows) | Windows mount adapter over WinFsp. |

The mount backends only speak to the `VirtualFilesystem` in `cacheshfs-core`;
SSH, cache policy, and path normalization stay behind that boundary. See
[`docs/spec.md`](docs/spec.md) for the full design and
[`docs/implementation-contracts.md`](docs/implementation-contracts.md) for the
trait contracts.

## Development

```sh
# Build/test the cross-platform crates (works on Windows and Linux)
cargo test -p cacheshfs-core -p cacheshfs
cargo clippy -p cacheshfs-core -p cacheshfs

# On Linux, the whole workspace including the FUSE backend
cargo test --workspace
```
