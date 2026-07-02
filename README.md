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
| `on-demand` *(default)* | Cache files/metadata on access; revalidate and re-fetch when the server copy changes. If the server becomes unreachable, already-cached files and listings keep being served. |
| `pinned` | Currently behaves like `on-demand` (a keep-resident/prefetch policy is not yet implemented). |
| `offline` | Serve only from the persistent cache; never connect. Uncached paths report not-found and writes are rejected. |

## Unmounting

Stop the `cacheshfs` process to unmount. On Linux, `Ctrl-C` (SIGINT) or SIGTERM
triggers a clean unmount; you can also unmount from another terminal with
`fusermount -u <mountpoint>`, which ends the process.

If the process is killed abruptly (e.g. `kill -9`, a crash, or a closed terminal
before the signal is handled), the mountpoint can be left stale — any access
reports `Transport endpoint is not connected`. Do **not** `rm` it; unmount it:

```sh
fusermount -u <mountpoint>      # fuse2
fusermount3 -u <mountpoint>     # fuse3, if the above is not found
sudo umount <mountpoint>        # fallback
fusermount -uz <mountpoint>     # lazy unmount if it says "target is busy"
```

Then the mountpoint is a normal empty directory again and `rmdir <mountpoint>`
works.

## Automounting on Linux at boot

Use a **systemd service**, not `/etc/fstab`. cacheshfs runs in the foreground
for the whole life of the mount (it does not daemonize), which is exactly what a
`Type=simple` unit expects. fstab-style FUSE entries rely on a `mount.<fstype>`
helper that follows the `mount` calling convention (the way `sshfs` does);
cacheshfs has its own `<[user@]host:/path> <mountpoint>` CLI and ships no such
helper, so `mount`/fstab cannot invoke it.

A ready-to-edit unit is in
[`examples/systemd/cacheshfs-data.service`](examples/systemd/cacheshfs-data.service):

```ini
[Unit]
Description=cacheshfs mount of example.com:/srv/data
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=alice
Group=alice
ExecStartPre=/bin/mkdir -p /mnt/data
ExecStart=/usr/local/bin/cacheshfs alice@example.com:/srv/data /mnt/data --cache-mode on-demand
ExecStop=/bin/fusermount -u /mnt/data
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```sh
sudo cp examples/systemd/cacheshfs-data.service /etc/systemd/system/
# edit the User, paths, and remote target to match your setup
sudo systemctl daemon-reload
sudo systemctl enable --now cacheshfs-data.service
```

A few things matter specifically because this runs at boot with no terminal:

- **Pre-seed the host key.** An unknown host key triggers an interactive
  trust-on-first-use prompt, which would hang forever with no TTY. Add the
  remote's key to the service user's `~/.ssh/known_hosts` first, or pass
  `--accept-unknown-host-key` (insecure: skips verification).
- **Use a passphrase-less key.** Run the service as a real user (`User=alice`)
  so `~/.ssh/config` and key files resolve, and use a key that needs no
  passphrase (or one held by an agent) — there is nothing to prompt at boot.
  `--identity-file` pins a specific key.
- **Only the mounting user sees the mount.** `--allow-other` is not yet applied,
  so the mountpoint is accessible only to the service user, not other users or
  root.
- **Offline mounts need no network.** With `--cache-mode offline`, drop the
  `network-online.target` lines — it serves entirely from the cache and never
  connects.
- **Keep the cache outside the mountpoint** (this is enforced). The per-user
  default (`~/.cache/cacheshfs`) is fine, or set `--cache-dir` to something like
  `/var/cache/cacheshfs` owned by the service user.

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
