use cacheshfs_core::Error;
use fuser::Errno;

pub fn errno(error: Error) -> Errno {
    match error {
        Error::NotFound => Errno::ENOENT,
        Error::AlreadyExists => Errno::EEXIST,
        Error::PermissionDenied => Errno::EACCES,
        Error::InvalidInput(_) => Errno::EINVAL,
        Error::UnsupportedOperation(_) | Error::UnsupportedPlatform(_) => Errno::ENOSYS,
        Error::Unavailable(_) | Error::MountBackend(_) | Error::RemoteBackend(_) => Errno::EIO,
    }
}
