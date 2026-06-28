use cacheshfs_core::{FileAttributes, FileKind, FileMetadata};
use fuser::{FileAttr, FileType, INodeNo};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_TTL: Duration = Duration::from_secs(1);
const DEFAULT_BLOCK_SIZE: u32 = 4096;

pub fn file_attr(metadata: &FileMetadata) -> FileAttr {
    let attributes = &metadata.attributes;

    FileAttr {
        ino: INodeNo(metadata.node.0),
        size: attributes.size,
        blocks: attributes.size.div_ceil(512),
        atime: unix_time(attributes.accessed_unix_seconds),
        mtime: unix_time(attributes.modified_unix_seconds),
        ctime: unix_time(attributes.changed_unix_seconds),
        crtime: UNIX_EPOCH,
        kind: file_type(attributes.kind),
        perm: permissions(attributes),
        nlink: link_count(attributes.kind),
        uid: attributes.uid,
        gid: attributes.gid,
        rdev: 0,
        blksize: DEFAULT_BLOCK_SIZE,
        flags: 0,
    }
}

pub fn file_type(kind: FileKind) -> FileType {
    match kind {
        FileKind::File => FileType::RegularFile,
        FileKind::Directory => FileType::Directory,
        FileKind::Symlink => FileType::Symlink,
    }
}

fn unix_time(seconds: Option<i64>) -> SystemTime {
    match seconds {
        Some(seconds) if seconds >= 0 => UNIX_EPOCH + Duration::from_secs(seconds as u64),
        Some(seconds) => UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs()),
        None => UNIX_EPOCH,
    }
}

fn permissions(attributes: &FileAttributes) -> u16 {
    let mode = attributes.mode & 0o7777;

    if mode == 0 {
        match attributes.kind {
            FileKind::Directory => 0o755,
            FileKind::File => 0o644,
            FileKind::Symlink => 0o777,
        }
    } else {
        mode as u16
    }
}

fn link_count(kind: FileKind) -> u32 {
    match kind {
        FileKind::Directory => 2,
        FileKind::File | FileKind::Symlink => 1,
    }
}
