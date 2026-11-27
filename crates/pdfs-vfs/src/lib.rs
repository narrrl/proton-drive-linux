//! The mounted-files service: what a file open through a mount is, and what
//! happens to its bytes between `open` and `close`.
//!
//! Step 5 of the 4.0 plan in `docs/ARCHITECTURE.md` moves the mount's
//! state here, slice by slice, so that it can be built and tested without a
//! kernel. Nothing in this crate knows about `fuser` or errno: the FUSE
//! adapter in `pdfs-fuse` maps the typed errors at the boundary.
//!
//! So far it holds the tree ([`tree`]): the nodes a mount shows, their
//! stable inode numbers, and the per-mount roots; and the open files
//! ([`open`]): the scratch file a handle writes into, which bytes of it are
//! authored, the released write that waits for its upload, and directory
//! handles; and what the mount accepted but has not sent to Drive yet
//! ([`writes`]).

pub mod ids;
pub mod open;
pub mod tree;
pub mod writes;
