//! The mounted-files conformance scenarios (`pdfs_vfs::conformance`) run
//! through the FUSE adapter: a daemon mounted over the fake Drive, driven with
//! ordinary file system calls. The headless adapter runs the same scenarios
//! in `pdfs-vfs`, and both have to see the same outcomes.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

use pdfs_vfs::conformance::Adapter;
use pdfs_vfs::headless::Failure;

/// A mount, addressed by paths relative to its root.
pub(crate) struct Mounted(pub(crate) PathBuf);

fn failure(error: std::io::Error) -> Failure {
    error
        .raw_os_error()
        .map_or(Failure::Io, Failure::from_errno)
}

impl Adapter for Mounted {
    type Fd = File;

    fn create(&self, path: &str) -> Result<File, Failure> {
        File::create(self.0.join(path)).map_err(failure)
    }

    fn open_write(&self, path: &str) -> Result<File, Failure> {
        OpenOptions::new()
            .write(true)
            .open(self.0.join(path))
            .map_err(failure)
    }

    fn write(&self, fd: &File, offset: u64, data: &[u8]) -> Result<(), Failure> {
        fd.write_all_at(data, offset).map_err(failure)
    }

    fn set_len(&self, fd: &File, size: u64) -> Result<(), Failure> {
        fd.set_len(size).map_err(failure)
    }

    fn fsync(&self, fd: &File) -> Result<(), Failure> {
        fd.sync_all().map_err(failure)
    }

    fn close(&self, fd: File) -> Result<(), Failure> {
        // `close(2)`'s own error is the flush's, which `File` drops; flush
        // first so a failure is seen.
        fd.sync_data().map_err(failure)?;
        drop(fd);
        Ok(())
    }

    fn read(&self, path: &str) -> Result<Vec<u8>, Failure> {
        std::fs::read(self.0.join(path)).map_err(failure)
    }

    fn mkdir(&self, path: &str) -> Result<(), Failure> {
        std::fs::create_dir(self.0.join(path)).map_err(failure)
    }

    fn unlink(&self, path: &str) -> Result<(), Failure> {
        std::fs::remove_file(self.0.join(path)).map_err(failure)
    }

    fn rmdir(&self, path: &str) -> Result<(), Failure> {
        std::fs::remove_dir(self.0.join(path)).map_err(failure)
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), Failure> {
        std::fs::rename(self.0.join(from), self.0.join(to)).map_err(failure)
    }

    fn list(&self, path: &str) -> Result<Vec<String>, Failure> {
        let mut names = std::fs::read_dir(self.0.join(path))
            .map_err(failure)?
            .map(|entry| {
                entry
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .map_err(failure)
            })
            .collect::<Result<Vec<_>, _>>()?;
        names.sort();
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::mount::MountOutcome;
    use crate::sim::daemon::{Daemon, scratch, wait_until};
    use crate::sim::fake_drive::{FakeDrive, Faults};

    fn run_through_a_mount(label: &str, online: bool) {
        let drive = FakeDrive::new();
        let dir = scratch(label);
        let daemon = Daemon::start(&dir, drive.client(1, Faults::lan())).unwrap();
        daemon.client.set_online(online);
        let mount = Mounted(daemon.mountpoint.clone());
        mount.mkdir("conformance").unwrap();
        let failed = pdfs_vfs::conformance::run(&mount, "conformance");
        daemon.client.set_online(true);
        // Whatever the scenarios queued drains before the daemon stops, so the
        // stop does not race an upload.
        let drained = wait_until(Duration::from_secs(60), || {
            daemon.pending().is_ok_and(|items| items.is_empty())
        });
        assert!(matches!(daemon.stop(), Ok(MountOutcome::Shutdown)));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(failed.is_empty(), "{failed:#?}");
        assert!(drained, "the queue did not drain");
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn the_mount_passes_every_conformance_scenario_online() {
        run_through_a_mount("conformance-online", true);
    }

    #[test]
    #[ignore = "mounts FUSE: run with `cargo test -p pdfs-fuse sim:: -- --ignored`"]
    fn the_mount_passes_every_conformance_scenario_offline() {
        run_through_a_mount("conformance-offline", false);
    }
}
