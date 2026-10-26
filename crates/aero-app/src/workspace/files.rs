//! Ordered file operations share the editor save barrier. Native termination
//! retains a join guard so accepted disk work cannot outlive the process.
use super::*;
use aero_core::VaultError;
use aero_core::transaction::{FileChange, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SyncRevision {
    pub(crate) entity: EntityId,
    pub(crate) path: VaultPath,
    pub(crate) version: u64,
    pub(crate) epoch: u64,
}

#[derive(Default)]
struct Busy(std::collections::HashSet<String>);
impl Global for Busy {}
pub(super) fn busy(vault: &str, cx: &App) -> bool {
    cx.try_global::<Busy>().is_some_and(|b| b.0.contains(vault))
}
fn set_busy(vault: &str, value: bool, cx: &mut App) {
    if !cx.has_global::<Busy>() {
        cx.set_global(Busy::default());
    }
    cx.update_global::<Busy, _>(|b, _| {
        if value {
            b.0.insert(vault.into());
        } else {
            b.0.remove(vault);
        }
    });
}

#[derive(Default)]
pub(super) struct Control(std::sync::atomic::AtomicU8);
impl Control {
    pub(super) fn cancel(&self) {
        let _ = self.0.compare_exchange(
            0,
            2,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        );
    }
    fn begin_commit(&self) -> Result<(), String> {
        self.0
            .compare_exchange(0, 1, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst)
            .map(|_| ())
            .map_err(|_| "操作已取消，源内容已恢复".into())
    }
}

fn commit(
    vault: &Vault,
    journal: &Path,
    changes: Vec<FileChange>,
    control: Option<&Control>,
) -> Result<(), String> {
    commit_restoring(vault, journal, changes, control, Default::default())
}

fn commit_restoring(
    vault: &Vault,
    journal: &Path,
    changes: Vec<FileChange>,
    control: Option<&Control>,
    restorations: std::collections::HashMap<VaultPath, Vec<u8>>,
) -> Result<(), String> {
    let transaction = Transaction::prepare_restoring(vault, journal, changes, restorations)?;
    if let Some(control) = control
        && let Err(error) = control.begin_commit()
    {
        transaction.cancel(vault)?;
        return Err(error);
    }
    transaction.commit(vault).map(|_| ())
}

#[derive(Clone)]
pub(crate) enum Operation {
    Paste {
        token: String,
        payload: aero_core::clipboard::Payload,
        source: String,
        destination: VaultPath,
        before: Vec<u8>,
        range: std::ops::Range<usize>,
        selection: Option<crate::editor::SelectionPlan>,
        cut: Option<FileChange>,
        pending_cut: Option<Arc<std::sync::Mutex<aero_core::transaction::PendingCut>>>,
        journal: PathBuf,
    },
    CreateChild {
        parent: VaultPath,
        before: Vec<u8>,
        range: std::ops::Range<usize>,
        journal: PathBuf,
    },
    Reparent {
        path: VaultPath,
        parent: Option<VaultPath>,
        before: Option<VaultPath>,
        journal: PathBuf,
    },
    Transaction {
        token: String,
        changes: Vec<FileChange>,
        history_changes: Option<Vec<FileChange>>,
        direction: Option<bool>,
        open: Option<VaultPath>,
        journal: PathBuf,
    },
    SyncReference {
        token: String,
        changes: Vec<FileChange>,
        history_changes: Option<Vec<FileChange>>,
        membership: BTreeSet<VaultPath>,
        revisions: BTreeMap<EntityId, SyncRevision>,
        journal: PathBuf,
    },
    Create {
        directory: String,
        base: String,
        unique: bool,
        text: String,
    },
    Import {
        paths: Vec<PathBuf>,
        directory: String,
        note: Option<VaultPath>,
    },
    ImportCopy {
        path: VaultPath,
        journal: PathBuf,
    },
    AttachmentRename {
        from: VaultPath,
        to: VaultPath,
    },
    Trash {
        path: VaultPath,
        note: bool,
    },
}

impl Operation {
    pub(super) fn validate_live(&self, vault: &str, cx: &App) -> Result<(), String> {
        if let Self::SyncReference { revisions, .. } = self {
            let actual = Editor::live_editors(vault, cx)
                .into_iter()
                .map(|editor| (editor.entity_id(), editor.read(cx).sync_revision(editor.entity_id())))
                .collect::<BTreeMap<_, _>>();
            if &actual != revisions {
                return Err("笔记已变化，请重新选择来源".into());
            }
        }
        Ok(())
    }

    pub(super) fn cancel(&self, cx: &mut App) {
        if let Self::Paste { token, cut: Some(_), .. } = self {
            Editor::complete_cut(token, false, cx);
        }
    }
    pub(super) fn set_journal(&mut self, path: PathBuf) {
        match self {
            Self::Paste { journal, .. }
            | Self::Transaction { journal, .. }
            | Self::SyncReference { journal, .. }
            | Self::CreateChild { journal, .. }
            | Self::Reparent { journal, .. }
            | Self::ImportCopy { journal, .. } => *journal = path,
            _ => {}
        }
    }
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Paste { .. } => "block_paste",
            Self::CreateChild { .. } => "document_child_create",
            Self::Reparent { .. } => "document_reparent",
            Self::Transaction { .. } => "workspace_transaction",
            Self::SyncReference { .. } => "sync_reference",
            Self::Create { .. } => "file_create",
            Self::Import { .. } => "file_import",
            Self::ImportCopy { .. } => "document_import_copy",
            Self::AttachmentRename { .. } => "attachment_rename",
            Self::Trash { .. } => "file_trash",
        }
    }
}

#[derive(Default)]
struct Outcome {
    history: Option<(String, Vec<FileChange>, Option<bool>)>,
    source_metadata: Vec<FileChange>,
    show_documents: bool,
    open: Option<(VaultPath, bool)>,
    trashed: Option<VaultPath>,
    removed: Vec<(VaultPath, Option<VaultPath>)>,
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
    queued: Vec<(Operation, crate::diagnostics::Operation)>,
    vault: Arc<Vault>,
) -> Option<Guard> {
    if queued.is_empty() {
        return current;
    }
    Some(Guard(Some(std::thread::spawn(move || {
        drop(current);
        for (operation, diagnostic) in queued {
            if let Err(error) = diagnostic.scope(|| execute(&vault, operation)) {
                diagnostic.finish(crate::diagnostics::Outcome::Failed);
                log::error!("file operation during shutdown: {error}");
            } else {
                diagnostic.finish(crate::diagnostics::Outcome::Ok);
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
    execute_controlled(vault, operation, None)
}

fn execute_controlled(
    vault: &Vault,
    operation: Operation,
    control: Option<&Control>,
) -> Result<Outcome, String> {
    execute_with_paste_snapshot(vault, operation, control, || {})
}

fn execute_with_paste_snapshot(
    vault: &Vault,
    operation: Operation,
    control: Option<&Control>,
    on_paste_snapshot: impl FnOnce(),
) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    match operation {
        Operation::Paste {
            token,
            payload,
            source,
            destination,
            before,
            range,
            selection,
            cut,
            pending_cut,
            journal,
        } => {
            let (actual, _) = vault.read_bytes(&destination).map_err(|e| e.to_string())?;
            if actual != before {
                return Err("目标文档已变化，请重新粘贴".into());
            }
            let mut doc = aero_core::Document::from_bytes(&before).map_err(|e| e.to_string())?;
            let (document, metadata) = aero_core::identity::ensure(&doc)?;
            on_paste_snapshot();
            let plan = aero_core::clipboard::Relocation::prepare(
                vault,
                &source,
                &payload,
                &destination,
                &document,
                cut.is_some(),
            )?;
            doc = if let Some(selection) = selection {
                selection.replace(&doc, &plan.source)?
            } else {
                crate::editor::insert_tree(&doc, range, &plan.source)?
            };
            // Metadata is inserted after the body, so its original prefix offsets remain valid.
            doc.edit_group(metadata).map_err(|e| e.to_string())?;
            if cut.is_some() {
                let text = plan.rewrite(&doc.to_string(), &destination);
                doc.edit(aero_core::Edit::replace(0..doc.len(), text)).map_err(|e| e.to_string())?;
            }
            let mut changes = plan.notes;
            let target = changes.iter_mut().find(|c| c.path == destination).ok_or("目标文档不在笔记库中")?;
            // The inserted document came from the editor's earlier snapshot.
            // A newer destination read by the all-note scan cannot authorize
            // overwriting that new content with the old insertion result.
            if target.before.as_deref() != Some(before.as_slice()) {
                return Err("目标文档在准备期间已变化，请重新粘贴".into());
            }
            target.after = Some(doc.to_bytes());
            changes.extend(plan.resources);
            let mut history: Vec<_> = changes.iter().filter(|c| c.before != c.after).cloned().collect();
            let mut restorations = std::collections::HashMap::new();
            if let Some(cut) = cut {
                let current = vault.read_bytes(&cut.path).map_err(|e| e.to_string())?.0;
                if Some(current) != cut.after {
                    return Err("剪切来源已变化，请先撤销源文档中的后续编辑".into());
                }
                if let Some(before) = &cut.before {
                    restorations.insert(cut.path.clone(), before.clone());
                }
                if let Some(source) = history.iter_mut().find(|c| c.path == cut.path) {
                    source.before = cut.before;
                } else {
                    history.push(cut);
                }
            }
            if let Some(pending) = pending_cut {
                // Retain ownership through sealing/rollback so editor Drop cannot race the worker.
                let mut pending = pending.lock().map_err(|_| "剪切恢复锁已损坏".to_owned())?;
                let transaction = pending.prepare_paste(vault, changes, restorations)?;
                if let Some(control) = control
                    && let Err(error) = control.begin_commit()
                {
                    transaction.cancel(vault)?;
                    return Err(error);
                }
                transaction.commit(vault)?;
            } else {
                if !restorations.is_empty() {
                    return Err("剪切缺少早期恢复日志".into());
                }
                commit_restoring(vault, &journal, changes, control, restorations)?;
            }
            outcome.touched = history.iter().map(|c| c.path.clone()).collect();
            outcome.history = Some((token, history, None));
        }
        Operation::CreateChild { parent, before, range, journal } => {
            let (doc, _) = vault.read(&parent).map_err(|e| e.to_string())?;
            if doc.to_bytes() != before {
                return Err("创建位置已变化，请重新执行子文档命令".into());
            }
            let (child, changes) = aero_core::relations::child_at(vault, &parent, &doc, range)?;
            let mut outcome = execute_controlled(
                vault,
                Operation::Transaction {
                    token: aero_core::identity::new_block_id(),
                    changes,
                    history_changes: None,
                    direction: None,
                    open: Some(child),
                    journal,
                },
                control,
            )?;
            outcome.show_documents = true;
            return Ok(outcome);
        }
        Operation::Reparent { path, parent, before, journal } => {
            let changes = aero_core::relations::reparent(vault, &path, parent.as_ref(), before.as_ref())?;
            return execute_controlled(
                vault,
                Operation::Transaction {
                    token: aero_core::identity::new_block_id(),
                    changes,
                    history_changes: None,
                    direction: None,
                    open: None,
                    journal,
                },
                control,
            );
        }
        Operation::SyncReference { token, changes, history_changes, membership, journal, .. } => {
            let transaction =
                Transaction::prepare(vault, &journal, changes.clone())?.observe_notes(membership);
            if let Some(control) = control
                && let Err(error) = control.begin_commit()
            {
                transaction.cancel(vault)?;
                return Err(error);
            }
            transaction.commit(vault)?;
            let changed = changes.into_iter().filter(|c| c.before != c.after).collect::<Vec<_>>();
            let history = history_changes.unwrap_or_else(|| changed.clone());
            outcome.source_metadata =
                changed.iter().filter(|c| !history.iter().any(|h| h.path == c.path)).cloned().collect();
            outcome.touched = changed.iter().map(|c| c.path.clone()).collect();
            outcome.history = Some((token, history, None));
        }
        Operation::Transaction { token, changes, history_changes, direction, open, journal } => {
            if changes.is_empty() {
                return Ok(outcome);
            }
            commit(vault, &journal, changes.clone(), control)?;
            outcome.touched =
                changes.iter().filter(|c| c.before != c.after).map(|c| c.path.clone()).collect();
            // Resolve the related parent on the disk worker, before history
            // snapshots move away. Undo may remove the parent's newly added ID.
            let metadata = |bytes: &Option<Vec<u8>>| {
                aero_core::Document::from_bytes(bytes.as_deref()?)
                    .ok()
                    .and_then(|doc| aero_core::identity::read(&doc).ok())
            };
            outcome.removed = changes
                .iter()
                .filter(|change| change.after.is_none())
                .map(|change| {
                    let parent = metadata(&change.before).and_then(|meta| meta.parent).and_then(|id| {
                        changes
                            .iter()
                            .find(|candidate| {
                                candidate.after.is_some()
                                    && [&candidate.before, &candidate.after].into_iter().any(|bytes| {
                                        metadata(bytes).is_some_and(|meta| meta.id.as_ref() == Some(&id))
                                    })
                            })
                            .map(|candidate| candidate.path.clone())
                    });
                    (change.path.clone(), parent)
                })
                .collect();
            outcome.open = open.map(|p| (p, true));
            outcome.history = Some((
                token,
                history_changes
                    .unwrap_or_else(|| changes.into_iter().filter(|c| c.before != c.after).collect()),
                direction,
            ));
        }
        Operation::Create { directory, base, unique, text } => {
            let mut document = aero_core::Document::from_text(&text);
            let (_, edits) = aero_core::identity::ensure(&document)?;
            document.edit_group(edits).map_err(|e| e.to_string())?;
            outcome.open = Some(create(vault, &directory, &base, &document.to_bytes(), unique)?);
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
        Operation::ImportCopy { path, journal } => {
            let (before, _) = vault.read_bytes(&path).map_err(|e| e.to_string())?;
            let original = aero_core::Document::from_bytes(&before).map_err(|e| e.to_string())?;
            let meta = aero_core::identity::read(&original)?;
            let id = aero_core::identity::new_document_id();
            let copied = aero_core::clipboard::copy_tree(&original.to_string(), &aero_core::clipboard::Payload {
                version: 1, vault: vault.id().into(), path: path.to_string(), document: meta.id, cut: None,
            }, &id)?;
            let mut copied = aero_core::Document::from_text(&copied);
            if let Some(old) = aero_core::identity::read(&original)?.id {
                let source = copied.to_string();
                let mut references = aero_core::links::scan_for_rename(&source).into_iter()
                    .filter(|span| span.target == format!("id:{old}")
                        && !span.fragment.as_deref().is_some_and(|fragment| fragment.starts_with('^')))
                    .map(|span| aero_core::Edit::replace(aero_core::links::target_range(&source, &span), format!("id:{id}")))
                    .collect::<Vec<_>>();
                references.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
                copied.edit_group(references).map_err(|e| e.to_string())?;
                copied.reparse();
            }
            let edits = aero_core::identity::prepare(&copied, &aero_core::identity::Patch { id: Some(id), ..Default::default() })?;
            copied.edit_group(edits).map_err(|e| e.to_string())?;
            copied.bom = original.bom;
            copied.line_ending = original.line_ending;
            let changes = vec![FileChange { path, before: Some(before), after: Some(copied.to_bytes()) }];
            return execute_controlled(vault, Operation::Transaction {
                token: aero_core::identity::new_block_id(), changes, history_changes: None,
                direction: None, open: None, journal,
            }, control);
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
        diagnostic: crate::diagnostics::Operation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = &self.open else { return };
        let vault = open.vault.clone();
        if let Err(error) = operation.validate_live(vault.id(), cx) {
            diagnostic.finish(crate::diagnostics::Outcome::Stale);
            window.push_notification(Notification::warning(error), cx);
            self.continue_operation(window, cx);
            return;
        }
        let cut_token = match &operation {
            Operation::Paste { token, cut: Some(_), .. } => Some(token.clone()),
            _ => None,
        };
        let editors = Editor::live_editors(vault.id(), cx);
        if let Operation::Transaction { token, direction: Some(redo), changes, .. } = &operation {
            if editors.iter().any(|editor| {
                let e = editor.read(cx);
                changes.iter().any(|c| &c.path == e.path())
                    && e.history_transaction(*redo) != Some(token.as_str())
            }) {
                window.push_notification(Notification::warning("请先撤销参与文档中的后续编辑。"), cx);
                return;
            }
        }
        set_busy(vault.id(), true, cx);
        for editor in &editors {
            if !editor.update(cx, |editor, cx| editor.suspend_io(cx)) {
                for editor in &editors {
                    editor.update(cx, |e, cx| e.resume_io(cx));
                }
                window.push_notification(Notification::warning("笔记仍在保存，请重试文件操作。"), cx);
                set_busy(vault.id(), false, cx);
                operation.cancel(cx);
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
        let control = Arc::new(Control::default());
        self.file_control = Some(control.clone());
        let worker_diagnostic = diagnostic.clone();
        let thread = std::thread::spawn(move || {
            worker_diagnostic.stage("disk_work", Default::default());
            let result = worker_diagnostic.scope(|| execute_controlled(&vault, operation, Some(&control)));
            worker_diagnostic.stage("disk_finished", Default::default());
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
                diagnostic.stage("ui_apply", Default::default());
                this.file_control = None;
                this.io_task = None;
                this.session_closing = false;
                let Some(open) = &this.open else {
                    this.file_guard = None;
                    return;
                };
                let vault_id = open.vault.id().to_owned();
                let (outcome, loaded) = match result {
                    Ok((result, loaded)) => (result, loaded),
                    Err(error) => (Err(error.to_string()), Vec::new()),
                };
                if let Ok(outcome) = &outcome
                    && let Some((token, changes, direction)) = &outcome.history
                {
                    super::history::record(open.vault.id(), token, changes, *direction, cx);
                }
                let mut completion_error = None;
                for editor in &editors {
                    let path = editor.read(cx).path().clone();
                    if let Ok(outcome) = &outcome
                        && let Some(change) = outcome.source_metadata.iter().find(|c| c.path == path)
                    {
                        let stamp = loaded
                            .iter()
                            .find(|(p, _)| p == &path)
                            .and_then(|(_, result)| result.as_ref().ok())
                            .map(|(_, stamp)| *stamp);
                        if let Err(error) =
                            editor.update(cx, |e, cx| e.finish_source_metadata(change, stamp, cx))
                        {
                            completion_error.get_or_insert(error);
                        }
                        continue;
                    }
                    if outcome.is_err()
                        && let Some(token) = &cut_token
                        && let Some((_, Ok((document, stamp)))) = loaded.iter().find(|(p, _)| p == &path)
                        && editor.update(cx, |e, cx| e.accept_cut_recovery(token, document, *stamp, cx))
                    {
                        continue;
                    }
                    if let Ok(outcome) = &outcome
                        && let Some((token, changes, direction)) = &outcome.history
                        && let Some(change) = changes.iter().find(|c| c.path == path)
                    {
                        let loaded =
                            loaded.iter().find(|(p, _)| p == &path).and_then(|(_, r)| r.as_ref().ok());
                        editor.update(cx, |e, cx| {
                            e.finish_transaction(
                                token,
                                change,
                                *direction,
                                loaded.map(|(_, stamp)| *stamp),
                                cx,
                            );
                            if let Some((_, parent)) = outcome.removed.iter().find(|(p, _)| p == &path) {
                                e.retire_transaction_source(cx);
                                cx.emit(EditorEvent::TransactionRemoved(parent.clone()));
                            }
                        });
                        continue;
                    }
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
                let mut outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => Outcome { error: Some(error), ..Default::default() },
                };
                if let Some(error) = completion_error {
                    outcome.error.get_or_insert(error);
                }
                if let Some(token) = &cut_token {
                    Editor::complete_cut(token, outcome.error.is_none(), cx);
                }
                diagnostic.finish(if outcome.error.is_some() {
                    crate::diagnostics::Outcome::Failed
                } else {
                    crate::diagnostics::Outcome::Ok
                });
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
                if outcome.show_documents {
                    if let Some(open) = &this.open {
                        open.tree.update(cx, |tree, cx| tree.set_documents(true, cx));
                    }
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
                // Editor events close removed tabs at the end of this effect
                // cycle. Keep the file guard and vault barrier until those
                // events finish, before accepting the next queued operation.
                cx.defer_in(window, move |this, window, cx| {
                    this.file_guard = None;
                    set_busy(&vault_id, false, cx);
                    this.continue_operation(window, cx);
                    cx.notify();
                });
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
    fn explicit_copy_import_rekeys_only_the_chosen_document_and_undoes() {
        use aero_core::{Document, VaultPath, identity, transaction::Transaction};
        let dir = tempfile::tempdir().unwrap();
        let journal = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let document_id = identity::new_document_id();
        let block_id = identity::new_block_id();
        let alias = identity::new_block_id();
        let source = format!("---\n# keep comment\ntitle: Copy\ncustom: unchanged\naero:\n  id: {document_id}\n---\n\nbody {{.red aero-aliases=\"{alias}\"}} ^{block_id}\n\n![[id:{document_id}#^{block_id}|body]]\n\n[[id:{document_id}|self]]\n\n[[id:{document_id}#标题|heading]]\n");
        let original = VaultPath::new("original.md").unwrap();
        let copy = VaultPath::new("copied.md").unwrap();
        vault.create_bytes(&original, source.as_bytes()).unwrap();
        vault.create_bytes(&copy, source.as_bytes()).unwrap();
        let result = execute(&vault, Operation::ImportCopy { path: copy.clone(), journal: journal.path().into() }).unwrap();
        assert_eq!(vault.read_bytes(&original).unwrap().0, source.as_bytes());
        let (doc, _) = vault.read(&copy).unwrap();
        let new_doc_id = identity::read(&doc).unwrap().id.unwrap();
        assert_ne!(new_doc_id, document_id);
        let block = doc.blocks().iter().find(|(_, b)| b.persisted_id.is_some()).unwrap().1;
        let new_block = block.persisted_id.as_deref().unwrap();
        assert_ne!(new_block, block_id);
        assert!(aero_core::block_refs::aliases(block).next().is_none());
        assert!(doc.to_string().contains("# keep comment\ntitle: Copy\ncustom: unchanged"));
        assert!(doc.to_string().contains(&format!("![[id:{new_doc_id}#^{new_block}|body]]")));
        assert!(doc.to_string().contains(&format!("[[id:{new_doc_id}|self]]")));
        assert!(doc.to_string().contains(&format!("[[id:{new_doc_id}#标题|heading]]")));
        let (_, changes, _) = result.history.unwrap();
        Transaction::prepare(&vault, journal.path(), changes.iter().map(|c| c.reverse()).collect()).unwrap().commit(&vault).unwrap();
        assert_eq!(vault.read_bytes(&copy).unwrap().0, source.as_bytes());
        assert_eq!(Document::from_text(&source).to_bytes(), source.as_bytes());
    }

    #[test]
    fn g02_paste_transaction_replaces_cross_parent_selection_and_undoes_exact_bytes() {
        use aero_core::{Document, VaultPath, clipboard::Payload, transaction::FileChange};
        for pasted in [
            "replacement ^copied",
            ":::highlight {.green}\nchild\n:::\n\n^copied",
            "```text\n::: \t\n```\n\n^copied",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let journal = tempfile::tempdir().unwrap();
            let vault = Vault::open(dir.path()).unwrap();
            let origin = VaultPath::new("source.md").unwrap();
            let target = VaultPath::new("target.md").unwrap();
            let before = ":::highlight {.red custom=one}\nleft kept ^keep-left\n\nfirst ^first\n:::\n\n^left\n\n:::highlight {.blue custom=two}\nsecond ^second\n\nright kept ^keep-right\n:::\n\n^right\n\n<!-- opaque  bytes -->\n";
            vault.create_bytes(&origin, pasted.as_bytes()).unwrap();
            vault.create_bytes(&target, before.as_bytes()).unwrap();
            let mut doc = Document::from_text(&format!("temporary\n\n{before}"));
            doc.edit(aero_core::Edit::delete(0.."temporary\n\n".len())).unwrap();
            doc.reparse();
            let first = aero_core::block_refs::resolve(&doc, "first").unwrap();
            let second = aero_core::block_refs::resolve(&doc, "second").unwrap();
            let outcome = execute(
                &vault,
                Operation::Paste {
                    token: "g02-paste".into(),
                    payload: Payload {
                        version: 1,
                        vault: vault.id().into(),
                        path: origin.to_string(),
                        document: None,
                        cut: None,
                    },
                    source: pasted.into(),
                    destination: target.clone(),
                    before: before.as_bytes().to_vec(),
                    range: usize::MAX..usize::MAX,
                    selection: Some(crate::editor::SelectionPlan::nodes(&doc, &[first, second])),
                    cut: None,
                    pending_cut: None,
                    journal: journal.path().into(),
                },
            )
            .unwrap();
            let (result, _) = vault.read(&target).unwrap();
            for (name, kept, count) in [("left", "keep-left", 2), ("right", "keep-right", 1)] {
                let id = aero_core::block_refs::resolve(&result, name).unwrap();
                let parent = result.blocks().find(id).unwrap();
                assert!(matches!(parent.kind, aero_core::BlockKind::Directive { .. }), "{result}");
                assert_eq!(
                    parent.children.iter().filter(|b| b.kind != aero_core::BlockKind::Marker).count(),
                    count,
                    "{result}"
                );
                let kept = aero_core::block_refs::resolve(&result, kept).unwrap();
                assert!(parent.children.iter().any(|b| b.id == kept));
            }
            let left =
                result.blocks().find(aero_core::block_refs::resolve(&result, "left").unwrap()).unwrap();
            let copied = &left.children[1];
            let original_tree = Document::from_text(pasted);
            let original_block = &original_tree.blocks().roots[0];
            assert_eq!(copied.kind, original_block.kind);
            assert_eq!(copied.attrs, original_block.attrs);
            assert!(copied.persisted_id.is_some());
            assert_ne!(copied.persisted_id, original_block.persisted_id);
            if matches!(copied.kind, aero_core::BlockKind::Directive { .. }) {
                assert_eq!(copied.children.len(), 1);
                assert_eq!(
                    aero_core::containers::local_source(&result, copied.children[0].id).as_deref(),
                    Some("child")
                );
                assert_eq!(copied.children[0].persisted_id, original_block.children[0].persisted_id);
            } else {
                assert_eq!(
                    aero_core::containers::local_source(&result, copied.id),
                    aero_core::containers::local_source(&original_tree, original_block.id)
                );
            }
            for removed in ["first", "second"] {
                assert!(aero_core::block_refs::resolve(&result, removed).is_err());
            }
            assert!(result.to_string().ends_with("<!-- opaque  bytes -->\n"));
            let (token, history, _) = outcome.history.unwrap();
            assert_eq!(history.len(), 1);
            execute(
                &vault,
                Operation::Transaction {
                    token,
                    changes: history.iter().map(FileChange::reverse).collect(),
                    history_changes: None,
                    direction: Some(false),
                    open: None,
                    journal: journal.path().into(),
                },
            )
            .unwrap();
            assert_eq!(vault.read_bytes(&target).unwrap().0, before.as_bytes());
            assert_eq!(vault.read_bytes(&origin).unwrap().0, pasted.as_bytes());
        }
    }

    #[test]
    fn paste_rejects_destination_changed_between_initial_read_and_relocation_scan() {
        use aero_core::{VaultPath, clipboard::Payload, transaction::FileChange};
        let dir = tempfile::tempdir().unwrap();
        let journal = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let source = VaultPath::new("a-source.md").unwrap();
        let target = VaultPath::new("z-target.md").unwrap();
        let id = aero_core::identity::new_document_id();
        let block = aero_core::identity::new_block_id();
        let moved = format!("move ^{block}\n");
        let source_before = format!("---\naero:\n  id: {id}\n---\n\n{moved}\nkept\n").into_bytes();
        let source_cut = format!("---\naero:\n  id: {id}\n---\n\nkept\n").into_bytes();
        vault.create_bytes(&source, &source_cut).unwrap();
        vault.create_bytes(&target, b"destination\n").unwrap();
        let token = aero_core::identity::new_block_id();
        let operation = Operation::Paste {
            token: token.clone(),
            payload: Payload {
                version: 1,
                vault: vault.id().into(),
                path: source.to_string(),
                document: Some(id),
                cut: Some(token),
            },
            source: moved,
            destination: target.clone(),
            before: b"destination\n".to_vec(),
            range: 11..11,
            selection: None,
            cut: Some(FileChange {
                path: source.clone(),
                before: Some(source_before),
                after: Some(source_cut.clone()),
            }),
            pending_cut: None,
            journal: journal.path().into(),
        };
        let result = super::execute_with_paste_snapshot(&vault, operation, None, || {
            vault.write_bytes(&target, b"external newer text\n").unwrap();
        });
        assert!(
            result.is_err(),
            "a later scan must not legitimize overwriting a newer destination with the initial editor snapshot"
        );
        assert_eq!(vault.read_bytes(&target).unwrap().0, b"external newer text\n");
        assert_eq!(
            vault.read_bytes(&source).unwrap().0,
            source_cut,
            "no transaction writes; the coordinator still owns pending-cut recovery"
        );
        assert_eq!(std::fs::read_dir(journal.path()).unwrap().count(), 0);
    }

    #[test]
    fn g11_pending_cut_later_edits_survive_paste_and_joint_undo() {
        use aero_core::{
            Document, Edit, VaultPath,
            clipboard::Payload,
            transaction::{FileChange, PendingCut},
        };
        let dir = tempfile::tempdir().unwrap();
        let journal = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let source = VaultPath::new("a.md").unwrap();
        let target = VaultPath::new("b.md").unwrap();
        let id = aero_core::identity::new_document_id();
        let block = aero_core::identity::new_block_id();
        let moved = format!("move ^{block}\n\n");
        let original = format!("---\naero:\n  id: {id}\n---\n\n{moved}keep\n");
        vault.create_bytes(&source, original.as_bytes()).unwrap();
        vault.create_bytes(&target, b"destination\n").unwrap();
        let token = aero_core::identity::new_block_id();
        let mut doc = Document::from_text(&original);
        let at = original.find(&moved).unwrap();
        doc.edit_transaction(vec![Edit::delete(at..at + moved.len())], &token).unwrap();
        let mut pending = PendingCut::prepare(
            &vault,
            journal.path(),
            &token,
            FileChange {
                path: source.clone(),
                before: Some(original.as_bytes().to_vec()),
                after: Some(doc.to_bytes()),
            },
            original.as_bytes().to_vec(),
            doc.version(),
        )
        .unwrap();
        let change = FileChange {
            path: source.clone(),
            before: Some(vault.read_bytes(&source).unwrap().0),
            after: Some(doc.to_bytes()),
        };
        pending.publish(&vault, change, original.as_bytes().to_vec(), doc.version()).unwrap();
        doc.edit(Edit::insert(doc.len(), "later\n")).unwrap();
        let restored = doc.without_transaction(&token).unwrap();
        let change = FileChange {
            path: source.clone(),
            before: Some(vault.read_bytes(&source).unwrap().0),
            after: Some(doc.to_bytes()),
        };
        pending.publish(&vault, change, restored.to_bytes(), doc.version()).unwrap();
        let outcome = execute(
            &vault,
            Operation::Paste {
                token: token.clone(),
                payload: Payload {
                    version: 1,
                    vault: vault.id().into(),
                    path: source.to_string(),
                    document: Some(id),
                    cut: Some(token.clone()),
                },
                source: moved,
                destination: target.clone(),
                before: b"destination\n".to_vec(),
                range: 12..12,
                selection: None,
                cut: Some(FileChange {
                    path: source.clone(),
                    before: Some(restored.to_bytes()),
                    after: Some(doc.to_bytes()),
                }),
                pending_cut: Some(Arc::new(std::sync::Mutex::new(pending))),
                journal: journal.path().into(),
            },
        )
        .unwrap();
        let (_, history, _) = outcome.history.unwrap();
        assert_eq!(history.iter().find(|c| c.path == source).unwrap().before, Some(restored.to_bytes()));
        aero_core::transaction::recover(&vault, journal.path()).unwrap();
        assert_eq!(
            vault.read_bytes(&source).unwrap().0,
            doc.to_bytes(),
            "sealed paste must not resurrect a second copy"
        );
        execute(
            &vault,
            Operation::Transaction {
                token,
                changes: history.iter().map(FileChange::reverse).collect(),
                history_changes: None,
                direction: Some(false),
                open: None,
                journal: journal.path().into(),
            },
        )
        .unwrap();
        assert_eq!(vault.read_bytes(&source).unwrap().0, format!("{original}later\n").as_bytes());
        assert_eq!(vault.read_bytes(&target).unwrap().0, b"destination\n");
    }

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
        drop(finish_on_shutdown(
            None,
            (0..2)
                .map(|_| (operation(), crate::diagnostics::Operation::new("file_create", Default::default())))
                .collect(),
            vault,
        ));
        let mut ids = std::collections::HashSet::new();
        for path in ["queued.md", "queued 1.md"] {
            let document =
                aero_core::Document::from_bytes(&std::fs::read(root.path().join(path)).unwrap()).unwrap();
            assert!(document.to_string().ends_with("kept"));
            assert!(ids.insert(aero_core::identity::read(&document).unwrap().id.unwrap()));
        }
    }

    #[test]
    fn cancellation_only_wins_before_disk_commit() {
        let control = super::Control::default();
        control.cancel();
        assert!(control.begin_commit().is_err());
        let control = super::Control::default();
        assert!(control.begin_commit().is_ok());
        control.cancel();
        assert_eq!(control.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
