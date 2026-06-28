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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_expected_core_errors_to_errno_values() {
        assert_errno(errno(Error::NotFound), Errno::ENOENT);
        assert_errno(errno(Error::AlreadyExists), Errno::EEXIST);
        assert_errno(errno(Error::PermissionDenied), Errno::EACCES);
        assert_errno(errno(Error::InvalidInput("bad".to_string())), Errno::EINVAL);
        assert_errno(
            errno(Error::UnsupportedOperation("not supported")),
            Errno::ENOSYS,
        );
        assert_errno(errno(Error::Unavailable("offline".to_string())), Errno::EIO);
        assert_errno(errno(Error::RemoteBackend("ssh".to_string())), Errno::EIO);
        assert_errno(errno(Error::MountBackend("fuse".to_string())), Errno::EIO);
    }

    fn assert_errno(actual: Errno, expected: Errno) {
        assert_eq!(
            actual.code(),
            expected.code(),
            "expected errno {expected:?}, got {actual:?}"
        );
    }
}
