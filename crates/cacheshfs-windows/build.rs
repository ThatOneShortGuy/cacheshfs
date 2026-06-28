// WinFsp links only via delay-loading. When building for Windows we must emit
// the `/DELAYLOAD:winfsp-x64.dll` (and `delayimp`) linker flags; the helper
// reads the target config from cargo env vars. On non-Windows targets the
// `winfsp` build-dependency is absent (it is target-gated in Cargo.toml), so
// this block is compiled out and the build script is a no-op.
fn main() {
    #[cfg(target_os = "windows")]
    winfsp::build::winfsp_link_delayload();
}
