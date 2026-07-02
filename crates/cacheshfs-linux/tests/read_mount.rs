#![cfg(target_os = "linux")]

use cacheshfs_core::{
    CacheMode, CreatedFile, DirectoryEntry, Error, FileAttributes, FileHandle, FileKind,
    FileMetadata, MountBackend, MountConfig, NodeId, OpenFlags, RemoteConfig, Result,
    SetAttributes, VirtualFilesystem,
};
use cacheshfs_linux::LinuxMountBackend;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const FILE_CONTENT: &[u8] = b"hello from cacheshfs through fuse\n";
const FILE_NAME: &str = "readme.txt";
const FILE_NODE: NodeId = NodeId(2);
const FILE_HANDLE: FileHandle = FileHandle(1);

struct ReadOnlyDemoVfs;

impl VirtualFilesystem for ReadOnlyDemoVfs {
    fn lookup(&self, parent: NodeId, name: &str) -> Result<FileMetadata> {
        if parent == NodeId::ROOT && name == FILE_NAME {
            Ok(file_metadata())
        } else {
            Err(Error::NotFound)
        }
    }

    fn getattr(&self, node: NodeId) -> Result<FileMetadata> {
        match node {
            NodeId::ROOT => Ok(FileMetadata {
                node,
                attributes: FileAttributes {
                    kind: FileKind::Directory,
                    size: 0,
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                    modified_unix_seconds: None,
                    accessed_unix_seconds: None,
                    changed_unix_seconds: None,
                },
            }),
            FILE_NODE => Ok(file_metadata()),
            _ => Err(Error::NotFound),
        }
    }

    fn readdir(&self, node: NodeId) -> Result<Vec<DirectoryEntry>> {
        if node != NodeId::ROOT {
            return Err(Error::NotFound);
        }

        Ok(vec![DirectoryEntry {
            name: FILE_NAME.to_string(),
            metadata: file_metadata(),
        }])
    }

    fn open(&self, node: NodeId, flags: OpenFlags) -> Result<FileHandle> {
        if node != FILE_NODE {
            return Err(Error::NotFound);
        }
        if flags.write || flags.append || flags.truncate {
            return Err(Error::UnsupportedOperation("demo filesystem is read-only"));
        }

        Ok(FILE_HANDLE)
    }

    fn read(&self, handle: FileHandle, offset: u64, size: u32) -> Result<Vec<u8>> {
        if handle != FILE_HANDLE {
            return Err(Error::InvalidInput("unknown file handle".to_string()));
        }

        let start = (offset as usize).min(FILE_CONTENT.len());
        let end = (start + size as usize).min(FILE_CONTENT.len());
        Ok(FILE_CONTENT[start..end].to_vec())
    }

    fn write(&self, _handle: FileHandle, _offset: u64, _data: &[u8]) -> Result<u32> {
        Err(Error::UnsupportedOperation("demo filesystem is read-only"))
    }

    fn flush(&self, _handle: FileHandle) -> Result<()> {
        Ok(())
    }

    fn release(&self, _handle: FileHandle) -> Result<()> {
        Ok(())
    }

    fn create(
        &self,
        _parent: NodeId,
        _name: &str,
        _mode: u32,
        _flags: OpenFlags,
    ) -> Result<CreatedFile> {
        Err(Error::UnsupportedOperation("demo filesystem is read-only"))
    }

    fn mkdir(&self, _parent: NodeId, _name: &str, _mode: u32) -> Result<FileMetadata> {
        Err(Error::UnsupportedOperation("demo filesystem is read-only"))
    }

    fn unlink(&self, _parent: NodeId, _name: &str) -> Result<()> {
        Err(Error::UnsupportedOperation("demo filesystem is read-only"))
    }

    fn rmdir(&self, _parent: NodeId, _name: &str) -> Result<()> {
        Err(Error::UnsupportedOperation("demo filesystem is read-only"))
    }

    fn rename(
        &self,
        _parent: NodeId,
        _name: &str,
        _new_parent: NodeId,
        _new_name: &str,
    ) -> Result<()> {
        Err(Error::UnsupportedOperation("demo filesystem is read-only"))
    }

    fn setattr(&self, _node: NodeId, _attributes: SetAttributes) -> Result<FileMetadata> {
        Err(Error::UnsupportedOperation("demo filesystem is read-only"))
    }
}

#[test]
fn mounted_fuse_filesystem_reads_file_contents() {
    if !fuse_is_available() {
        eprintln!("skipping FUSE read e2e test: /dev/fuse or fusermount3 is unavailable");
        return;
    }

    let temp = unique_temp_dir();
    let mountpoint = temp.join("mnt");
    let cache_dir = temp.join("cache");
    fs::create_dir_all(&mountpoint).expect("create mountpoint");
    fs::create_dir_all(&cache_dir).expect("create cache dir");

    let config = MountConfig {
        remote: RemoteConfig {
            target: "demo".to_string(),
            root: "/".to_string(),
        },
        mountpoint: mountpoint.clone(),
        cache_dir,
        cache_mode: CacheMode::Remote,
        cache_chunk_size: cacheshfs_core::DEFAULT_CACHE_CHUNK_SIZE,
        read_only: true,
    };

    let mountpoint_for_thread = mountpoint.clone();
    let mount_thread =
        thread::spawn(move || LinuxMountBackend.mount(config, Arc::new(ReadOnlyDemoVfs)));

    wait_for_mount(&mountpoint).expect("mount should become active");

    let contents = fs::read_to_string(mountpoint.join(FILE_NAME)).expect("read mounted file");
    assert_eq!(contents.as_bytes(), FILE_CONTENT);

    unmount(&mountpoint_for_thread).expect("unmount fuse filesystem");
    let mount_result = mount_thread.join().expect("mount thread should not panic");
    mount_result.expect("mount should exit cleanly after unmount");

    let _ = fs::remove_dir_all(temp);
}

fn file_metadata() -> FileMetadata {
    FileMetadata {
        node: FILE_NODE,
        attributes: FileAttributes {
            kind: FileKind::File,
            size: FILE_CONTENT.len() as u64,
            mode: 0o644,
            uid: 0,
            gid: 0,
            modified_unix_seconds: None,
            accessed_unix_seconds: None,
            changed_unix_seconds: None,
        },
    }
}

fn fuse_is_available() -> bool {
    Path::new("/dev/fuse").exists()
        && Command::new("fusermount3")
            .arg("--version")
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
}

fn unique_temp_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "cacheshfs-linux-read-e2e-{}-{nanos}",
        std::process::id()
    ))
}

fn wait_for_mount(mountpoint: &Path) -> std::io::Result<()> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if Command::new("mountpoint")
            .arg("-q")
            .arg(mountpoint)
            .status()?
            .success()
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "mountpoint did not become active",
    ))
}

fn unmount(mountpoint: &Path) -> std::io::Result<()> {
    let status = Command::new("fusermount3")
        .arg("-u")
        .arg(mountpoint)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("fusermount3 -u failed"))
    }
}
