//! The WinFsp [`FileSystemContext`] adapter.
//!
//! This translates WinFsp's path/handle callbacks into [`VirtualFilesystem`]
//! operations and maps shared-core errors to `NTSTATUS`. It deliberately keeps
//! no SSH, cache-policy, or path-normalization logic of its own — that all
//! lives behind the `VirtualFilesystem` it is handed.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cacheshfs_core::{Error, FileHandle, FileKind, NodeId, OpenFlags, SetAttributes, VirtualFilesystem};
use winfsp::U16CStr;
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::Result as FspResult;

use crate::attr::{fill_file_info, windows_file_attributes, FILE_ATTRIBUTE_READONLY};
use crate::error::to_fsp;
use crate::path::{resolve, resolve_parent};

// `NtCreateFile` create option: create a directory rather than a file.
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
// WinFsp cleanup flag indicating the file should be deleted now.
const FSP_CLEANUP_DELETE: u32 = 1;

// Access-right bits used to decide whether an open needs write access.
const FILE_WRITE_DATA: u32 = 0x0002;
const FILE_APPEND_DATA: u32 = 0x0004;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;

/// The filesystem context handed to WinFsp; thin wrapper over the shared VFS.
pub struct CacheFs {
    vfs: Arc<dyn VirtualFilesystem>,
}

impl CacheFs {
    pub fn new(vfs: Arc<dyn VirtualFilesystem>) -> Self {
        Self { vfs }
    }
}

/// Per-open state. WinFsp only ever hands this back by shared reference, so any
/// mutable state uses interior mutability.
pub struct CacheFile {
    node: NodeId,
    /// `Some` for files (an open VFS handle); `None` for directories.
    handle: Option<FileHandle>,
    is_dir: bool,
    /// Buffer owned by the WinFsp driver for directory enumeration.
    dir_buffer: DirBuffer,
    /// Set by `set_delete`; acted on in `cleanup`.
    delete_pending: AtomicBool,
}

/// Derive shared-core open flags from WinFsp's create options / granted access.
fn open_flags(_create_options: u32, granted_access: u32) -> OpenFlags {
    let write =
        granted_access & (FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_ATTRIBUTES) != 0;
    OpenFlags {
        read: true,
        write,
        append: granted_access & FILE_APPEND_DATA != 0,
        // Truncation is requested via `overwrite`, never plain `open`.
        truncate: false,
    }
}

/// Best-effort Unix mode bits for a newly created node given its WinFsp
/// attribute flags.
fn mode_for_create(file_attributes: u32, is_dir: bool) -> u32 {
    let read_only = file_attributes & FILE_ATTRIBUTE_READONLY != 0;
    match (is_dir, read_only) {
        (true, true) => 0o555,
        (true, false) => 0o755,
        (false, true) => 0o444,
        (false, false) => 0o644,
    }
}

impl FileSystemContext for CacheFs {
    type FileContext = CacheFile;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> FspResult<FileSecurity> {
        let metadata = resolve(self.vfs.as_ref(), file_name).map_err(to_fsp)?;
        Ok(FileSecurity {
            reparse: false,
            // We don't supply ACLs (persistent_acls is disabled); WinFsp uses a
            // permissive default descriptor.
            sz_security_descriptor: 0,
            attributes: windows_file_attributes(&metadata.attributes),
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> FspResult<Self::FileContext> {
        let metadata = resolve(self.vfs.as_ref(), file_name).map_err(to_fsp)?;
        let is_dir = matches!(metadata.attributes.kind, FileKind::Directory);

        let handle = if is_dir {
            None
        } else {
            let flags = open_flags(create_options, granted_access);
            Some(self.vfs.open(metadata.node, flags).map_err(to_fsp)?)
        };

        fill_file_info(&metadata.attributes, metadata.node.0, file_info.as_mut());
        Ok(CacheFile {
            node: metadata.node,
            handle,
            is_dir,
            dir_buffer: DirBuffer::new(),
            delete_pending: AtomicBool::new(false),
        })
    }

    fn close(&self, context: Self::FileContext) {
        if let Some(handle) = context.handle {
            // `close` cannot report failure; releasing is best-effort.
            let _ = self.vfs.release(handle);
        }
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        granted_access: u32,
        file_attributes: u32,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> FspResult<Self::FileContext> {
        let (parent, name) = resolve_parent(self.vfs.as_ref(), file_name).map_err(to_fsp)?;

        if create_options & FILE_DIRECTORY_FILE != 0 {
            let mode = mode_for_create(file_attributes, true);
            let metadata = self.vfs.mkdir(parent, &name, mode).map_err(to_fsp)?;
            fill_file_info(&metadata.attributes, metadata.node.0, file_info.as_mut());
            Ok(CacheFile {
                node: metadata.node,
                handle: None,
                is_dir: true,
                dir_buffer: DirBuffer::new(),
                delete_pending: AtomicBool::new(false),
            })
        } else {
            let mode = mode_for_create(file_attributes, false);
            let flags = open_flags(create_options, granted_access);
            let created = self
                .vfs
                .create(parent, &name, mode, flags)
                .map_err(to_fsp)?;
            fill_file_info(
                &created.metadata.attributes,
                created.metadata.node.0,
                file_info.as_mut(),
            );
            Ok(CacheFile {
                node: created.metadata.node,
                handle: Some(created.handle),
                is_dir: false,
                dir_buffer: DirBuffer::new(),
                delete_pending: AtomicBool::new(false),
            })
        }
    }

    fn cleanup(&self, context: &Self::FileContext, file_name: Option<&U16CStr>, flags: u32) {
        if flags & FSP_CLEANUP_DELETE == 0 || !context.delete_pending.load(Ordering::SeqCst) {
            return;
        }
        // `file_name` is supplied by WinFsp when a delete is requested.
        let Some(file_name) = file_name else { return };
        let Ok((parent, name)) = resolve_parent(self.vfs.as_ref(), file_name) else {
            return;
        };
        // Cleanup cannot report failure (a Windows limitation), so errors are
        // dropped here; a failed unlink simply leaves the entry in place.
        let _ = if context.is_dir {
            self.vfs.rmdir(parent, &name)
        } else {
            self.vfs.unlink(parent, &name)
        };
    }

    fn flush(
        &self,
        context: Option<&Self::FileContext>,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        // `None` means flush the whole volume; we have nothing buffered.
        if let Some(context) = context {
            if let Some(handle) = context.handle {
                self.vfs.flush(handle).map_err(to_fsp)?;
            }
            let metadata = self.vfs.getattr(context.node).map_err(to_fsp)?;
            fill_file_info(&metadata.attributes, context.node.0, file_info);
        }
        Ok(())
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        let metadata = self.vfs.getattr(context.node).map_err(to_fsp)?;
        fill_file_info(&metadata.attributes, context.node.0, file_info);
        Ok(())
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        // Overwrite-on-open truncates the file to zero length.
        self.vfs
            .setattr(
                context.node,
                SetAttributes {
                    size: Some(0),
                    ..Default::default()
                },
            )
            .map_err(to_fsp)?;
        let metadata = self.vfs.getattr(context.node).map_err(to_fsp)?;
        fill_file_info(&metadata.attributes, context.node.0, file_info);
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> FspResult<u32> {
        // Fill the driver-managed buffer once (reset when starting a fresh
        // enumeration, i.e. when the resume marker is absent), then let WinFsp
        // page through it using the marker.
        if let Ok(lock) = context.dir_buffer.acquire(marker.is_none(), None) {
            let dir_meta = self.vfs.getattr(context.node).map_err(to_fsp)?;

            // WinFsp expects the "." and ".." pseudo-entries. We don't track a
            // parent NodeId, so ".." reuses this directory's attributes, which
            // is sufficient for enumeration.
            for dot in [".", ".."] {
                let mut entry: DirInfo = DirInfo::new();
                entry.set_name(dot)?;
                fill_file_info(&dir_meta.attributes, context.node.0, entry.file_info_mut());
                lock.write(&mut entry)?;
            }

            for child in self.vfs.readdir(context.node).map_err(to_fsp)? {
                let mut entry: DirInfo = DirInfo::new();
                entry.set_name(child.name.as_str())?;
                fill_file_info(
                    &child.metadata.attributes,
                    child.metadata.node.0,
                    entry.file_info_mut(),
                );
                lock.write(&mut entry)?;
            }
        }
        Ok(context.dir_buffer.read(marker, buffer))
    }

    fn rename(
        &self,
        _context: &Self::FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        _replace_if_exists: bool,
    ) -> FspResult<()> {
        let (old_parent, old_name) = resolve_parent(self.vfs.as_ref(), file_name).map_err(to_fsp)?;
        let (new_parent, new_name) =
            resolve_parent(self.vfs.as_ref(), new_file_name).map_err(to_fsp)?;
        self.vfs
            .rename(old_parent, &old_name, new_parent, &new_name)
            .map_err(to_fsp)
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        file_attributes: u32,
        _creation_time: u64,
        last_access_time: u64,
        last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        let mut attributes = SetAttributes::default();

        // WinFsp passes 0 for "leave unchanged"; INVALID_FILE_ATTRIBUTES
        // (0xFFFFFFFF) likewise means no change for the attribute mask.
        if last_write_time != 0 {
            attributes.modified_unix_seconds =
                Some(crate::attr::filetime_to_unix(last_write_time));
        }
        if last_access_time != 0 {
            attributes.accessed_unix_seconds =
                Some(crate::attr::filetime_to_unix(last_access_time));
        }
        if file_attributes != 0 && file_attributes != u32::MAX {
            // Best-effort: only the read-only bit maps onto Unix mode bits.
            attributes.mode = Some(if file_attributes & FILE_ATTRIBUTE_READONLY != 0 {
                0o444
            } else {
                0o644
            });
        }

        self.vfs.setattr(context.node, attributes).map_err(to_fsp)?;
        let metadata = self.vfs.getattr(context.node).map_err(to_fsp)?;
        fill_file_info(&metadata.attributes, context.node.0, file_info);
        Ok(())
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> FspResult<()> {
        // Per the contract, never delete here — just record intent for cleanup.
        context.delete_pending.store(delete_file, Ordering::SeqCst);
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        // We don't model allocation size separately from file size; only act on
        // real end-of-file changes.
        if !set_allocation_size {
            self.vfs
                .setattr(
                    context.node,
                    SetAttributes {
                        size: Some(new_size),
                        ..Default::default()
                    },
                )
                .map_err(to_fsp)?;
        }
        let metadata = self.vfs.getattr(context.node).map_err(to_fsp)?;
        fill_file_info(&metadata.attributes, context.node.0, file_info);
        Ok(())
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> FspResult<u32> {
        let handle = context
            .handle
            .ok_or_else(|| to_fsp(Error::InvalidInput("read on a directory".to_string())))?;
        let data = self
            .vfs
            .read(handle, offset, buffer.len() as u32)
            .map_err(to_fsp)?;
        let n = data.len().min(buffer.len());
        buffer[..n].copy_from_slice(&data[..n]);
        Ok(n as u32)
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> FspResult<u32> {
        let handle = context
            .handle
            .ok_or_else(|| to_fsp(Error::InvalidInput("write on a directory".to_string())))?;

        // `write_to_eof` (append) and `constrained_io` (don't extend the file)
        // both need the current size.
        let (offset, data): (u64, &[u8]) = if write_to_eof || constrained_io {
            let size = self.vfs.getattr(context.node).map_err(to_fsp)?.attributes.size;
            let offset = if write_to_eof { size } else { offset };
            if constrained_io {
                if offset >= size {
                    // Nothing fits within the current file bounds.
                    let metadata = self.vfs.getattr(context.node).map_err(to_fsp)?;
                    fill_file_info(&metadata.attributes, context.node.0, file_info);
                    return Ok(0);
                }
                let max = (size - offset) as usize;
                (offset, &buffer[..buffer.len().min(max)])
            } else {
                (offset, buffer)
            }
        } else {
            (offset, buffer)
        };

        let written = self.vfs.write(handle, offset, data).map_err(to_fsp)?;
        let metadata = self.vfs.getattr(context.node).map_err(to_fsp)?;
        fill_file_info(&metadata.attributes, context.node.0, file_info);
        Ok(written)
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> FspResult<()> {
        // The remote has no portable capacity figure, so report a large
        // synthetic volume. This is purely cosmetic for tools that query it.
        out_volume_info.total_size = 1 << 40; // 1 TiB
        out_volume_info.free_size = 1 << 39; // 512 GiB
        out_volume_info.set_volume_label("cacheshfs");
        Ok(())
    }
}
