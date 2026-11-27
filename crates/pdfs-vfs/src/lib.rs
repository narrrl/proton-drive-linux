//! The mounted-files service: what a file open through a mount is, and what
//! happens to its bytes between `open` and `close`.
//!
//! Step 5 of the 4.0 plan in `docs/ARCHITECTURE.md` moves the mount's
//! state here, slice by slice, so that it can be built and tested without a
//! kernel. Nothing in this crate knows about `fuser` or errno: the FUSE
//! adapter in `pdfs-fuse` maps the typed errors at the boundary.
//!
//! The first slice is the open file ([`open`]): the scratch file a handle
//! writes into, which bytes of it are authored, and the released write that
//! waits for its upload.

pub mod open;
