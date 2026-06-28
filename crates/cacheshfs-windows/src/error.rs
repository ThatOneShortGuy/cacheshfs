//! Translation of [`cacheshfs_core::Error`] into WinFsp `NTSTATUS` codes.
//!
//! The platform contract requires the mount adapter to convert shared core
//! errors into the OS filesystem error representation. WinFsp expects an
//! `NTSTATUS`, which [`winfsp::FspError::NTSTATUS`] wraps directly.

use cacheshfs_core::Error;
use winfsp::FspError;

// Standard NTSTATUS values from `ntstatus.h`. These are part of the stable
// Windows ABI, so we define the handful we need locally rather than pulling in
// the `windows` crate just for a few constants.
const STATUS_NOT_IMPLEMENTED: i32 = 0xC000_0002u32 as i32;
const STATUS_INVALID_PARAMETER: i32 = 0xC000_000Du32 as i32;
const STATUS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
const STATUS_OBJECT_NAME_COLLISION: i32 = 0xC000_0035u32 as i32;
const STATUS_DEVICE_NOT_READY: i32 = 0xC000_00A3u32 as i32;
const STATUS_NOT_SUPPORTED: i32 = 0xC000_00BBu32 as i32;
const STATUS_UNEXPECTED_NETWORK_ERROR: i32 = 0xC000_00C4u32 as i32;
const STATUS_INTERNAL_ERROR: i32 = 0xC000_00E5u32 as i32;

/// Map a shared-core error to the WinFsp error carrying the equivalent
/// `NTSTATUS`.
pub fn to_fsp(err: Error) -> FspError {
    let status = match err {
        Error::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
        Error::AlreadyExists => STATUS_OBJECT_NAME_COLLISION,
        Error::PermissionDenied => STATUS_ACCESS_DENIED,
        Error::InvalidInput(_) => STATUS_INVALID_PARAMETER,
        Error::UnsupportedOperation(_) => STATUS_NOT_IMPLEMENTED,
        Error::Unavailable(_) => STATUS_DEVICE_NOT_READY,
        Error::RemoteBackend(_) => STATUS_UNEXPECTED_NETWORK_ERROR,
        Error::MountBackend(_) => STATUS_INTERNAL_ERROR,
        // Should never surface inside a callback, but map it to something sane.
        Error::UnsupportedPlatform(_) => STATUS_NOT_SUPPORTED,
    };
    FspError::NTSTATUS(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ntstatus(err: Error) -> i32 {
        match to_fsp(err) {
            FspError::NTSTATUS(s) => s,
            other => panic!("expected NTSTATUS, got {other:?}"),
        }
    }

    #[test]
    fn maps_core_errors_to_expected_ntstatus() {
        assert_eq!(ntstatus(Error::NotFound), STATUS_OBJECT_NAME_NOT_FOUND);
        assert_eq!(ntstatus(Error::AlreadyExists), STATUS_OBJECT_NAME_COLLISION);
        assert_eq!(ntstatus(Error::PermissionDenied), STATUS_ACCESS_DENIED);
        assert_eq!(
            ntstatus(Error::InvalidInput("x".into())),
            STATUS_INVALID_PARAMETER
        );
        assert_eq!(
            ntstatus(Error::UnsupportedOperation("x")),
            STATUS_NOT_IMPLEMENTED
        );
        assert_eq!(
            ntstatus(Error::RemoteBackend("x".into())),
            STATUS_UNEXPECTED_NETWORK_ERROR
        );
        // All NTSTATUS error codes have the high "severity: error" bit set.
        assert!(ntstatus(Error::NotFound) < 0);
    }
}
