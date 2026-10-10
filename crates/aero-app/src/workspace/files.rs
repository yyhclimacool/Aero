//! Ordered file operations share the editor save barrier. Native termination
//! retains a join guard so accepted disk work cannot outlive the process.
use super::*;
use aero_core::VaultError;
use std::sync::mpsc;

#[derive(Clone)]
pub(crate) enum Operation {
    Create { directory: String, base: String, unique: bool, text: String },
    Import { paths: Vec<PathBuf>, directory: String, note: Option<VaultPath> },
    AttachmentRename { from: VaultPath, to: VaultPath },
    Trash { path: VaultPath, note: bool },
}

#[derive(Default)]
struct Outcome {
    open: Option<(VaultPath, bool)>,
    trashed: Option<VaultPath>,
    touched: Vec<VaultPath>,
    error: Option<String>,
}

pub(super) struct Guard(Option<std::thread::JoinHandle<()>>);
impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(thread) = self.0.take() {
            let _ = thread.join();
        }
    }
}

pub(super) fn finish_on_shutdown(
    current: Option<Guard>,
    queued: Vec<Operation>,
    vault: Arc<Vault>,
) -> Option<Guard> {
    if queued.is_empty() {
        return current;
    }
    Some(Guard(Some(std::thread::spawn(move || {
        drop(current);
        for operation in queued {
            if let Err(error) = execute(&vault, operation) {
                log::error!("file operation during shutdown: {error}");
            }
        }
    }))))
}

fn collision(error: &VaultError) -> bool {
    matches!(error, VaultError::AlreadyExists(_))
        || matches!(error, VaultError::Io { source, .. } if source.kind() == std::io::ErrorKind::AlreadyExists)
}

fn create(
    vault: &Vault,
    directory: &str,
    base: &str,
    text: &[u8],
    unique: bool,
) -> Result<(VaultPath, bool), String> {
    for suffix in 0.. {
        let name = if suffix == 0 { format!("{base}.md") } else { format!("{base} {suffix}.md") };
        let path = VaultPath::new(&if directory.is_empty() { name } else { format!("{directory}/{name}") })
            .map_err(|e| e.to_string())?;
        if vault.exists(&path) {
            if unique {
                continue;
            } else {
                return Ok((path, false));
            }
        }
        match vault.create_bytes(&path, text) {
            Ok(_) => return Ok((path, true)),
            Err(error) if collision(&error) && unique => continue,
            Err(error) if collision(&error) => return Ok((path, false)),
            Err(error) => return Err(error.to_string()),
        }
    }
    unreachable!()
}

fn execute(vault: &Vault, operation: Operation) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    match operation {
        Operation::Create { directory, base, unique, text } => {
            outcome.open = Some(create(vault, &directory, &base, text.as_bytes(), unique)?);
        }
        Operation::Import { paths, directory, note } => {
            for source in paths {
                if source.is_dir() {
                    continue;
                }
                let result = (|| {
                    let name = source.file_name().ok_or("无效文件名")?.to_string_lossy();
                    let bytes = std::fs::read(&source).map_err(|e| format!("{name}：{e}"))?;
                    if name.to_ascii_lowercase().ends_with(".md") {
                        outcome
                            .touched
                            .push(create(vault, &directory, &name[..name.len() - 3], &bytes, true)?.0);
                    } else if let Some(note) = &note {
                        let extension = source.extension().map(|e| e.to_string_lossy()).unwrap_or_default();
                        let path = aero_core::attachments::import_path(vault, note, Some(&name), &extension)
                            .ok_or_else(|| format!("无法导入 {name}"))?;
                        vault.create_bytes(&path, &bytes).map_err(|e| e.to_string())?;
                    } else {
                        return Err(format!("{name} 不是笔记。请拖到某一篇笔记上。"));
                    }
                    Ok::<_, String>(())
                })();
                if let Err(error) = result {
                    outcome.error.get_or_insert(error);
                }
            }
        }
        Operation::AttachmentRename { from, to } => {
            let report = aero_core::rename::RenamePlan::prepare_attachment(vault, from, to)
                .and_then(|plan| plan.execute())
                .map_err(|e| e.to_string())?;
            outcome.touched = report.updated;
        }
        Operation::Trash { path, note } => {
            crate::trash::move_to_trash(&vault.fs_path(&path))?;
            if note {
                outcome.trashed = Some(path);
            }
        }
    }
    Ok(outcome)
}

#[cfg(test)]
pub(crate) fn execute_test(vault: &Vault, operation: Operation) -> Result<Option<String>, String> {
    execute(vault, operation).map(|outcome| outcome.error)
}

impl Workspace {
    pub(super) fn execute_file_operation(
        &mut self,
        operation: Operation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = &self.open else { return };
        let vault = open.vault.clone();
        let editors: Vec<_> = open.editors().cloned().collect();
        for editor in &editors {
            if !editor.update(cx, |editor, cx| editor.suspend_io(cx)) {
                for editor in &editors {
                    editor.update(cx, |e, cx| e.resume_io(cx));
                }
                window.push_notification(Notification::warning("笔记仍在保存，请重试文件操作。"), cx);
                return;
            }
        }
        let paths = editors.iter().map(|e| e.read(cx).path().clone()).collect::<Vec<_>>();
        if let Some(open) = self.open.as_mut() {
            for tab in &mut open.tabs {
                tab.retarget(&Clone::clone);
            }
        }
        self.session_closing = true;
        self.quick_open = None;
        self.focus_handle.focus(window, cx);
        let (send, receive) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let result = execute(&vault, operation);
            let loaded = paths
                .into_iter()
                .filter(|path| {
                    result.as_ref().map_or(true, |outcome| {
                        outcome.touched.contains(path) || outcome.trashed.as_ref() == Some(path)
                    })
                })
                .map(|path| {
                    let document = vault.read(&path);
                    (path, document)
                })
                .collect::<Vec<_>>();
            let _ = send.send((result, loaded));
        });
        self.file_guard = Some(Guard(Some(thread)));
        self.io_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(async move { receive.recv() }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.file_guard = None;
                this.io_task = None;
                this.session_closing = false;
                let Some(open) = &this.open else { return };
                let editors: Vec<_> = open.editors().cloned().collect();
                let (outcome, loaded) = match result {
                    Ok((result, loaded)) => (result, loaded),
                    Err(error) => (Err(error.to_string()), Vec::new()),
                };
                for editor in &editors {
                    let path = editor.read(cx).path().clone();
                    if let Some((_, Ok((document, stamp)))) = loaded.iter().find(|(p, _)| p == &path)
                        && stamp.hash != editor.read(cx).reference_snapshot().0.hash
                    {
                        editor.update(cx, |e, cx| {
                            e.finish_file_operation(path, Ok((document.clone(), *stamp)), cx)
                        });
                    } else {
                        editor.update(cx, |e, cx| e.resume_io(cx));
                    }
                }
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => Outcome { error: Some(error), ..Default::default() },
                };
                open.tree.update(cx, |t, cx| {
                    t.set_operation_error(outcome.error.clone());
                    t.rescan_in_background(cx);
                });
                if !outcome.touched.is_empty() {
                    open.search.update(cx, |s, cx| s.notes_touched(outcome.touched.clone(), cx));
                }
                for editor in editors {
                    editor.update(cx, |e, cx| e.check_external(cx));
                }
                if let Some(error) = outcome.error {
                    window.push_notification(Notification::error(error).title("文件操作"), cx);
                }
                if let Some(path) = outcome.trashed {
                    this.drop_tab_showing(&path, window, cx);
                    if let Some(open) = &this.open {
                        open.search.update(cx, |s, cx| s.note_removed(path.clone(), cx));
                    }
                    this.set_favorites(this.favorites().into_iter().filter(|p| p != &path).collect(), cx);
                }
                if let Some((path, created)) = outcome.open {
                    this.open_note(path.clone(), window, cx);
                    if created {
                        this.navigate_note(&path, tabs::Navigation::Write, window, cx);
                    }
                }
                if let Some(open) = &this.open {
                    this.load_editor(open.active, window, cx);
                }
                this.refresh_active_tab(true, window, cx);
                this.refresh_attachment_references(cx);
                let changes = std::mem::take(&mut this.rename_changes);
                if !changes.is_empty() {
                    this.on_vault_changed(&changes, window, cx);
                }
                this.continue_operation(window, cx);
                cx.notify();
            });
        }));
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{Operation, execute, finish_on_shutdown};
    use aero_core::Vault;
    use std::sync::Arc;

    #[test]
    fn import_batch_keeps_order_reports_failure_and_never_overwrites() {
        let root = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        std::fs::write(first.path().join("same.md"), b"first").unwrap();
        std::fs::write(second.path().join("same.md"), b"second").unwrap();
        std::fs::write(root.path().join("same.md"), b"existing").unwrap();
        let vault = Vault::open(root.path()).unwrap();
        let outcome = execute(
            &vault,
            Operation::Import {
                paths: vec![
                    first.path().join("same.md"),
                    first.path().join("missing.md"),
                    second.path().join("same.md"),
                ],
                directory: String::new(),
                note: None,
            },
        )
        .unwrap();
        assert!(outcome.error.unwrap().contains("missing.md"));
        assert_eq!(std::fs::read(root.path().join("same.md")).unwrap(), b"existing");
        assert_eq!(std::fs::read(root.path().join("same 1.md")).unwrap(), b"first");
        assert_eq!(std::fs::read(root.path().join("same 2.md")).unwrap(), b"second");
    }

    #[test]
    fn native_shutdown_guard_finishes_accepted_queue_without_polling_a_future() {
        let root = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(root.path()).unwrap());
        let operation = || Operation::Create {
            directory: String::new(),
            base: "queued".into(),
            unique: true,
            text: "kept".into(),
        };
        drop(finish_on_shutdown(None, vec![operation(), operation()], vault));
        assert_eq!(std::fs::read(root.path().join("queued.md")).unwrap(), b"kept");
        assert_eq!(std::fs::read(root.path().join("queued 1.md")).unwrap(), b"kept");
    }
}
