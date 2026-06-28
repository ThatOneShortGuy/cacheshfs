// On Windows the CLI binary transitively links WinFsp (through
// cacheshfs-windows), which is only available via delay-loading. The
// `/DELAYLOAD` linker flag emitted by dependency build scripts does not
// propagate to this binary, so we must emit it here too. Without it the
// executable fails to start with STATUS_DLL_NOT_FOUND. On other targets the
// winfsp build-dependency is absent and this is a no-op.
fn main() {
    #[cfg(target_os = "windows")]
    winfsp::build::winfsp_link_delayload();
}
