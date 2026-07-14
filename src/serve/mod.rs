//! Code shared by the serve backends that expose a Drive over a filesystem-like
//! protocol: WebDAV (`serve webdav`) and FUSE (`mount`), with sftp/smb possible
//! later. Holds the protocol-agnostic Drive-tree walk (`tree`), the folder
//! listing cache (`cache`), and the refreshable credentials holder (`creds`).

pub mod cache;
pub mod creds;
pub mod tree;
