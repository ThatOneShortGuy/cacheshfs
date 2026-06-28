use cacheshfs_core::{FileAttributes, FileKind, FileMetadata};
use fuser::{FileAttr, FileType, INodeNo};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_TTL: Duration = Duration::from_secs(1);
const DEFAULT_BLOCK_SIZE: u32 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinuxOwner {
    pub uid: u32,
    pub gid: u32,
}

pub fn file_attr(metadata: &FileMetadata, owner: LinuxOwner) -> FileAttr {
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
        uid: owner.uid,
        gid: owner.gid,
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

#[cfg(test)]
mod tests {
    use super::*;
    use cacheshfs_core::{FileAttributes, FileKind, FileMetadata, NodeId};

    const TEST_OWNER: LinuxOwner = LinuxOwner {
        uid: 1000,
        gid: 1001,
    };

    #[test]
    fn converts_regular_file_metadata_to_fuser_attr() {
        let metadata = FileMetadata {
            node: NodeId(42),
            attributes: FileAttributes {
                kind: FileKind::File,
                size: 1025,
                mode: 0o100640,
                uid: 1000,
                gid: 1001,
                modified_unix_seconds: Some(10),
                accessed_unix_seconds: Some(20),
                changed_unix_seconds: Some(30),
            },
        };

        let attr = file_attr(&metadata, TEST_OWNER);

        assert_eq!(attr.ino, INodeNo(42));
        assert_eq!(attr.size, 1025);
        assert_eq!(attr.blocks, 3);
        assert_eq!(attr.kind, FileType::RegularFile);
        assert_eq!(attr.perm, 0o640);
        assert_eq!(attr.nlink, 1);
        assert_eq!(attr.uid, 1000);
        assert_eq!(attr.gid, 1001);
        assert_eq!(attr.blksize, DEFAULT_BLOCK_SIZE);
    }

    #[test]
    fn supplies_default_permissions_when_remote_mode_is_missing() {
        assert_eq!(
            file_attr(&metadata(FileKind::File, 0), TEST_OWNER).perm,
            0o644
        );
        assert_eq!(
            file_attr(&metadata(FileKind::Directory, 0), TEST_OWNER).perm,
            0o755
        );
        assert_eq!(
            file_attr(&metadata(FileKind::Symlink, 0), TEST_OWNER).perm,
            0o777
        );
    }

    #[test]
    fn maps_file_kinds_to_fuser_types_and_link_counts() {
        let directory = file_attr(&metadata(FileKind::Directory, 0o755), TEST_OWNER);
        let symlink = file_attr(&metadata(FileKind::Symlink, 0o777), TEST_OWNER);

        assert_eq!(directory.kind, FileType::Directory);
        assert_eq!(directory.nlink, 2);
        assert_eq!(symlink.kind, FileType::Symlink);
        assert_eq!(symlink.nlink, 1);
    }

    #[test]
    fn maps_remote_ownership_to_mount_owner() {
        let attr = file_attr(&metadata(FileKind::File, 0o644), TEST_OWNER);

        assert_eq!(attr.uid, TEST_OWNER.uid);
        assert_eq!(attr.gid, TEST_OWNER.gid);
    }

    fn metadata(kind: FileKind, mode: u32) -> FileMetadata {
        FileMetadata {
            node: NodeId(1),
            attributes: FileAttributes {
                kind,
                size: 0,
                mode,
                uid: 0,
                gid: 0,
                modified_unix_seconds: None,
                accessed_unix_seconds: None,
                changed_unix_seconds: None,
            },
        }
    }
}
