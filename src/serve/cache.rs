//! A small TTL cache for folder and file listings, shared by the serve
//! backends.
//!
//! Path resolution walks the folder tree from the root, listing subfolders at
//! every level (see `tree`). Under a burst of requests to the same folder that
//! walk repeats identically for each request and dominates latency. Caching the
//! per-folder subfolder listing for a short TTL collapses those repeated walks.
//!
//! File listings are cached the same way, behind separate opt-in accessors
//! (`get_files`/`put_files`) — callers that need read-your-own-writes
//! visibility (a freshly uploaded file must show up immediately) keep using the
//! always-live `tree::list_files`/`find_folder`-style fetch instead. A miss
//! against a *cached* listing (see `tree::find_folder`/`find_file`) forces one
//! live re-fetch before concluding an entry really isn't there, so another
//! device's concurrent create doesn't look like a false negative for the rest
//! of the TTL. Any mutation (create / move / delete / rename, folder or file)
//! invalidates the affected parent's entry in both maps via `invalidate`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::tree::{FileItem, FolderItem};

struct Entry<T> {
    items: T,
    expires: Instant,
}

/// TTL cache of `folder_uuid -> [subfolders]` and `folder_uuid -> [subfiles]`.
/// A TTL of 0 disables it entirely (every `get` misses and nothing is stored).
pub struct FolderCache {
    ttl: Duration,
    folders: Mutex<HashMap<String, Entry<Vec<FolderItem>>>>,
    files: Mutex<HashMap<String, Entry<Vec<FileItem>>>>,
}

impl FolderCache {
    pub fn new(ttl_secs: u64) -> Self {
        FolderCache {
            ttl: Duration::from_secs(ttl_secs),
            folders: Mutex::new(HashMap::new()),
            files: Mutex::new(HashMap::new()),
        }
    }

    /// Whether caching is on (TTL > 0).
    pub fn enabled(&self) -> bool {
        !self.ttl.is_zero()
    }

    /// Cached subfolders for `folder_uuid`, if present and unexpired.
    pub fn get(&self, folder_uuid: &str) -> Option<Vec<FolderItem>> {
        if !self.enabled() {
            return None;
        }
        let mut map = self.folders.lock().unwrap();
        match map.get(folder_uuid) {
            Some(e) if e.expires > Instant::now() => Some(e.items.clone()),
            Some(_) => {
                map.remove(folder_uuid);
                None
            }
            None => None,
        }
    }

    /// Store the subfolder listing for `folder_uuid` with a fresh TTL.
    pub fn put(&self, folder_uuid: &str, folders: Vec<FolderItem>) {
        if !self.enabled() {
            return;
        }
        let entry = Entry { items: folders, expires: Instant::now() + self.ttl };
        self.folders.lock().unwrap().insert(folder_uuid.to_string(), entry);
    }

    /// Cached subfiles for `folder_uuid`, if present and unexpired.
    pub fn get_files(&self, folder_uuid: &str) -> Option<Vec<FileItem>> {
        if !self.enabled() {
            return None;
        }
        let mut map = self.files.lock().unwrap();
        match map.get(folder_uuid) {
            Some(e) if e.expires > Instant::now() => Some(e.items.clone()),
            Some(_) => {
                map.remove(folder_uuid);
                None
            }
            None => None,
        }
    }

    /// Store the subfile listing for `folder_uuid` with a fresh TTL.
    pub fn put_files(&self, folder_uuid: &str, files: Vec<FileItem>) {
        if !self.enabled() {
            return;
        }
        let entry = Entry { items: files, expires: Instant::now() + self.ttl };
        self.files.lock().unwrap().insert(folder_uuid.to_string(), entry);
    }

    /// Drop one folder's cached listings, both kinds (e.g. after creating a
    /// child in it, of either kind).
    pub fn invalidate(&self, folder_uuid: &str) {
        if !self.enabled() {
            return;
        }
        self.folders.lock().unwrap().remove(folder_uuid);
        self.files.lock().unwrap().remove(folder_uuid);
    }

    /// Drop everything (used after a mutation whose affected parent isn't known).
    pub fn clear(&self) {
        if !self.enabled() {
            return;
        }
        self.folders.lock().unwrap().clear();
        self.files.lock().unwrap().clear();
    }
}
