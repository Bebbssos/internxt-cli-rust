//! A small TTL cache for folder listings.
//!
//! Path resolution walks the folder tree from the root, listing subfolders at
//! every level (see `resource`). Under a burst of uploads to the same folder
//! that walk repeats identically for each request and dominates latency — the
//! delay before the server even begins reading a PUT body, which slow clients
//! (WinSCP) time out on and abort. Caching the per-folder subfolder listing for
//! a short TTL collapses those repeated walks.
//!
//! Only *folder* listings are cached — file listings stay live so a freshly
//! uploaded file is always visible (no duplicate-on-replace races). Folder
//! mutations (create / move / delete) invalidate affected entries.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::resource::FolderItem;

struct Entry {
    folders: Vec<FolderItem>,
    expires: Instant,
}

/// TTL cache of `folder_uuid -> [subfolders]`. A TTL of 0 disables it entirely
/// (every `get` misses and nothing is stored).
pub struct FolderCache {
    ttl: Duration,
    map: Mutex<HashMap<String, Entry>>,
}

impl FolderCache {
    pub fn new(ttl_secs: u64) -> Self {
        FolderCache {
            ttl: Duration::from_secs(ttl_secs),
            map: Mutex::new(HashMap::new()),
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
        let mut map = self.map.lock().unwrap();
        match map.get(folder_uuid) {
            Some(e) if e.expires > Instant::now() => Some(e.folders.clone()),
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
        let entry = Entry {
            folders,
            expires: Instant::now() + self.ttl,
        };
        self.map.lock().unwrap().insert(folder_uuid.to_string(), entry);
    }

    /// Drop one folder's cached listing (e.g. after creating a child in it).
    pub fn invalidate(&self, folder_uuid: &str) {
        if !self.enabled() {
            return;
        }
        self.map.lock().unwrap().remove(folder_uuid);
    }

    /// Drop everything (used after a mutation whose affected parent isn't known).
    pub fn clear(&self) {
        if !self.enabled() {
            return;
        }
        self.map.lock().unwrap().clear();
    }
}
