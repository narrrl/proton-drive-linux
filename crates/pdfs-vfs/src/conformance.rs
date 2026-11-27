//! Scenarios every adapter of the mounted-files service has to pass the same
//! way: what a file holds after it is written, resized, synced, closed and
//! opened again, and what create, mkdir, unlink, rmdir and rename do with
//! names.
//!
//! [`run`] takes an [`Adapter`] and a folder to work in. The headless adapter
//! runs them in this crate's tests; `pdfs-fuse` runs them through a mount. A
//! scenario reports what it saw instead of panicking, so one run lists every
//! difference.

use crate::headless::{Failure, Fd, Headless};

/// File and folder operations by path, relative to the adapter's root.
pub trait Adapter {
    /// A file open for writing.
    type Fd;

    /// Create the file, or empty it when it exists, and open it for writing.
    fn create(&self, path: &str) -> Result<Self::Fd, Failure>;
    fn open_write(&self, path: &str) -> Result<Self::Fd, Failure>;
    fn write(&self, fd: &Self::Fd, offset: u64, data: &[u8]) -> Result<(), Failure>;
    fn set_len(&self, fd: &Self::Fd, size: u64) -> Result<(), Failure>;
    fn fsync(&self, fd: &Self::Fd) -> Result<(), Failure>;
    fn close(&self, fd: Self::Fd) -> Result<(), Failure>;
    /// The whole content of the file.
    fn read(&self, path: &str) -> Result<Vec<u8>, Failure>;
    fn mkdir(&self, path: &str) -> Result<(), Failure>;
    fn unlink(&self, path: &str) -> Result<(), Failure>;
    fn rmdir(&self, path: &str) -> Result<(), Failure>;
    fn rename(&self, from: &str, to: &str) -> Result<(), Failure>;
    /// The names in the folder, sorted.
    fn list(&self, path: &str) -> Result<Vec<String>, Failure>;
}

impl Adapter for Headless {
    type Fd = Fd;

    fn create(&self, path: &str) -> Result<Fd, Failure> {
        Headless::create(self, path)
    }
    fn open_write(&self, path: &str) -> Result<Fd, Failure> {
        Headless::open_write(self, path)
    }
    fn write(&self, fd: &Fd, offset: u64, data: &[u8]) -> Result<(), Failure> {
        Headless::write(self, fd, offset, data)
    }
    fn set_len(&self, fd: &Fd, size: u64) -> Result<(), Failure> {
        Headless::set_len(self, fd, size)
    }
    fn fsync(&self, fd: &Fd) -> Result<(), Failure> {
        Headless::fsync(self, fd)
    }
    fn close(&self, fd: Fd) -> Result<(), Failure> {
        Headless::close(self, fd)
    }
    fn read(&self, path: &str) -> Result<Vec<u8>, Failure> {
        Headless::read(self, path)
    }
    fn mkdir(&self, path: &str) -> Result<(), Failure> {
        Headless::mkdir(self, path)
    }
    fn unlink(&self, path: &str) -> Result<(), Failure> {
        Headless::unlink(self, path)
    }
    fn rmdir(&self, path: &str) -> Result<(), Failure> {
        Headless::rmdir(self, path)
    }
    fn rename(&self, from: &str, to: &str) -> Result<(), Failure> {
        Headless::rename(self, from, to)
    }
    fn list(&self, path: &str) -> Result<Vec<String>, Failure> {
        Headless::list(self, path)
    }
}

/// What a scenario saw that it should not have.
pub type Outcome = Result<(), String>;

/// A scenario, run in a folder of its own.
pub type Scenario<A> = fn(&A, &Dir) -> Outcome;

/// The folder a scenario works in.
pub struct Dir(String);

impl Dir {
    /// The path of `name` in this folder.
    pub fn at(&self, name: &str) -> String {
        format!("{}/{name}", self.0)
    }

    pub fn path(&self) -> &str {
        &self.0
    }
}

/// Every scenario, by name.
pub fn scenarios<A: Adapter>() -> Vec<(&'static str, Scenario<A>)> {
    vec![
        ("a_reopened_file_has_what_was_written", reopened_file::<A>),
        ("a_partial_overwrite_keeps_the_rest", partial_overwrite::<A>),
        ("a_read_while_open_sees_the_writes", read_while_open::<A>),
        (
            "a_hole_past_a_grow_reads_as_zeros",
            hole_reads_as_zeros::<A>,
        ),
        ("a_create_over_a_file_empties_it", create_empties::<A>),
        ("folders_list_their_children", folders_list::<A>),
        ("a_rename_moves_and_replaces", rename_replaces::<A>),
        ("a_rename_refuses_what_posix_refuses", rename_refusals::<A>),
        ("removing_follows_posix", remove_rules::<A>),
        (
            "an_unlinked_open_file_does_not_come_back",
            unlinked_open::<A>,
        ),
    ]
}

/// Run every scenario in its own folder under `base`, which must exist. The
/// result names each scenario that failed and why.
pub fn run<A: Adapter>(adapter: &A, base: &str) -> Vec<(&'static str, String)> {
    let mut failed = Vec::new();
    for (name, scenario) in scenarios::<A>() {
        let dir = Dir(format!("{base}/{name}"));
        let outcome = adapter
            .mkdir(dir.path())
            .map_err(|e| format!("mkdir {}: {e:?}", dir.path()))
            .and_then(|()| scenario(adapter, &dir));
        if let Err(why) = outcome {
            failed.push((name, why));
        }
    }
    failed
}

/// `Err` naming `what` unless `got` is `want`.
fn expect<T: PartialEq + std::fmt::Debug>(what: &str, got: T, want: T) -> Outcome {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: got {got:?}, want {want:?}"))
    }
}

/// The `Ok` of `result`, or `Err` naming `what`.
fn ok<T>(what: &str, result: Result<T, Failure>) -> Result<T, String> {
    result.map_err(|e| format!("{what}: {e:?}"))
}

fn write_file<A: Adapter>(a: &A, path: &str, bytes: &[u8]) -> Outcome {
    let fd = ok(&format!("create {path}"), a.create(path))?;
    ok(&format!("write {path}"), a.write(&fd, 0, bytes))?;
    ok(&format!("close {path}"), a.close(fd))
}

/// Written, resized, synced, closed and opened again: the bytes are what the
/// handle left, and a second handle adds to them.
fn reopened_file<A: Adapter>(a: &A, d: &Dir) -> Outcome {
    let f = d.at("f.txt");
    let fd = ok("create", a.create(&f))?;
    ok("write", a.write(&fd, 0, b"hello world"))?;
    ok("set_len", a.set_len(&fd, 5))?;
    ok("fsync", a.fsync(&fd))?;
    ok("close", a.close(fd))?;
    expect("after close", a.read(&f), Ok(b"hello".to_vec()))?;
    let fd = ok("reopen", a.open_write(&f))?;
    ok("append", a.write(&fd, 5, b" again"))?;
    ok("close again", a.close(fd))?;
    expect("after reopen", a.read(&f), Ok(b"hello again".to_vec()))
}

fn partial_overwrite<A: Adapter>(a: &A, d: &Dir) -> Outcome {
    let f = d.at("f.txt");
    write_file(a, &f, b"abcdef")?;
    let fd = ok("reopen", a.open_write(&f))?;
    ok("overwrite", a.write(&fd, 2, b"XY"))?;
    ok("close", a.close(fd))?;
    expect("content", a.read(&f), Ok(b"abXYef".to_vec()))
}

fn read_while_open<A: Adapter>(a: &A, d: &Dir) -> Outcome {
    let f = d.at("f.txt");
    let fd = ok("create", a.create(&f))?;
    ok("write", a.write(&fd, 0, b"draft"))?;
    let seen = a.read(&f);
    ok("close", a.close(fd))?;
    expect("while open", seen, Ok(b"draft".to_vec()))
}

fn hole_reads_as_zeros<A: Adapter>(a: &A, d: &Dir) -> Outcome {
    let f = d.at("f.bin");
    let fd = ok("create", a.create(&f))?;
    ok("write", a.write(&fd, 0, b"ab"))?;
    ok("grow", a.set_len(&fd, 4))?;
    ok("write past the end", a.write(&fd, 6, b"z"))?;
    ok("close", a.close(fd))?;
    expect("content", a.read(&f), Ok(b"ab\0\0\0\0z".to_vec()))
}

fn create_empties<A: Adapter>(a: &A, d: &Dir) -> Outcome {
    let f = d.at("f.txt");
    write_file(a, &f, b"old content")?;
    write_file(a, &f, b"new")?;
    expect("content", a.read(&f), Ok(b"new".to_vec()))
}

fn folders_list<A: Adapter>(a: &A, d: &Dir) -> Outcome {
    let sub = d.at("sub");
    ok("mkdir", a.mkdir(&sub))?;
    expect("mkdir again", a.mkdir(&sub), Err(Failure::Exists))?;
    write_file(a, &format!("{sub}/b.txt"), b"b")?;
    write_file(a, &format!("{sub}/a.txt"), b"a")?;
    expect(
        "listing",
        a.list(&sub),
        Ok(vec!["a.txt".to_owned(), "b.txt".to_owned()]),
    )?;
    expect(
        "parent listing",
        a.list(d.path()),
        Ok(vec!["sub".to_owned()]),
    )?;
    expect(
        "mkdir under a file",
        a.mkdir(&format!("{sub}/a.txt/x")),
        Err(Failure::NotDirectory),
    )
}

fn rename_replaces<A: Adapter>(a: &A, d: &Dir) -> Outcome {
    let (from, to) = (d.at("a.txt"), d.at("b.txt"));
    write_file(a, &from, b"A")?;
    write_file(a, &to, b"B")?;
    ok("rename over", a.rename(&from, &to))?;
    expect("destination", a.read(&to), Ok(b"A".to_vec()))?;
    expect("source", a.read(&from), Err(Failure::NotFound))?;
    ok("mkdir", a.mkdir(&d.at("sub")))?;
    ok("move", a.rename(&to, &d.at("sub/c.txt")))?;
    expect("moved", a.read(&d.at("sub/c.txt")), Ok(b"A".to_vec()))?;
    expect("listing", a.list(d.path()), Ok(vec!["sub".to_owned()]))
}

fn rename_refusals<A: Adapter>(a: &A, d: &Dir) -> Outcome {
    ok("mkdir", a.mkdir(&d.at("dir")))?;
    ok("mkdir inner", a.mkdir(&d.at("dir/inner")))?;
    ok("mkdir full", a.mkdir(&d.at("full")))?;
    write_file(a, &d.at("full/x"), b"x")?;
    write_file(a, &d.at("f.txt"), b"f")?;
    expect(
        "folder into itself",
        a.rename(&d.at("dir"), &d.at("dir/inner/dir")),
        Err(Failure::InvalidArgument),
    )?;
    expect(
        "file over folder",
        a.rename(&d.at("f.txt"), &d.at("dir")),
        Err(Failure::IsDirectory),
    )?;
    expect(
        "folder over file",
        a.rename(&d.at("dir"), &d.at("f.txt")),
        Err(Failure::NotDirectory),
    )?;
    expect(
        "folder over a full folder",
        a.rename(&d.at("dir"), &d.at("full")),
        Err(Failure::NotEmpty),
    )?;
    expect(
        "missing source",
        a.rename(&d.at("nothing"), &d.at("g.txt")),
        Err(Failure::NotFound),
    )?;
    expect("file kept", a.read(&d.at("f.txt")), Ok(b"f".to_vec()))?;
    expect("full kept", a.list(&d.at("full")), Ok(vec!["x".to_owned()]))
}

fn remove_rules<A: Adapter>(a: &A, d: &Dir) -> Outcome {
    ok("mkdir", a.mkdir(&d.at("dir")))?;
    write_file(a, &d.at("dir/f"), b"f")?;
    expect(
        "unlink a folder",
        a.unlink(&d.at("dir")),
        Err(Failure::IsDirectory),
    )?;
    expect(
        "rmdir a file",
        a.rmdir(&d.at("dir/f")),
        Err(Failure::NotDirectory),
    )?;
    expect(
        "rmdir a full folder",
        a.rmdir(&d.at("dir")),
        Err(Failure::NotEmpty),
    )?;
    expect(
        "unlink nothing",
        a.unlink(&d.at("nothing")),
        Err(Failure::NotFound),
    )?;
    ok("unlink", a.unlink(&d.at("dir/f")))?;
    ok("rmdir", a.rmdir(&d.at("dir")))?;
    expect("listing", a.list(d.path()), Ok(Vec::new()))
}

fn unlinked_open<A: Adapter>(a: &A, d: &Dir) -> Outcome {
    let f = d.at("f.txt");
    let fd = ok("create", a.create(&f))?;
    ok("write", a.write(&fd, 0, b"doomed"))?;
    ok("unlink", a.unlink(&f))?;
    ok("write after unlink", a.write(&fd, 6, b" still"))?;
    ok("close", a.close(fd))?;
    expect("read", a.read(&f), Err(Failure::NotFound))?;
    expect("listing", a.list(d.path()), Ok(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_headless_adapter_passes_every_scenario() {
        let dir = std::env::temp_dir().join(format!("pdfs-headless-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let headless = Headless::open(&dir).unwrap();
        headless.mkdir("conformance").unwrap();
        let failed = run(&headless, "conformance");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(failed.is_empty(), "{failed:#?}");
    }
}
