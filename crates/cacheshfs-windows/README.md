# cacheshfs-windows

Windows mount adapter crate.

Implements [`cacheshfs_core::MountBackend`] on top of [WinFsp](https://winfsp.dev/).
The adapter translates WinFsp's path/handle callbacks into `VirtualFilesystem`
calls and maps `cacheshfs_core::Error` into Windows `NTSTATUS` values. SSH, cache
policy, and remote path normalization stay out of this crate — they live behind
the `VirtualFilesystem` it is handed.

## Layout

- `lib.rs` — `WindowsMountBackend`; real impl on Windows, stub elsewhere.
- `mount.rs` — `winfsp_init`, `VolumeParams`, and the `FileSystemHost` lifecycle.
- `fs.rs` — `FileSystemContext` adapter (the bulk of the translation logic).
- `path.rs` — resolves a WinFsp path (`\a\b\c`) to a `NodeId` by walking `lookup`.
- `attr.rs` — `FileAttributes` ⇄ WinFsp `FileInfo` (attributes, FILETIME, size).
- `error.rs` — `cacheshfs_core::Error` → `NTSTATUS` mapping.

All WinFsp-specific code is gated behind `#[cfg(windows)]`; on other platforms
the backend compiles to a stub returning `Error::UnsupportedPlatform`, so the
workspace still builds everywhere. The `winfsp` dependency is target-gated to
`cfg(windows)` for the same reason.

## Build requirements (Windows)

1. **WinFsp runtime** installed (provides `winfsp-x64.dll`, loaded at runtime via
   delay-loading). The `winfsp-sys` crate bundles its own import library and
   headers, so the WinFsp *developer SDK* is **not** required to build.
2. **LLVM/libclang** on `PATH` — `winfsp-sys`' build script runs `bindgen`.
3. **Windows SDK + MSVC headers** reachable by clang. The simplest way is to
   build from a *Developer PowerShell/Command Prompt for VS*, which sets the
   `INCLUDE`/`LIB` environment variables. Otherwise set `INCLUDE` to the MSVC and
   Windows SDK `include` directories (and `LIB` for linking) before `cargo build`.

`build.rs` calls `winfsp::build::winfsp_link_delayload()` (only when targeting
Windows) to emit the required `/DELAYLOAD` linker flags.

## Verification

`cargo test -p cacheshfs-windows` runs runtime-free unit tests for path
resolution, attribute/FILETIME conversion, and error mapping (using a small
in-memory `VirtualFilesystem`).

A full end-to-end mount test is **deferred**: the shared `VirtualFilesystem`
(`cacheshfs-core`) and the SFTP transport are still stubs, so there is no real
backend to mount yet. Once they land, the manual smoke test is:

1. Run the CLI to mount onto a drive letter (e.g. `Z:`) or directory.
2. `dir Z:\`, open/read a file, create/write/delete, rename.
3. Unmount (currently by terminating the process — graceful shutdown signalling
   is a TODO in `mount.rs`).
