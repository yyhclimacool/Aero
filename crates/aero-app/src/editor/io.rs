use super::*;

#[derive(Default)]
pub(super) struct IoState {
    pub epoch: u64,
    pub checking: Option<Task<()>>,
    pub check_generation: u64,
    pub writing: bool,
    pub pending: Option<WriteJob>,
    pub suspended: bool,
    pub remove_after_save: Option<VaultPath>,
}

#[derive(Clone)]
pub(super) struct SaveOutcome {
    pub result: Result<Option<FileStamp>, String>,
    pub conflict: Option<VaultPath>,
}

// Accepted writes start immediately, independently of GPUI polling a future. Native
// termination can therefore wait for them even after the executor stops polling.
#[derive(Clone)]
pub(super) struct WriteJob {
    path: VaultPath,
    epoch: u64,
    version: u64,
    result: std::sync::Arc<(std::sync::Mutex<Option<SaveOutcome>>, std::sync::Condvar)>,
}

impl WriteJob {
    pub(super) fn spawn(
        path: VaultPath,
        epoch: u64,
        version: u64,
        work: impl FnOnce() -> SaveOutcome + Send + 'static,
    ) -> Self {
        let result = std::sync::Arc::new((std::sync::Mutex::new(None), std::sync::Condvar::new()));
        let output = result.clone();
        let operation = crate::diagnostics::Operation::new(
            "save_write",
            crate::diagnostics::Facts { epoch, version, ..Default::default() },
        );
        let queued = std::time::Instant::now();
        let failed = operation.clone();
        let started = std::thread::Builder::new().name("aero-save".into()).spawn(move || {
            operation.stage(
                "running",
                crate::diagnostics::Facts {
                    queue_us: queued.elapsed().as_micros() as u64,
                    ..Default::default()
                },
            );
            let outcome = operation
                .scope(|| std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)))
                .unwrap_or_else(|_| SaveOutcome {
                    result: Err("保存线程意外终止".into()), conflict: None
                });
            if outcome.conflict.is_some() {
                operation.stage("conflict", Default::default());
            }
            operation.finish(if outcome.result.is_ok() {
                crate::diagnostics::Outcome::Ok
            } else {
                crate::diagnostics::Outcome::Failed
            });
            *output.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(outcome);
            output.1.notify_all();
        });
        if let Err(error) = started {
            failed.finish(crate::diagnostics::Outcome::Failed);
            *result.0.lock().unwrap() = Some(SaveOutcome { result: Err(error.to_string()), conflict: None });
        }
        Self { path, epoch, version, result }
    }

    pub(super) fn wait(&self) -> SaveOutcome {
        let mut result = self.result.0.lock().unwrap_or_else(|p| p.into_inner());
        while result.is_none() {
            result = self.result.1.wait(result).unwrap_or_else(|p| p.into_inner());
        }
        result.as_ref().unwrap().clone()
    }
}

#[derive(Clone)]
pub(super) struct SaveRequest {
    pub vault: std::sync::Arc<Vault>,
    pub path: VaultPath,
    pub known: FileStamp,
    pub conflict_base: VaultPath,
    pub snapshot: aero_core::Snapshot,
    pub bom: bool,
    pub ending: aero_core::LineEnding,
    pub epoch: u64,
    pub allow_missing: bool,
}

impl SaveRequest {
    pub(super) fn save(self) -> SaveOutcome {
        save(
            &self.vault,
            &self.path,
            self.known,
            self.conflict_base,
            self.snapshot,
            self.bom,
            self.ending,
            self.allow_missing,
        )
    }

    pub(super) fn finish(
        mut self,
        previous: Option<WriteJob>,
        dirty: bool,
        remove: Option<VaultPath>,
    ) -> SaveGuard {
        SaveGuard(WriteJob::spawn(self.path.clone(), self.epoch, self.snapshot.version(), move || {
            let mut conflict = None;
            if let Some(previous) = previous {
                let outcome = previous.wait();
                if previous.path == self.path && previous.epoch == self.epoch {
                    match outcome.result {
                        Ok(Some(stamp)) => self.known = stamp,
                        Ok(None) => {}
                        // Do not retry a failed write/backup during irrevocable native shutdown.
                        Err(_) => return outcome,
                    }
                    if previous.version == self.snapshot.version() && matches!(outcome.result, Ok(Some(_))) {
                        return outcome;
                    }
                    conflict = outcome.conflict;
                }
            }
            if !dirty {
                return SaveOutcome { result: Ok(None), conflict };
            }
            let vault = self.vault.clone();
            let mut outcome = self.save();
            if outcome.conflict.is_none() {
                outcome.conflict = conflict;
            }
            if matches!(outcome.result, Ok(Some(_))) {
                if let Some(path) = remove {
                    if let Err(error) = std::fs::remove_file(vault.fs_path(&path)) {
                        log::warn!("remove conflict copy: {error}");
                    }
                }
            }
            outcome
        }))
    }
}

pub(crate) struct SaveGuard(WriteJob);

impl SaveGuard {
    pub(super) fn wait(&self) -> SaveOutcome {
        self.0.wait()
    }
    pub(super) fn job(&self) -> WriteJob {
        self.0.clone()
    }
}

impl Drop for SaveGuard {
    fn drop(&mut self) {
        let outcome = self.wait();
        if let Err(error) = outcome.result {
            log::error!("save during native termination: {error}");
        }
    }
}

pub(super) fn disk_state(
    vault: &Vault,
    path: &VaultPath,
    known: FileStamp,
) -> Result<(Disk, FileStamp), VaultError> {
    let (mtime, size) = match vault.stat(path) {
        Ok(stamp) => stamp,
        Err(VaultError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Disk::Missing, known));
        }
        Err(error) => return Err(error),
    };
    if mtime == known.mtime && size == known.size {
        return Ok((Disk::Unchanged, known));
    }
    let (bytes, stamp) = vault.read_bytes(path)?;
    if stamp.hash == known.hash { Ok((Disk::Unchanged, stamp)) } else { Ok((Disk::Changed(bytes), stamp)) }
}

pub(super) fn backup(vault: &Vault, base: VaultPath, bytes: &[u8]) -> Result<VaultPath, VaultError> {
    for attempt in 0..1000 {
        let path = if attempt == 0 {
            base.clone()
        } else {
            VaultPath::new(&format!("{}-{attempt}.md", base.as_str().trim_end_matches(".md")))?
        };
        match vault.create_bytes(&path, bytes) {
            Ok(_) => return Ok(path),
            Err(VaultError::Io { source, .. }) if source.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(VaultError::AlreadyExists(base.to_string()))
}

pub(super) fn save(
    vault: &Vault,
    path: &VaultPath,
    known: FileStamp,
    conflict_base: VaultPath,
    snapshot: aero_core::Snapshot,
    bom: bool,
    ending: aero_core::LineEnding,
    allow_missing: bool,
) -> SaveOutcome {
    let mut conflict = None;
    let result = (|| {
        crate::diagnostics::stage_current("disk_check", Default::default());
        match disk_state(vault, path, known).map_err(|e| format!("无法校验磁盘版本，已停止保存：{e}"))?.0
        {
            Disk::Missing if !allow_missing => return Ok(None),
            Disk::Changed(bytes) => {
                conflict = Some(
                    backup(vault, conflict_base, &bytes)
                        .map_err(|e| format!("无法保存冲突副本，已停止保存：{e}"))?,
                );
            }
            Disk::Unchanged | Disk::Missing => {}
        }
        let bytes = aero_core::SourceText::encode(&String::from(snapshot.text()), bom, ending);
        crate::diagnostics::stage_current(
            "write_begin",
            crate::diagnostics::Facts { bytes: bytes.len() as u64, ..Default::default() },
        );
        let result = vault.write_bytes(path, &bytes).map(Some).map_err(|e| e.to_string());
        crate::diagnostics::stage_current("write_done", Default::default());
        result
    })();
    SaveOutcome { result, conflict }
}

#[cfg(test)]
mod tests {
    use super::{SaveOutcome, SaveRequest, WriteJob};
    use aero_core::{Document, Edit, Vault, VaultPath};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    fn request() -> (tempfile::TempDir, SaveRequest) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), b"base\n").unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let path = VaultPath::new("a.md").unwrap();
        let (mut doc, known) = vault.read(&path).unwrap();
        doc.edit(Edit::insert(0, "first ")).unwrap();
        let request = SaveRequest {
            vault,
            path,
            known,
            conflict_base: VaultPath::new("a.conflict-test.md").unwrap(),
            snapshot: doc.snapshot(),
            bom: true,
            ending: aero_core::LineEnding::CrLf,
            epoch: 7,
            allow_missing: false,
        };
        (dir, request)
    }

    #[test]
    fn native_barrier_waits_pending_write_then_persists_newest_snapshot() {
        let (dir, first) = request();
        let mut newest = first.clone();
        let mut doc = Document::from_bytes(b"first base\n").unwrap();
        doc.edit(Edit::insert(0, "newest ")).unwrap();
        doc.edit(Edit::insert(0, "latest ")).unwrap();
        newest.snapshot = doc.snapshot();
        let (release, gate) = mpsc::channel();
        let pending = WriteJob::spawn(first.path.clone(), first.epoch, first.snapshot.version(), move || {
            gate.recv().unwrap();
            first.save()
        });
        let guard = newest.finish(Some(pending), true, None);
        assert_eq!(std::fs::read(dir.path().join("a.md")).unwrap(), b"base\n");
        release.send(()).unwrap();
        assert!(guard.wait().result.unwrap().is_some());
        assert_eq!(
            std::fs::read(dir.path().join("a.md")).unwrap(),
            b"\xef\xbb\xbflatest newest first base\r\n"
        );
        assert!(
            !dir.path().join("a.conflict-test.md").exists(),
            "our pending write is not an external conflict"
        );
    }

    #[test]
    fn native_barrier_failed_backup_never_overwrites_disk() {
        let (dir, mut request) = request();
        std::fs::write(dir.path().join("a.md"), b"external version\n").unwrap();
        request.conflict_base = VaultPath::new(&format!("{}.md", "x".repeat(256))).unwrap();
        let guard = request.finish(None, true, None);
        assert!(guard.wait().result.is_err());
        assert_eq!(std::fs::read(dir.path().join("a.md")).unwrap(), b"external version\n");
    }

    #[test]
    fn native_barrier_drop_waits_even_without_polling_async_wait() {
        let (dir, request) = request();
        let (release, gate) = mpsc::channel();
        let pending = WriteJob::spawn(request.path.clone(), request.epoch, 0, move || {
            gate.recv().unwrap();
            SaveOutcome { result: Ok(None), conflict: None }
        });
        let guard = request.finish(Some(pending), true, None);
        let (dropping, entered) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            dropping.send(()).unwrap();
            drop(guard);
            done.send(()).unwrap();
        });
        entered.recv().unwrap();
        assert!(finished.recv_timeout(Duration::from_millis(30)).is_err(), "Drop must wait for accepted I/O");
        release.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(5)).unwrap();
        thread.join().unwrap();
        assert_eq!(std::fs::read(dir.path().join("a.md")).unwrap(), b"\xef\xbb\xbffirst base\r\n");
    }

    #[gpui_kit::test]
    fn native_barrier_captures_new_input_before_accepted_save_is_polled(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        let (dir, request) = request();
        let (doc, stamp) = request.vault.read(&request.path).unwrap();
        cx.update(|cx| gpui_kit::init(cx));
        let editor = cx.new(|cx| super::Editor::new(request.vault, request.path, doc, stamp, cx));
        let (guard, wait) = editor.update(cx, |e, cx| {
            e.doc.edit(Edit::insert(0, "first ")).unwrap();
            e.after_doc_change(cx);
            e.save_now(cx);
            e.doc.edit(Edit::insert(0, "newest ")).unwrap();
            e.after_doc_change(cx);
            e.native_save_barrier(cx)
        });
        drop(wait);
        drop(guard);
        assert_eq!(std::fs::read(dir.path().join("a.md")).unwrap(), b"newest first base\n");
        assert_eq!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().contains("conflict"))
                .count(),
            0
        );
    }

    #[gpui_kit::test]
    fn suspension_cancels_checks_and_blocks_save_restore_and_disk_choice(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        let (dir, request) = request();
        let (doc, stamp) = request.vault.read(&request.path).unwrap();
        cx.update(|cx| gpui_kit::init(cx));
        let editor = cx.new(|cx| super::Editor::new(request.vault, request.path, doc, stamp, cx));
        editor.update(cx, |e, cx| {
            e.check_external(cx);
            assert!(e.suspend_io(cx));
            assert!(e.io.checking.is_none());
            std::fs::rename(dir.path().join("a.md"), dir.path().join("new.md")).unwrap();
            e.check_external(cx);
            assert!(e.io.checking.is_none());
            e.set_banner(Some(super::Banner::Deleted), cx);
            e.restore_deleted(cx);
            assert!(!e.io.writing);
            e.dirty = true;
            e.save_now(cx);
            assert!(!e.io.writing);
            assert!(e.flush_sync().is_err());
            let copy = VaultPath::new("copy.md").unwrap();
            e.vault.create_bytes(&copy, b"other\n").unwrap();
            e.set_banner(Some(super::Banner::Conflict { copy }), cx);
            e.take_disk_version(cx);
        });
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| editor.read(cx).doc.to_string()), "base\n");
        assert!(!dir.path().join("a.md").exists());
        editor.update(cx, |e, cx| {
            e.set_path(VaultPath::new("new.md").unwrap(), cx);
            e.resume_io(cx);
            assert!(!e.io.suspended);
            e.save_now(cx);
        });
        cx.run_until_parked();
        assert!(!dir.path().join("a.md").exists());
    }

    #[test]
    fn native_barrier_does_not_recreate_missing_file() {
        let (dir, request) = request();
        std::fs::remove_file(dir.path().join("a.md")).unwrap();
        let guard = request.finish(None, true, None);
        assert!(guard.wait().result.unwrap().is_none());
        assert!(!dir.path().join("a.md").exists());
    }
}
