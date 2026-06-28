use crate::attr::{DEFAULT_TTL, file_attr, file_type};
use crate::error::errno;
use cacheshfs_core::{FileHandle, NodeId, OpenFlags, SetAttributes, VirtualFilesystem};
use fuser::{
    FileHandle as FuseFileHandle, Filesystem, FopenFlags, INodeNo, LockOwner, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use std::ffi::OsStr;
use std::sync::Arc;
use std::time::SystemTime;

pub struct LinuxFilesystem {
    filesystem: Arc<dyn VirtualFilesystem>,
}

impl LinuxFilesystem {
    pub fn new(filesystem: Arc<dyn VirtualFilesystem>) -> Self {
        Self { filesystem }
    }
}

impl Filesystem for LinuxFilesystem {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(fuser::Errno::EINVAL);
            return;
        };

        match self.filesystem.lookup(NodeId(u64::from(parent)), name) {
            Ok(metadata) => reply.entry(&DEFAULT_TTL, &file_attr(&metadata), fuser::Generation(0)),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FuseFileHandle>, reply: ReplyAttr) {
        match self.filesystem.getattr(NodeId(u64::from(ino))) {
            Ok(metadata) => reply.attr(&DEFAULT_TTL, &file_attr(&metadata)),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FuseFileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let attributes = SetAttributes {
            size,
            mode,
            accessed_unix_seconds: time_or_now(atime),
            modified_unix_seconds: time_or_now(mtime),
        };

        match self.filesystem.setattr(NodeId(u64::from(ino)), attributes) {
            Ok(metadata) => reply.attr(&DEFAULT_TTL, &file_attr(&metadata)),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(name) = name.to_str() else {
            reply.error(fuser::Errno::EINVAL);
            return;
        };

        match self
            .filesystem
            .mkdir(NodeId(u64::from(parent)), name, mode & !umask)
        {
            Ok(metadata) => reply.entry(&DEFAULT_TTL, &file_attr(&metadata), fuser::Generation(0)),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            reply.error(fuser::Errno::EINVAL);
            return;
        };

        match self.filesystem.unlink(NodeId(u64::from(parent)), name) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            reply.error(fuser::Errno::EINVAL);
            return;
        };

        match self.filesystem.rmdir(NodeId(u64::from(parent)), name) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        if !flags.is_empty() {
            reply.error(fuser::Errno::EINVAL);
            return;
        }

        let Some(name) = name.to_str() else {
            reply.error(fuser::Errno::EINVAL);
            return;
        };
        let Some(newname) = newname.to_str() else {
            reply.error(fuser::Errno::EINVAL);
            return;
        };

        match self.filesystem.rename(
            NodeId(u64::from(parent)),
            name,
            NodeId(u64::from(newparent)),
            newname,
        ) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: fuser::OpenFlags, reply: ReplyOpen) {
        match self
            .filesystem
            .open(NodeId(u64::from(ino)), open_flags(flags))
        {
            Ok(handle) => reply.opened(FuseFileHandle(handle.0), FopenFlags::empty()),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuseFileHandle,
        offset: u64,
        size: u32,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self
            .filesystem
            .read(FileHandle(u64::from(fh)), offset, size)
        {
            Ok(data) => reply.data(&data),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuseFileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self
            .filesystem
            .write(FileHandle(u64::from(fh)), offset, data)
        {
            Ok(written) => reply.written(written),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuseFileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        match self.filesystem.flush(FileHandle(u64::from(fh))) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuseFileHandle,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.filesystem.release(FileHandle(u64::from(fh))) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: fuser::OpenFlags, reply: ReplyOpen) {
        reply.opened(FuseFileHandle(0), FopenFlags::empty());
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FuseFileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let entries = match self.filesystem.readdir(NodeId(u64::from(ino))) {
            Ok(entries) => entries,
            Err(error) => {
                reply.error(errno(error));
                return;
            }
        };

        for (index, entry) in entries.into_iter().enumerate().skip(offset as usize) {
            let next_offset = (index + 1) as u64;
            if reply.add(
                INodeNo(entry.metadata.node.0),
                next_offset,
                file_type(entry.metadata.attributes.kind),
                entry.name,
            ) {
                break;
            }
        }

        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(name) = name.to_str() else {
            reply.error(fuser::Errno::EINVAL);
            return;
        };

        match self.filesystem.create(
            NodeId(u64::from(parent)),
            name,
            mode & !umask,
            open_flags(fuser::OpenFlags(flags)),
        ) {
            Ok(created) => reply.created(
                &DEFAULT_TTL,
                &file_attr(&created.metadata),
                fuser::Generation(0),
                FuseFileHandle(created.handle.0),
                FopenFlags::empty(),
            ),
            Err(error) => reply.error(errno(error)),
        }
    }
}

fn open_flags(flags: fuser::OpenFlags) -> OpenFlags {
    let access_mode = flags.acc_mode();

    OpenFlags {
        read: access_mode != fuser::OpenAccMode::O_WRONLY,
        write: access_mode != fuser::OpenAccMode::O_RDONLY,
        append: flags.0 & libc::O_APPEND == libc::O_APPEND,
        truncate: flags.0 & libc::O_TRUNC == libc::O_TRUNC,
    }
}

fn time_or_now(value: Option<fuser::TimeOrNow>) -> Option<i64> {
    match value {
        Some(fuser::TimeOrNow::SpecificTime(time)) => unix_seconds(time),
        Some(fuser::TimeOrNow::Now) => unix_seconds(SystemTime::now()),
        None => None,
    }
}

fn unix_seconds(time: SystemTime) -> Option<i64> {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => Some(duration.as_secs() as i64),
        Err(error) => Some(-(error.duration().as_secs() as i64)),
    }
}
