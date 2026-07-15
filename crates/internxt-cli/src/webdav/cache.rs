//! The folder-listing cache now lives in `crate::serve::cache` (shared with the
//! FUSE backend). Re-exported here so existing `super::cache::…` paths in this
//! module keep resolving.

pub use crate::serve::cache::FolderCache;
