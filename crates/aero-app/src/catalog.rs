//! One versioned, FTS-independent directory per vault. Only background readers
//! take the build lock; UI invalidation holds the state lock for a short update.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use aero_core::links::{NoteSummary, Resolver};
use aero_core::{Vault, VaultPath};

pub(crate) struct Snapshot {
    pub version: u64,
    stamps: BTreeMap<VaultPath, (std::time::SystemTime, u64)>,
    pub notes: Arc<Vec<NoteSummary>>,
    pub prepared: Arc<crate::matching::Notes>,
    pub resolver: Resolver,
    pub folders: Vec<VaultPath>,
    pub attachments: aero_core::attachments::Catalog,
    pub relations: Vec<aero_core::relations::Note>,
}

struct State {
    version: u64,
    full: bool,
    dirty: BTreeSet<VaultPath>,
    snapshot: Option<Arc<Snapshot>>,
}

pub(crate) struct Catalog {
    vault: Arc<Vault>,
    state: Mutex<State>,
    build: Mutex<()>,
}

impl Catalog {
    pub fn new(vault: Arc<Vault>) -> Arc<Self> {
        Arc::new(Self {
            vault,
            state: Mutex::new(State { version: 0, full: true, dirty: BTreeSet::new(), snapshot: None }),
            build: Mutex::new(()),
        })
    }

    pub fn vault_id(&self) -> &str {
        self.vault.id()
    }

    pub fn touch(&self, paths: impl IntoIterator<Item = VaultPath>) {
        let mut paths = paths.into_iter().peekable();
        if paths.peek().is_none() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        state.dirty.extend(paths);
        state.version += 1;
    }

    pub fn invalidate(&self) {
        let mut state = self.state.lock().unwrap();
        state.full = true;
        state.version += 1;
    }

    /// Call from a background executor. Coalesces concurrent tree/completion/
    /// resolver requests and retries if a file event arrives during the build.
    pub fn snapshot(&self) -> Result<Arc<Snapshot>, String> {
        let _build = self.build.lock().unwrap();
        loop {
            let (version, full, dirty, previous) = {
                let state = self.state.lock().unwrap();
                if !state.full && state.dirty.is_empty() {
                    if let Some(snapshot) = &state.snapshot {
                        return Ok(snapshot.clone());
                    }
                }
                (state.version, state.full, state.dirty.clone(), state.snapshot.clone())
            };
            let mut notes: BTreeMap<_, _> = previous.as_ref().map_or_else(BTreeMap::new, |s| {
                s.notes.iter().cloned().map(|note| (note.path.clone(), note)).collect()
            });
            // Only additions need visibility validation against ignore rules.
            // Existing paths are refreshed directly; rule changes request full calibration.
            let visible = if !full && dirty.iter().any(|path| path.is_note() && !notes.contains_key(path)) {
                Some(self.vault.notes().map_err(|e| e.to_string())?.into_iter().collect::<BTreeSet<_>>())
            } else {
                None
            };
            let paths: Vec<_> = if full {
                let paths = self.vault.notes().map_err(|e| e.to_string())?;
                let present: BTreeSet<_> = paths.iter().cloned().collect();
                notes.retain(|path, _| present.contains(path));
                paths
            } else {
                dirty.iter().filter(|p| p.is_note()).cloned().collect()
            };
            let mut stamps = previous.as_ref().map_or_else(BTreeMap::new, |s| s.stamps.clone());
            let mut relations: BTreeMap<_, _> = previous.as_ref().map_or_else(BTreeMap::new, |s| {
                s.relations.iter().cloned().map(|note| (note.path.clone(), note)).collect()
            });
            let mut membership_changed = false;
            let mut resolver = previous.as_ref().map_or_else(Resolver::new, |s| s.resolver.clone());
            if full {
                for path in stamps.keys().filter(|path| !notes.contains_key(*path)) {
                    resolver.remove(path);
                }
            }
            for path in paths {
                if visible.as_ref().is_some_and(|visible| !visible.contains(&path))
                    || !self.vault.exists(&path)
                {
                    membership_changed |= notes.remove(&path).is_some();
                    stamps.remove(&path);
                    relations.remove(&path);
                    resolver.remove(&path);
                    continue;
                }
                let stamp = self.vault.stat(&path).ok();
                if !dirty.contains(&path)
                    && stamp.is_some()
                    && stamps.get(&path) == stamp.as_ref()
                    && notes.contains_key(&path)
                {
                    continue;
                }
                if let Some(stamp) = stamp {
                    stamps.insert(path.clone(), stamp);
                }
                let note = match self.vault.read(&path) {
                    Ok((doc, _)) => {
                        let note = NoteSummary::from_document(path.clone(), &doc);
                        resolver.insert(path.clone(), Some(&note.title), &note.aliases);
                        resolver.register_identity(&path, &doc);
                        relations.insert(
                            path.clone(),
                            aero_core::relations::Note {
                                path: path.clone(),
                                title: note.title.clone(),
                                metadata: aero_core::identity::read(&doc),
                            },
                        );
                        note
                    }
                    Err(_) => {
                        resolver.remove(&path);
                        relations.insert(
                            path.clone(),
                            aero_core::relations::Note {
                                path: path.clone(),
                                title: path.stem().into(),
                                metadata: Err("无法读取文件".into()),
                            },
                        );
                        NoteSummary { title: path.stem().into(), path: path.clone(), aliases: Vec::new() }
                    }
                };
                membership_changed |= notes.insert(path, note).is_none();
            }
            let directories_changed = full || membership_changed || dirty.iter().any(|p| !p.is_note());
            let folders = if directories_changed {
                self.vault.folders().map_err(|e| e.to_string())?
            } else {
                previous.as_ref().map_or_else(Vec::new, |s| s.folders.clone())
            };
            let attachments = if full || dirty.iter().any(|p| !p.is_note()) {
                aero_core::attachments::Catalog::read(&self.vault).map_err(|e| e.to_string())?
            } else {
                previous.as_ref().map_or_else(Default::default, |s| s.attachments.clone())
            };
            stamps.retain(|path, _| notes.contains_key(path));
            relations.retain(|path, _| notes.contains_key(path));
            let notes: Vec<_> = notes.into_values().collect();
            let prepared = Arc::new(previous.as_ref().map_or_else(
                || crate::matching::Notes::new(&notes),
                |previous| crate::matching::Notes::reuse(&notes, &previous.notes, &previous.prepared),
            ));
            for note in &notes {
                if !resolver.contains(&note.path) {
                    resolver.insert(note.path.clone(), Some(&note.title), &note.aliases);
                }
            }
            let snapshot = Arc::new(Snapshot {
                version,
                stamps,
                notes: Arc::new(notes),
                prepared,
                resolver,
                folders,
                attachments,
                relations: relations.into_values().collect(),
            });
            let mut state = self.state.lock().unwrap();
            if state.version != version {
                continue;
            }
            state.snapshot = Some(snapshot.clone());
            state.full = false;
            state.dirty.clear();
            return Ok(snapshot);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str) -> VaultPath {
        VaultPath::new(name).unwrap()
    }

    #[test]
    fn shared_snapshot_updates_metadata_deletion_recreation_and_assets() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "---\ntitle: 第一篇\naliases: shared\n---\nbody\n").unwrap();
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        let catalog = Catalog::new(Arc::new(Vault::open(dir.path()).unwrap()));
        let initial = catalog.snapshot().unwrap();
        assert!(
            Arc::ptr_eq(&initial, &catalog.snapshot().unwrap()),
            "unchanged consumers share one snapshot"
        );
        assert_eq!(initial.notes[0].title, "第一篇");
        assert!(initial.folders.contains(&path("empty")));
        std::fs::write(dir.path().join("a.md"), "---\ntitle: 第二篇\naliases: renamed\n---\nbody\n").unwrap();
        std::fs::write(dir.path().join("b.md"), "---\naliases: renamed\n---\nbody\n").unwrap();
        catalog.touch([path("a.md"), path("b.md")]);
        let changed = catalog.snapshot().unwrap();
        assert!(changed.version > initial.version);
        assert_eq!(initial.notes[0].title, "第一篇", "published snapshots are immutable");
        assert_eq!(changed.notes[0].title, "第二篇");
        assert!(matches!(
            changed.resolver.resolve("renamed", None, ""),
            aero_core::links::Resolution::Ambiguous { .. }
        ));
        std::fs::remove_file(dir.path().join("a.md")).unwrap();
        catalog.touch([path("a.md")]);
        assert_eq!(catalog.snapshot().unwrap().notes.len(), 1);
        std::fs::write(dir.path().join("a.md"), "new contents").unwrap();
        std::fs::create_dir(dir.path().join("a.assets")).unwrap();
        std::fs::write(dir.path().join("a.assets/image.png"), b"image").unwrap();
        catalog.touch([path("a.md"), path("a.assets/image.png")]);
        let recreated = catalog.snapshot().unwrap();
        assert_eq!(recreated.notes.len(), 2);
        assert_eq!(recreated.notes[0].title, "a");
        assert_eq!(recreated.attachments.entries.len(), 1);
        std::fs::create_dir_all(dir.path().join(".aero")).unwrap();
        std::fs::write(dir.path().join(".aero/ignore"), "b.md\n").unwrap();
        catalog.invalidate();
        assert_eq!(catalog.snapshot().unwrap().notes.len(), 1);
        std::fs::write(dir.path().join("a.assets/hidden.md"), "attachment markdown").unwrap();
        catalog.touch([path("b.md"), path("a.assets/hidden.md")]);
        assert_eq!(
            catalog.snapshot().unwrap().notes.len(),
            1,
            "ignored and attachment Markdown cannot re-enter through incremental events"
        );
    }

    #[test]
    fn concurrent_consumers_publish_one_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "body").unwrap();
        let catalog = Catalog::new(Arc::new(Vault::open(dir.path()).unwrap()));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let catalog = catalog.clone();
                std::thread::spawn(move || catalog.snapshot().unwrap())
            })
            .collect();
        let snapshots: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert!(snapshots.iter().all(|s| Arc::ptr_eq(s, &snapshots[0])));
    }
}
