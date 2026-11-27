//! The POSIX rules on names and on what a rename may replace.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

/// Why a pathname component cannot name a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    /// Longer than `NAME_MAX` (255 bytes).
    TooLong,
    /// Empty, `.` or `..`.
    Reserved,
    /// Not UTF-8, which Drive requires.
    NotUtf8,
}

/// Why a rename may not replace its destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceError {
    /// A file would replace a directory.
    IsDirectory,
    /// A directory would replace a file.
    NotDirectory,
    /// The directory to replace is not empty.
    NotEmpty,
}

/// Convert one kernel pathname component into the UTF-8 name accepted by Drive.
/// Linux filesystems conventionally cap a component at `NAME_MAX` (255 bytes),
/// but the FUSE kernel path can pass a longer component through to userspace.
pub fn component(name: &OsStr) -> Result<String, NameError> {
    let bytes = name.as_bytes();
    if bytes.len() > 255 {
        return Err(NameError::TooLong);
    }
    if bytes.is_empty() || bytes == b"." || bytes == b".." {
        return Err(NameError::Reserved);
    }
    name.to_str().map(str::to_owned).ok_or(NameError::NotUtf8)
}

/// Whether a `rename` may replace an existing destination, per POSIX.
///
/// Split out from the handler because it is the part that is pure and the part
/// that is dangerous: every `Err` here is a refusal that happens *before*
/// anything is trashed, and getting one wrong turns a refusal into the
/// destruction of the destination (`docs/BUGS.md` B13).
///
/// `dst_empty` is only meaningful when `dst_dir`; pass `true` otherwise.
pub fn check_replaceable(
    src_dir: bool,
    dst_dir: bool,
    dst_empty: bool,
) -> Result<(), ReplaceError> {
    match (src_dir, dst_dir) {
        // A non-directory may not replace a directory, or vice versa.
        (false, true) => Err(ReplaceError::IsDirectory),
        (true, false) => Err(ReplaceError::NotDirectory),
        // Proton trashes a folder with its whole subtree, so replacing a
        // non-empty one would silently take its contents with it. POSIX says
        // ENOTEMPTY, which is also the safe answer.
        (true, true) if !dst_empty => Err(ReplaceError::NotEmpty),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod replace_tests {
    use super::{ReplaceError, check_replaceable};

    /// The case that motivated all of this: rsync renaming its temp file over
    /// the real one. Two plain files, and it has to be allowed — refusing is
    /// what made every rsync transfer fail at the last step (`docs/BUGS.md` B13).
    #[test]
    fn a_file_may_replace_a_file() {
        assert!(check_replaceable(false, false, true).is_ok());
    }

    #[test]
    fn an_empty_directory_may_be_replaced_by_a_directory() {
        assert!(check_replaceable(true, true, true).is_ok());
    }

    /// Proton trashes a folder with its whole subtree, so allowing this would
    /// discard every file under the destination without ever naming them.
    #[test]
    fn a_non_empty_directory_is_never_replaced() {
        let e = check_replaceable(true, true, false).expect_err("must refuse");
        assert_eq!(e, ReplaceError::NotEmpty);
    }

    #[test]
    fn the_two_ends_must_agree_on_being_a_directory() {
        assert_eq!(
            check_replaceable(false, true, true).expect_err("a file may not replace a directory"),
            ReplaceError::IsDirectory
        );
        assert_eq!(
            check_replaceable(true, false, true).expect_err("a directory may not replace a file"),
            ReplaceError::NotDirectory
        );
    }

    /// `dst_empty` describes a directory; it must not leak into the file case,
    /// where callers pass `true` by convention.
    #[test]
    fn emptiness_is_ignored_when_the_destination_is_a_file() {
        assert!(check_replaceable(false, false, false).is_ok());
    }
}
