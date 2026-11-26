//! Persistent cache of full-content hashes for the duplicate finder.
//!
//! Key: lowercase path. An entry is valid only while the file keeps the same
//! size and last-write time, so any change makes it miss and get re-hashed.
//! Stored in `%LOCALAPPDATA%\WizEverything\hash_cache.bin`; entries unused for
//! 90 days are dropped when saving.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const MAGIC: &[u8; 6] = b"WZHC01";
const KEEP_DAYS: u32 = 90;

#[derive(Clone, Copy)]
struct Entry {
    size: u64,
    mtime: u64,
    hash: u128,
    /// Day (since 1970) the entry was last used.
    seen: u32,
}

pub struct HashCache {
    map: HashMap<String, Entry>,
    path: PathBuf,
    loaded: bool,
    dirty: bool,
}

static CACHE: OnceLock<Mutex<HashCache>> = OnceLock::new();
static PATH_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

fn today() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| (d.as_secs() / 86_400) as u32)
}

pub fn default_path() -> PathBuf {
    if let Some(p) = PATH_OVERRIDE.get() {
        return p.clone();
    }
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("WizEverything").join("hash_cache.bin")
}

/// Use another file (tests). Must be called before the cache is first used.
#[cfg_attr(not(test), allow(dead_code))]
pub fn set_path(p: PathBuf) {
    let _ = PATH_OVERRIDE.set(p);
}

pub fn global() -> &'static Mutex<HashCache> {
    CACHE.get_or_init(|| Mutex::new(HashCache { map: HashMap::new(), path: default_path(), loaded: false, dirty: false }))
}

/// (size, last-write time) used to validate entries; no file is opened.
pub fn file_key(path: &str) -> Option<(u64, u64)> {
    let m = std::fs::metadata(path).ok()?;
    let mtime = m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos() as u64;
    Some((m.len(), mtime))
}

impl HashCache {
    fn ensure_loaded(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let Ok(mut f) = std::fs::File::open(&self.path) else { return };
        let mut data = Vec::new();
        if f.read_to_end(&mut data).is_err() || !data.starts_with(MAGIC) {
            return;
        }
        let mut at = MAGIC.len();
        let mut take = |n: usize| -> Option<&[u8]> {
            let s = data.get(at..at + n)?;
            at += n;
            Some(s)
        };
        let Some(count) = take(8).map(|b| u64::from_le_bytes(b.try_into().unwrap())) else { return };
        for _ in 0..count {
            let Some(len) = take(2).map(|b| u16::from_le_bytes(b.try_into().unwrap()) as usize) else { break };
            let Some(path) = take(len).and_then(|b| String::from_utf8(b.to_vec()).ok()) else { break };
            let Some(rest) = take(8 + 8 + 16 + 4) else { break };
            let e = Entry {
                size: u64::from_le_bytes(rest[0..8].try_into().unwrap()),
                mtime: u64::from_le_bytes(rest[8..16].try_into().unwrap()),
                hash: u128::from_le_bytes(rest[16..32].try_into().unwrap()),
                seen: u32::from_le_bytes(rest[32..36].try_into().unwrap()),
            };
            self.map.insert(path, e);
        }
    }

    /// Cached hash if the file still has this size and modification time.
    pub fn get(&mut self, path: &str, key: (u64, u64)) -> Option<u128> {
        self.ensure_loaded();
        let e = self.map.get_mut(&path.to_lowercase())?;
        if (e.size, e.mtime) != key {
            return None;
        }
        let d = today();
        if e.seen != d {
            e.seen = d;
            self.dirty = true;
        }
        Some(e.hash)
    }

    pub fn put(&mut self, path: &str, key: (u64, u64), hash: u128) {
        self.ensure_loaded();
        self.map.insert(path.to_lowercase(), Entry { size: key.0, mtime: key.1, hash, seen: today() });
        self.dirty = true;
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.loaded = true;
        self.dirty = false;
        let _ = std::fs::remove_file(&self.path);
    }

    /// Write to disk (atomically) if anything changed; drops stale entries.
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let cutoff = today().saturating_sub(KEEP_DAYS);
        self.map.retain(|_, e| e.seen >= cutoff);
        let mut out = Vec::with_capacity(16 + self.map.len() * 96);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(self.map.len() as u64).to_le_bytes());
        for (path, e) in &self.map {
            let b = path.as_bytes();
            let len = b.len().min(u16::MAX as usize);
            out.extend_from_slice(&(len as u16).to_le_bytes());
            out.extend_from_slice(&b[..len]);
            out.extend_from_slice(&e.size.to_le_bytes());
            out.extend_from_slice(&e.mtime.to_le_bytes());
            out.extend_from_slice(&e.hash.to_le_bytes());
            out.extend_from_slice(&e.seen.to_le_bytes());
        }
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.path.with_extension("tmp");
        let ok = std::fs::File::create(&tmp).and_then(|mut f| f.write_all(&out)).is_ok();
        if ok && std::fs::rename(&tmp, &self.path).is_ok() {
            self.dirty = false;
        }
    }
}
