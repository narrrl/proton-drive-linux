//! Shared Drive-result activation policy for the GTK browser and quick prompt.
//!
//! Media should be handed to its mounted path so players can issue range reads
//! through FUSE. Other files retain the materialize-then-open behaviour because
//! many desktop applications expect a stable ordinary file.

use std::path::{Path, PathBuf};

use pdfs_core::control::PhotoKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriveActivation {
    Folder,
    MountedMedia,
    Materialize,
}

pub(crate) fn drive_activation(name: &str, is_dir: bool) -> DriveActivation {
    if is_dir {
        DriveActivation::Folder
    } else if is_streamable_media(name) {
        DriveActivation::MountedMedia
    } else {
        DriveActivation::Materialize
    }
}

pub(crate) fn mounted_path(mountpoint: &Path, relative_path: &str) -> PathBuf {
    mountpoint.join(relative_path)
}

fn is_streamable_media(name: &str) -> bool {
    if PhotoKind::classify(Some(name), None) == PhotoKind::Video {
        return true;
    }

    let extension = Path::new(name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();

    matches!(
        extension.to_ascii_lowercase().as_str(),
        "mp3" | "flac" | "wav" | "ogg" | "opus" | "m4a" | "aac"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_use_the_mount_regardless_of_extension() {
        assert_eq!(
            drive_activation("recordings.mp4", true),
            DriveActivation::Folder
        );
    }

    #[test]
    fn media_extensions_use_fuse_path_case_insensitively() {
        for name in ["movie.MKV", "voice.opus", "song.FLAC", "clip.m4v"] {
            assert_eq!(
                drive_activation(name, false),
                DriveActivation::MountedMedia,
                "{name}"
            );
        }
    }

    #[test]
    fn ordinary_files_are_materialized() {
        for name in ["report.pdf", "notes.txt", "archive.zip", "no-extension"] {
            assert_eq!(
                drive_activation(name, false),
                DriveActivation::Materialize,
                "{name}"
            );
        }
    }

    #[test]
    fn mounted_paths_preserve_relative_components() {
        assert_eq!(
            mounted_path(Path::new("/mnt/drive"), "Videos/a movie.mkv"),
            Path::new("/mnt/drive/Videos/a movie.mkv")
        );
    }
}
