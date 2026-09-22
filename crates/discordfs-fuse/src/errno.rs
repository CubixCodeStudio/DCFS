//! Errno mapping from client errors to libc error codes.

use crate::client::ClientError;

/// Convert a client error to a libc errno value.
pub fn to_errno(err: &ClientError) -> i32 {
    match err {
        ClientError::NotFound => libc::ENOENT,
        ClientError::AlreadyExists => libc::EEXIST,
        ClientError::InvalidName => libc::EINVAL,
        ClientError::Conflict(_) => libc::EEXIST,
        ClientError::DirectoryNotEmpty => libc::ENOTEMPTY,
        ClientError::Unauthorized => libc::EACCES,
        ClientError::BackendUnavailable => libc::EIO,
        ClientError::InvalidRequest(_) => libc::EINVAL,
        ClientError::Http(_) => libc::EIO,
        ClientError::Io(e) => e.raw_os_error().unwrap_or(libc::EIO),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_maps_to_enoent() {
        assert_eq!(to_errno(&ClientError::NotFound), libc::ENOENT);
    }

    #[test]
    fn directory_not_empty_maps_to_enotempty() {
        assert_eq!(to_errno(&ClientError::DirectoryNotEmpty), libc::ENOTEMPTY);
    }

    #[test]
    fn already_exists_maps_to_eexist() {
        assert_eq!(to_errno(&ClientError::AlreadyExists), libc::EEXIST);
    }

    #[test]
    fn invalid_name_maps_to_einval() {
        assert_eq!(to_errno(&ClientError::InvalidName), libc::EINVAL);
    }

    #[test]
    fn unauthorized_maps_to_eacces() {
        assert_eq!(to_errno(&ClientError::Unauthorized), libc::EACCES);
    }

    #[test]
    fn backend_unavailable_maps_to_eio() {
        assert_eq!(to_errno(&ClientError::BackendUnavailable), libc::EIO);
    }
}
