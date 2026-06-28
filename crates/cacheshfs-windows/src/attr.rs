//! Conversion between platform-neutral [`FileAttributes`] and the Windows
//! [`FileInfo`] structure WinFsp passes through its callbacks.

use cacheshfs_core::{FileAttributes, FileKind};
use winfsp::filesystem::FileInfo;

// Windows `FILE_ATTRIBUTE_*` flags (stable ABI values).
pub const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

// Number of 100ns ticks between the Windows FILETIME epoch (1601-01-01) and the
// Unix epoch (1970-01-01).
const UNIX_EPOCH_IN_FILETIME: i64 = 116_444_736_000_000_000;
const TICKS_PER_SECOND: i64 = 10_000_000;

/// Convert optional Unix seconds into a Windows FILETIME (100ns ticks).
/// `None` (unknown) maps to 0, which WinFsp treats as "no time available".
pub fn unix_to_filetime(secs: Option<i64>) -> u64 {
    match secs {
        Some(s) => s
            .saturating_mul(TICKS_PER_SECOND)
            .saturating_add(UNIX_EPOCH_IN_FILETIME)
            .max(0) as u64,
        None => 0,
    }
}

/// Convert a Windows FILETIME (100ns ticks) into Unix seconds.
pub fn filetime_to_unix(filetime: u64) -> i64 {
    (filetime as i64 - UNIX_EPOCH_IN_FILETIME) / TICKS_PER_SECOND
}

/// Compute the Windows file-attribute bitmask for the given core attributes.
pub fn windows_file_attributes(attrs: &FileAttributes) -> u32 {
    match attrs.kind {
        FileKind::Directory => FILE_ATTRIBUTE_DIRECTORY,
        FileKind::Symlink => FILE_ATTRIBUTE_REPARSE_POINT,
        FileKind::File => {
            // FILE_ATTRIBUTE_NORMAL is only valid when it appears alone, so a
            // read-only file is reported as READONLY without NORMAL.
            if attrs.mode & 0o222 == 0 {
                FILE_ATTRIBUTE_READONLY
            } else {
                FILE_ATTRIBUTE_NORMAL
            }
        }
    }
}

/// Round a byte size up to the next 4 KiB allocation boundary.
fn allocation_size(size: u64) -> u64 {
    const UNIT: u64 = 4096;
    size.saturating_add(UNIT - 1) / UNIT * UNIT
}

/// Populate a WinFsp [`FileInfo`] from the shared-core attributes for `node`.
pub fn fill_file_info(attrs: &FileAttributes, node: u64, info: &mut FileInfo) {
    let mtime = unix_to_filetime(attrs.modified_unix_seconds);
    let atime = unix_to_filetime(attrs.accessed_unix_seconds);
    let ctime = unix_to_filetime(attrs.changed_unix_seconds);

    info.file_attributes = windows_file_attributes(attrs);
    info.reparse_tag = 0;
    info.file_size = attrs.size;
    info.allocation_size = allocation_size(attrs.size);
    info.last_write_time = mtime;
    info.last_access_time = if atime != 0 { atime } else { mtime };
    info.change_time = if ctime != 0 { ctime } else { mtime };
    info.creation_time = mtime;
    // WinFsp uses this as the file reference number; the stable NodeId works.
    info.index_number = node;
    // Hard links are unimplemented in WinFsp and must be left at 0.
    info.hard_links = 0;
    info.ea_size = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(kind: FileKind, mode: u32) -> FileAttributes {
        FileAttributes {
            kind,
            size: 1234,
            mode,
            uid: 0,
            gid: 0,
            modified_unix_seconds: Some(1_700_000_000),
            accessed_unix_seconds: None,
            changed_unix_seconds: None,
        }
    }

    #[test]
    fn filetime_round_trip() {
        for secs in [0i64, 1, 1_700_000_000, -5] {
            assert_eq!(filetime_to_unix(unix_to_filetime(Some(secs))), secs);
        }
    }

    #[test]
    fn directory_attribute() {
        let a = windows_file_attributes(&attrs(FileKind::Directory, 0o755));
        assert_eq!(a & FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_DIRECTORY);
    }

    #[test]
    fn read_only_file_attribute() {
        let a = windows_file_attributes(&attrs(FileKind::File, 0o444));
        assert_eq!(a, FILE_ATTRIBUTE_READONLY);
        let w = windows_file_attributes(&attrs(FileKind::File, 0o644));
        assert_eq!(w, FILE_ATTRIBUTE_NORMAL);
    }

    #[test]
    fn fill_uses_modified_time_as_fallback() {
        let mut info = FileInfo::default();
        fill_file_info(&attrs(FileKind::File, 0o644), 7, &mut info);
        assert_eq!(info.file_size, 1234);
        assert_eq!(info.index_number, 7);
        assert_ne!(info.last_write_time, 0);
        // accessed/changed were None, so they fall back to the modified time.
        assert_eq!(info.last_access_time, info.last_write_time);
        assert_eq!(info.change_time, info.last_write_time);
    }
}
