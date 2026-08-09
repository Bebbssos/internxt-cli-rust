//! Code shared by the serve backends that expose a Drive over a filesystem-like
//! protocol: WebDAV (`serve webdav`) and FUSE (`mount`), with sftp/smb possible
//! later. Holds the protocol-agnostic Drive-tree walk (`tree`), the folder
//! listing cache (`cache`), and the refreshable credentials holder (`creds`).

pub mod cache;
// Moved to a top-level module so `vpn` (no serve backend enabled) can reuse
// it too; re-exported here so every existing `serve::creds::…` reference
// keeps working unchanged.
pub(crate) use crate::session_creds as creds;
pub mod log;
pub mod recent_window;
#[cfg(any(
    feature = "webdav",
    all(unix, feature = "fuse"),
    feature = "smb",
    feature = "nfs",
    feature = "sftp"
))]
pub mod run;
#[cfg(any(
    feature = "webdav",
    all(unix, feature = "fuse"),
    feature = "smb",
    feature = "nfs",
    feature = "sftp"
))]
pub mod thumbnail;
pub mod tree;
