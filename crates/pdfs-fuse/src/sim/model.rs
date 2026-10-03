//! The reference file system of the simulation (`docs/MILESTONE-3.0.0.md`
//! §8.2, property 4): what each syscall a simulated client makes should
//! answer, and the tree it should leave, on a POSIX file system.
//!
//! A [`Model`] covers one client's own folder; paths are relative to it. The
//! same type is used for the trees read back from a mount and from Drive.

use std::collections::BTreeMap;

use super::rng::Rng;

/// A tree by path: `None` is a folder, `Some` a file's content.
pub(crate) type Tree = BTreeMap<String, Option<Vec<u8>>>;

/// One file system call, or the few a program makes in a row.
#[derive(Clone, Debug)]
pub(crate) enum FsOp {
    /// `open(O_CREAT | O_TRUNC)`, write `data`, close.
    Create {
        path: String,
        data: Vec<u8>,
    },
    /// Open an existing file, write `data` at `offset` (past the end leaves
    /// a hole), close.
    Write {
        path: String,
        offset: u64,
        data: Vec<u8>,
    },
    /// Open an existing file and `ftruncate` it to `len`.
    Truncate {
        path: String,
        len: u64,
    },
    Mkdir {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
    Unlink {
        path: String,
    },
    /// Open a file, unlink it, append `data`, read it all back, close.
    UnlinkOpen {
        path: String,
        data: Vec<u8>,
    },
    Rmdir {
        path: String,
    },
}

impl FsOp {
    /// The paths the op names, for the log.
    pub(crate) fn describe(&self) -> String {
        match self {
            FsOp::Create { path, data } => format!("create {path} ({} bytes)", data.len()),
            FsOp::Write { path, offset, data } => {
                format!("write {path} at {offset} ({} bytes)", data.len())
            }
            FsOp::Truncate { path, len } => format!("truncate {path} to {len}"),
            FsOp::Mkdir { path } => format!("mkdir {path}"),
            FsOp::Rename { from, to } => format!("rename {from} -> {to}"),
            FsOp::Unlink { path } => format!("unlink {path}"),
            FsOp::UnlinkOpen { path, data } => {
                format!("unlink {path} while open, then append {} bytes", data.len())
            }
            FsOp::Rmdir { path } => format!("rmdir {path}"),
        }
    }
}

/// The tree one client should have, and what its syscalls should answer.
#[derive(Clone, Debug, Default)]
pub(crate) struct Model {
    tree: Tree,
}

/// Names are drawn from small pools, so ops collide: a create over an existing
/// file, a rename over another, a mkdir of a name that is taken.
const FILE_NAMES: [&str; 6] = ["a.txt", "b.txt", "c.bin", "d", "e.md", "f.txt"];
const DIR_NAMES: [&str; 3] = ["x", "y", "z"];

impl Model {
    pub(crate) fn tree(&self) -> &Tree {
        &self.tree
    }

    pub(crate) fn content(&self, path: &str) -> Option<&Vec<u8>> {
        self.tree.get(path)?.as_ref()
    }

    /// What `op` should answer on a POSIX file system, applying it on success.
    pub(crate) fn apply(&mut self, op: &FsOp) -> Result<(), i32> {
        match op {
            FsOp::Create { path, data } => {
                self.check_parent(path)?;
                match self.tree.get(path) {
                    Some(None) => Err(libc::EISDIR),
                    _ => {
                        self.tree.insert(path.clone(), Some(data.clone()));
                        Ok(())
                    }
                }
            }
            FsOp::Write { path, offset, data } => {
                let content = self.file_mut(path)?;
                // Writing nothing changes nothing, even past the end.
                if data.is_empty() {
                    return Ok(());
                }
                let end = *offset as usize + data.len();
                if content.len() < end {
                    content.resize(end, 0);
                }
                content[*offset as usize..end].copy_from_slice(data);
                Ok(())
            }
            FsOp::Truncate { path, len } => {
                self.file_mut(path)?.resize(*len as usize, 0);
                Ok(())
            }
            FsOp::Mkdir { path } => {
                self.check_parent(path)?;
                if self.tree.contains_key(path) {
                    return Err(libc::EEXIST);
                }
                self.tree.insert(path.clone(), None);
                Ok(())
            }
            FsOp::Rename { from, to } => self.rename(from, to),
            FsOp::Unlink { path } | FsOp::UnlinkOpen { path, .. } => {
                self.check_parent(path)?;
                match self.tree.get(path) {
                    None => Err(libc::ENOENT),
                    Some(None) => Err(libc::EISDIR),
                    Some(Some(_)) => {
                        self.tree.remove(path);
                        Ok(())
                    }
                }
            }
            FsOp::Rmdir { path } => {
                self.check_parent(path)?;
                match self.tree.get(path) {
                    None => Err(libc::ENOENT),
                    Some(Some(_)) => Err(libc::ENOTDIR),
                    Some(None) if self.has_children(path) => Err(libc::ENOTEMPTY),
                    Some(None) => {
                        self.tree.remove(path);
                        Ok(())
                    }
                }
            }
        }
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), i32> {
        self.check_parent(from)?;
        let Some(moving) = self.tree.get(from).cloned() else {
            return Err(libc::ENOENT);
        };
        self.check_parent(to)?;
        if from == to {
            return Ok(());
        }
        if is_below(to, from) {
            return Err(libc::EINVAL);
        }
        match (&moving, self.tree.get(to)) {
            (None, Some(Some(_))) => return Err(libc::ENOTDIR),
            (Some(_), Some(None)) => return Err(libc::EISDIR),
            (None, Some(None)) if self.has_children(to) => return Err(libc::ENOTEMPTY),
            _ => {}
        }
        let below: Vec<(String, Option<Vec<u8>>)> = self
            .tree
            .iter()
            .filter(|(path, _)| is_below(path, from))
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect();
        self.tree.remove(from);
        self.tree.insert(to.to_owned(), moving);
        for (path, entry) in below {
            self.tree.remove(&path);
            self.tree
                .insert(format!("{to}{}", &path[from.len()..]), entry);
        }
        Ok(())
    }

    fn file_mut(&mut self, path: &str) -> Result<&mut Vec<u8>, i32> {
        self.check_parent(path)?;
        match self.tree.get_mut(path) {
            None => Err(libc::ENOENT),
            Some(None) => Err(libc::EISDIR),
            Some(Some(content)) => Ok(content),
        }
    }

    fn check_parent(&self, path: &str) -> Result<(), i32> {
        let mut at = String::new();
        let mut names = path.split('/').peekable();
        while let Some(name) = names.next() {
            if names.peek().is_none() {
                break;
            }
            if !at.is_empty() {
                at.push('/');
            }
            at.push_str(name);
            match self.tree.get(&at) {
                None => return Err(libc::ENOENT),
                Some(Some(_)) => return Err(libc::ENOTDIR),
                Some(None) => {}
            }
        }
        Ok(())
    }

    fn has_children(&self, path: &str) -> bool {
        self.tree.keys().any(|other| is_below(other, path))
    }

    pub(crate) fn files(&self) -> Vec<&String> {
        self.tree
            .iter()
            .filter(|(_, entry)| entry.is_some())
            .map(|(path, _)| path)
            .collect()
    }

    /// Folders, the client's own root (`""`) first.
    fn dirs(&self) -> Vec<String> {
        std::iter::once(String::new())
            .chain(
                self.tree
                    .iter()
                    .filter(|(_, entry)| entry.is_none())
                    .map(|(path, _)| path.clone()),
            )
            .collect()
    }

    /// A random op, mostly one that should succeed.
    pub(crate) fn next_op(&self, rng: &mut Rng) -> FsOp {
        let dirs = self.dirs();
        let files = self.files();
        let in_dir = |rng: &mut Rng, names: &[&str]| {
            let parent = rng.pick(&dirs).expect("the root is always there");
            join(parent, rng.pick(names).expect("names"))
        };
        let a_file = |rng: &mut Rng| rng.pick(&files).map(|path| (*path).clone());
        let roll = rng.below(100);
        let op = match roll {
            0..30 => Some(FsOp::Create {
                path: in_dir(rng, &FILE_NAMES),
                data: data(rng),
            }),
            30..45 => a_file(rng).map(|path| {
                let len = self.content(&path).map_or(0, Vec::len) as u64;
                FsOp::Write {
                    offset: rng.below(len + 8),
                    path,
                    data: data(rng),
                }
            }),
            45..50 => a_file(rng).map(|path| FsOp::Truncate {
                len: rng.below(32),
                path,
            }),
            50..60 => Some(FsOp::Mkdir {
                path: in_dir(rng, &DIR_NAMES),
            }),
            // A rename in place, over another file as often as not.
            60..72 => a_file(rng).map(|from| FsOp::Rename {
                to: join(parent_of(&from), rng.pick(&FILE_NAMES).expect("names")),
                from,
            }),
            // A move to another folder, renamed or not.
            72..82 => a_file(rng).map(|from| {
                let name = if rng.chance(0.5) {
                    name_of(&from)
                } else {
                    rng.pick(&FILE_NAMES).expect("names")
                };
                let parent = rng.pick(&dirs).expect("the root is always there");
                FsOp::Rename {
                    to: join(parent, name),
                    from,
                }
            }),
            // A folder renamed or moved, with what is in it.
            82..86 => {
                let from = rng.pick(&dirs[1..]).cloned();
                from.map(|from| FsOp::Rename {
                    to: in_dir(rng, &DIR_NAMES),
                    from,
                })
            }
            86..94 => a_file(rng).map(|path| FsOp::Unlink { path }),
            94..97 => a_file(rng).map(|path| FsOp::UnlinkOpen {
                path,
                data: data(rng),
            }),
            _ => rng
                .pick(&dirs[1..])
                .cloned()
                .map(|path| FsOp::Rmdir { path }),
        };
        op.unwrap_or_else(|| FsOp::Create {
            path: in_dir(rng, &FILE_NAMES),
            data: data(rng),
        })
    }
}

/// Content for a write: short and recognisable, now and then empty.
pub(crate) fn data(rng: &mut Rng) -> Vec<u8> {
    let len = if rng.chance(0.1) {
        0
    } else {
        rng.between(1, 48)
    };
    let tag = rng.next_u64();
    (0..len).map(|i| b'a' + ((tag + i) % 26) as u8).collect()
}

pub(crate) fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

pub(crate) fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

pub(crate) fn name_of(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

fn is_below(path: &str, ancestor: &str) -> bool {
    path.len() > ancestor.len() + 1
        && path.starts_with(ancestor)
        && path.as_bytes()[ancestor.len()] == b'/'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(path: &str, data: &[u8]) -> FsOp {
        FsOp::Create {
            path: path.into(),
            data: data.to_vec(),
        }
    }

    #[test]
    fn a_rename_over_a_file_replaces_it_and_a_folder_moves_with_its_contents() {
        let mut model = Model::default();
        model.apply(&FsOp::Mkdir { path: "x".into() }).unwrap();
        model.apply(&create("x/a", b"1")).unwrap();
        model.apply(&create("b", b"2")).unwrap();
        model
            .apply(&FsOp::Rename {
                from: "b".into(),
                to: "x/a".into(),
            })
            .unwrap();
        assert_eq!(model.content("x/a"), Some(&b"2".to_vec()));
        model
            .apply(&FsOp::Rename {
                from: "x".into(),
                to: "y".into(),
            })
            .unwrap();
        assert_eq!(model.tree().keys().collect::<Vec<_>>(), vec!["y", "y/a"]);
    }

    #[test]
    fn calls_fail_the_way_posix_says() {
        let mut model = Model::default();
        model.apply(&FsOp::Mkdir { path: "x".into() }).unwrap();
        model.apply(&create("x/a", b"1")).unwrap();
        assert_eq!(
            model.apply(&FsOp::Mkdir { path: "x".into() }),
            Err(libc::EEXIST)
        );
        assert_eq!(model.apply(&create("x", b"")), Err(libc::EISDIR));
        assert_eq!(model.apply(&create("x/a/b", b"")), Err(libc::ENOTDIR));
        assert_eq!(model.apply(&create("q/a", b"")), Err(libc::ENOENT));
        assert_eq!(
            model.apply(&FsOp::Rmdir { path: "x".into() }),
            Err(libc::ENOTEMPTY)
        );
        assert_eq!(
            model.apply(&FsOp::Unlink { path: "x".into() }),
            Err(libc::EISDIR)
        );
        let into_itself = FsOp::Rename {
            from: "x".into(),
            to: "x/y".into(),
        };
        assert_eq!(model.apply(&into_itself), Err(libc::EINVAL));
    }

    #[test]
    fn a_write_past_the_end_leaves_a_hole() {
        let mut model = Model::default();
        model.apply(&create("a", b"ab")).unwrap();
        let write = FsOp::Write {
            path: "a".into(),
            offset: 4,
            data: b"z".to_vec(),
        };
        model.apply(&write).unwrap();
        assert_eq!(model.content("a"), Some(&b"ab\0\0z".to_vec()));
        let nothing = FsOp::Write {
            path: "a".into(),
            offset: 9,
            data: Vec::new(),
        };
        model.apply(&nothing).unwrap();
        assert_eq!(model.content("a"), Some(&b"ab\0\0z".to_vec()));
    }

    #[test]
    fn generated_ops_mostly_succeed() {
        let mut rng = Rng::new(9);
        let mut model = Model::default();
        let ok = (0..500)
            .filter(|_| {
                let op = model.next_op(&mut rng);
                model.apply(&op).is_ok()
            })
            .count();
        assert!(ok > 350, "{ok} of 500 succeeded");
    }
}
