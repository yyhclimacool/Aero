//! Shared disk snapshots for sync references across tabs and windows. All reads
//! run off the UI thread; the cache owns at most 128 snapshots / 32 MiB of source.
use aero_core::{Document, Vault, VaultPath};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, OnceLock},
    time::SystemTime,
};

const MAX_ENTRIES: usize = 128;
const MAX_BYTES: usize = 32 * 1024 * 1024;
#[derive(Default)]
struct Cache {
    entries: VecDeque<Entry>,
    bytes: usize,
}
type SharedRead = Arc<OnceLock<Result<Arc<Document>, String>>>;
struct Entry {
    vault: String,
    path: VaultPath,
    modified: SystemTime,
    size: u64,
    read: SharedRead,
}
fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}
impl Cache {
    fn cell(&mut self, vault: &str, path: &VaultPath, modified: SystemTime, size: u64) -> SharedRead {
        if let Some(index) = self.entries.iter().position(|entry| entry.vault == vault && &entry.path == path)
        {
            let entry = self.entries.remove(index).unwrap();
            if entry.modified == modified && entry.size == size {
                let read = entry.read.clone();
                self.entries.push_back(entry);
                return read;
            }
            self.bytes -= entry.size as usize;
        }
        let read: SharedRead = Arc::new(OnceLock::new());
        if size <= MAX_BYTES as u64 {
            while self.entries.len() >= MAX_ENTRIES || self.bytes + size as usize > MAX_BYTES {
                let Some(entry) = self.entries.pop_front() else { break };
                self.bytes -= entry.size as usize;
            }
            self.bytes += size as usize;
            self.entries.push_back(Entry {
                vault: vault.into(),
                path: path.clone(),
                modified,
                size,
                read: read.clone(),
            });
        }
        read
    }
}

pub(super) fn read(vault: &Vault, path: &VaultPath) -> Result<Arc<Document>, String> {
    vault.validate_local_path(path).map_err(|e| e.to_string())?;
    let metadata = std::fs::metadata(vault.fs_path(path)).map_err(|e| e.to_string())?;
    let read = cache().lock().unwrap_or_else(|e| e.into_inner()).cell(
        vault.id(),
        path,
        metadata.modified().map_err(|e| e.to_string())?,
        metadata.len(),
    );
    // Only requests for this exact revision share the once cell. Cache
    // invalidation on the UI thread never waits for disk I/O or parsing.
    read.get_or_init(|| vault.read(path).map(|(doc, _)| Arc::new(doc)).map_err(|e| e.to_string())).clone()
}

pub(super) fn invalidate(vault: &str, paths: &[VaultPath]) {
    let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
    cache.entries.retain(|entry| entry.vault != vault || !paths.contains(&entry.path));
    cache.bytes = cache.entries.iter().map(|e| e.size as usize).sum();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_cache_enforces_entry_and_byte_limits_and_preserves_active_readers() {
        let mut cache = Cache::default();
        let now = SystemTime::UNIX_EPOCH;
        let path = VaultPath::new("first.md").unwrap();
        let first = cache.cell("vault", &path, now, 10);
        assert!(first.set(Ok(Arc::new(Document::from_text("active")))).is_ok());
        for i in 0..MAX_ENTRIES {
            cache.cell("vault", &VaultPath::new(&format!("entry-{i}.md")).unwrap(), now, 10);
        }
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
        assert!(!Arc::ptr_eq(&first, &cache.cell("vault", &path, now, 10)));
        assert_eq!(first.get().unwrap().as_ref().unwrap().to_string(), "active");
        let large = VaultPath::new("large.md").unwrap();
        let full = cache.cell("vault", &large, now, MAX_BYTES as u64);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.bytes, MAX_BYTES);
        assert!(Arc::ptr_eq(&full, &cache.cell("vault", &large, now, MAX_BYTES as u64)));
        let refreshed = cache.cell("vault", &large, now, 20);
        assert!(!Arc::ptr_eq(&full, &refreshed));
        assert_eq!(cache.bytes, 20);
        let uncached = VaultPath::new("oversized.md").unwrap();
        let first_large = cache.cell("vault", &uncached, now, MAX_BYTES as u64 + 1);
        let second_large = cache.cell("vault", &uncached, now, MAX_BYTES as u64 + 1);
        assert!(!Arc::ptr_eq(&first_large, &second_large));
        assert_eq!(cache.bytes, 20);
        assert_eq!(cache.entries.len(), 1);
    }
    #[test]
    fn identical_revisions_share_one_parse_and_invalidation_replaces_it() {
        let root = tempfile::tempdir().unwrap();
        let vault = Vault::open(root.path()).unwrap();
        let path = VaultPath::new("source.md").unwrap();
        vault.create_bytes(&path, b"first").unwrap();
        let first = read(&vault, &path).unwrap();
        let second = read(&vault, &path).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        vault.write_bytes(&path, b"new revision").unwrap();
        invalidate(vault.id(), std::slice::from_ref(&path));
        let next = read(&vault, &path).unwrap();
        assert!(!Arc::ptr_eq(&first, &next));
        assert_eq!(next.to_string(), "new revision");
    }
}
