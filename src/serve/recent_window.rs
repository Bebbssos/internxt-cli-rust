//! A trailing window of the most recently streamed bytes for one open file,
//! shared by every serve backend's sequential read handle (FUSE, SMB, NFS,
//! SFTP).
//!
//! Each of those backends serves reads from one lazily-(re)started decrypt
//! stream per open file; a stream restart pays a fresh `getDownloadLinks`
//! round trip plus a new ranged GET. Backward seeks (even one byte) and
//! forward gaps past the backend's forward-skip threshold always restarted,
//! with nothing retained across restarts. Container-index parsing (MP4 `moov`
//! atom, MKV cues) does lots of short hops re-reading a box header then its
//! payload, often backing up slightly — each one was a full restart. Keeping
//! this window means a read fully covered by recently-seen bytes (whether
//! served to the caller or discarded while skipping a forward gap) is
//! answered from memory instead, independent of and surviving any one
//! underlying stream's restart.

use std::collections::VecDeque;

/// How much recently-streamed data is kept around per open file. Covers small
/// backward/forward re-reads (container-index parsing) without a network
/// round trip; large enough for that, small enough that many open files stay
/// cheap.
pub const BACKWARD_WINDOW: u64 = 4 * 1024 * 1024;

pub struct RecentWindow {
    /// Bytes `[start, start + data.len())` of the file, most recent last.
    data: VecDeque<u8>,
    start: u64,
}

impl RecentWindow {
    pub fn new() -> Self {
        RecentWindow { data: VecDeque::new(), start: 0 }
    }

    /// Record `chunk` as having been read from `chunk_start`. A discontinuity
    /// (the stream restarted elsewhere) resets the window to just this chunk
    /// rather than keeping a stale, disjoint range around.
    pub fn push(&mut self, chunk: &[u8], chunk_start: u64) {
        if chunk.is_empty() {
            return;
        }
        if self.data.is_empty() || chunk_start != self.start + self.data.len() as u64 {
            self.data.clear();
            self.start = chunk_start;
        }
        self.data.extend(chunk.iter().copied());
        while self.data.len() as u64 > BACKWARD_WINDOW {
            self.data.pop_front();
            self.start += 1;
        }
    }

    /// `size` bytes at `offset`, only if fully covered by the window.
    pub fn read_full(&self, offset: u64, size: usize) -> Option<Vec<u8>> {
        if size == 0 {
            return Some(Vec::new());
        }
        if offset < self.start {
            return None;
        }
        let rel = usize::try_from(offset - self.start).ok()?;
        if rel.checked_add(size)? > self.data.len() {
            return None;
        }
        Some(self.data.iter().skip(rel).take(size).copied().collect())
    }
}

impl Default for RecentWindow {
    fn default() -> Self {
        Self::new()
    }
}
