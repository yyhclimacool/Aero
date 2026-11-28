//! Ordered file operations share the editor save barrier. Native termination
//! retains a join guard so accepted disk work cannot outlive the process.
use super::*;
use aero_core::VaultError;
use aero_core::transaction::{FileChange, Transaction};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc;

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
    prepare_transaction(vault, journal, changes, control, restorations)?.commit(vault).map(|_| ())
}

fn prepare_transaction(
    vault: &Vault,
    journal: &Path,
    changes: Vec<FileChange>,
    control: Option<&Control>,
    restorations: std::collections::HashMap<VaultPath, Vec<u8>>,
) -> Result<Transaction, String> {
    let transaction = Transaction::prepare_restoring(vault, journal, changes, restorations)?;
    if let Some(control) = control
        && let Err(error) = control.begin_commit()
    {
        transaction.cancel(vault)?;
        return Err(error);
    }
    Ok(transaction)
}

#[derive(Clone)]
pub(crate) enum PastePosition {
    Text(std::ops::Range<usize>),
    AfterBlock { offset: usize, prefix: String },
}

pub(crate) struct NoteSnapshot {
    pub known: aero_core::FileStamp,
    pub text: aero_core::Snapshot,
    pub bom: bool,
    pub ending: aero_core::LineEnding,
    pub pending_cut: Option<(aero_core::Document, Vec<String>)>,
}

impl NoteSnapshot {
    fn agrees_with(&self, other: &Self) -> bool {
        let tokens = |snapshot: &Self| snapshot.pending_cut.as_ref().map(|(_, tokens)| tokens.clone());
        self.known.hash == other.known.hash
            && self.bom == other.bom
            && self.ending == other.ending
            && self.text.text() == other.text.text()
            && tokens(self) == tokens(other)
    }

    fn has_other_cut(&self, moving: Option<&str>) -> bool {
        self.pending_cut.as_ref().is_some_and(|(_, tokens)| tokens.last().map(String::as_str) != moving)
    }

    fn projected_bytes(&self) -> Result<Option<Vec<u8>>, String> {
        self.pending_cut
            .as_ref()
            .map(|(doc, tokens)| crate::editor::restore_pending_cuts(doc, tokens).map(|doc| doc.to_bytes()))
            .transpose()
    }

    fn bytes(&self, moving: Option<&str>) -> Result<Vec<u8>, String> {
        if self.has_other_cut(moving)
            && let Some(bytes) = self.projected_bytes()?
        {
            return Ok(bytes);
        }
        Ok(aero_core::SourceText::encode(&String::from(self.text.text()), self.bom, self.ending))
    }
}

#[derive(Clone)]
pub(crate) enum SyncEdit {
    Insert(std::ops::Range<usize>),
    Replace(std::ops::Range<usize>),
}

#[derive(Clone)]
pub(crate) struct SyncReference {
    pub source: VaultPath,
    pub source_before: Vec<u8>,
    pub block: usize,
    pub destination: VaultPath,
    pub destination_before: Vec<u8>,
    pub edit: SyncEdit,
}

#[derive(Clone)]
pub(crate) enum Operation {
    Paste {
        token: String,
        // Keep clipboard metadata off the stack for unrelated file operation events.
        payload: Box<aero_core::clipboard::Payload>,
        source: String,
        destination: VaultPath,
        before: Vec<u8>,
        position: PastePosition,
        cut: Option<FileChange>,
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
    SyncReference {
        request: Box<SyncReference>,
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
    ImportNoteCopy {
        source: PathBuf,
        directory: String,
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
    pub(super) fn uses_editor_snapshots(&self) -> bool {
        matches!(self, Self::Paste { .. } | Self::Transaction { direction: Some(_), .. })
    }

    pub(super) fn cancel(&self, cx: &mut App) {
        if let Self::Paste { token, cut: Some(_), .. } = self {
            Editor::complete_cut(token, false, cx);
        }
    }
    pub(super) fn set_journal(&mut self, path: PathBuf) {
        match self {
            Self::Paste { journal, .. }
            | Self::SyncReference { journal, .. }
            | Self::Transaction { journal, .. }
            | Self::CreateChild { journal, .. }
            | Self::Reparent { journal, .. } => *journal = path,
            _ => {}
        }
    }
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Paste { .. } => "block_paste",
            Self::CreateChild { .. } => "document_child_create",
            Self::Reparent { .. } => "document_reparent",
            Self::SyncReference { .. } => "sync_reference",
            Self::Transaction { .. } => "workspace_transaction",
            Self::Create { .. } => "file_create",
            Self::Import { .. } => "file_import",
            Self::ImportNoteCopy { .. } => "note_copy_import",
            Self::AttachmentRename { .. } => "attachment_rename",
            Self::Trash { .. } => "file_trash",
        }
    }
}

#[derive(Default)]
struct Outcome {
    history: Vec<(String, Vec<FileChange>, Option<bool>)>,
    open: Option<(VaultPath, bool)>,
    show_documents: bool,
    trashed: Option<VaultPath>,
    removed: Vec<(VaultPath, Option<VaultPath>)>,
    touched: Vec<VaultPath>,
    error: Option<String>,
}

fn insert_sync(
    vault: &Vault,
    request: SyncReference,
    journal: &Path,
    control: Option<&Control>,
) -> Result<Outcome, String> {
    use aero_core::{Document, Edit, block_refs, identity};

    let (mut resolver, mut changes) = block_refs::vault_snapshot(vault)?;
    let participant = |path, before: &[u8]| {
        changes
            .iter()
            .position(|c| &c.path == path && c.before.as_deref() == Some(before))
            .ok_or("来源或目标文档已变化，请重新选择同步来源")
    };
    let source_index = participant(&request.source, &request.source_before)?;
    let destination_index = participant(&request.destination, &request.destination_before)?;
    let mut source = Document::from_bytes(&request.source_before).map_err(|e| e.to_string())?;
    let (_, block) = source.blocks().iter().nth(request.block).ok_or("同步来源块已变化")?;
    let assignment = block_refs::permanent_in(&mut resolver, &request.source, &source, block.id)?;
    let reference = format!("!{}", assignment.link(&source.title_or(request.source.stem())));
    let same_document = source_index == destination_index;
    let (mut range, replace) = match request.edit {
        SyncEdit::Insert(range) => (range, false),
        SyncEdit::Replace(range) => (range, true),
    };
    if same_document {
        let original = range.clone();
        for edit in &assignment.edits {
            if edit.range.end <= original.start {
                range.start = range.start.saturating_add_signed(edit.delta());
                range.end = range.end.saturating_add_signed(edit.delta());
            } else if edit.range.start < original.end {
                return Err("同步来源身份与插入位置重叠，请重新选择插入位置".into());
            }
        }
    }
    source.edit_group(assignment.edits).map_err(|e| e.to_string())?;
    let mut destination = if same_document {
        source
    } else {
        changes[source_index].after = Some(source.to_bytes());
        Document::from_bytes(&request.destination_before).map_err(|e| e.to_string())?
    };
    if replace {
        destination.edit(Edit::replace(range, reference)).map_err(|e| e.to_string())?;
    } else {
        destination = crate::editor::insert_tree(&destination, range, &reference)?;
    }
    if same_document {
        destination.reparse();
        block_refs::resolve(&destination, &assignment.id)?;
    }
    changes[destination_index].after = Some(destination.to_bytes());
    let mut outcome = Outcome::default();
    // Commit both files together, but give the source identity its own undo boundary.
    for index in [destination_index, source_index].into_iter().take(if same_document { 1 } else { 2 }) {
        let change = &changes[index];
        if change.before != change.after {
            outcome.touched.push(change.path.clone());
            outcome.history.push((identity::new_block_id(), vec![change.clone()], None));
        }
    }
    let paths: Vec<_> = changes.iter().map(|change| change.path.clone()).collect();
    prepare_transaction(vault, journal, changes, control, Default::default())?.commit_checked(
        vault,
        || {
            if vault.notes().map_err(|e| e.to_string())? != paths {
                return Err("笔记库文件列表已变化，请重新选择同步来源".into());
            }
            Ok(())
        },
    )?;
    Ok(outcome)
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
    execute_with_paste_snapshot(vault, operation, control, &HashMap::new(), || {})
}

fn execute_with_paste_snapshot(
    vault: &Vault,
    operation: Operation,
    control: Option<&Control>,
    editors: &HashMap<VaultPath, NoteSnapshot>,
    on_paste_snapshot: impl FnOnce(),
) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    match operation {
        Operation::Paste { token, payload, source, destination, before, position, cut, journal } => {
            let moving = cut.as_ref().map(|_| token.as_str());
            let (disk, stamp) = vault.read_bytes(&destination).map_err(|e| e.to_string())?;
            let expected = editors
                .get(&destination)
                .map_or_else(|| (disk == before).then_some(stamp.hash), |editor| Some(editor.known.hash));
            if expected != Some(stamp.hash) {
                return Err("目标文档已变化，请重新粘贴".into());
            }
            let mut doc = aero_core::Document::from_bytes(&before).map_err(|e| e.to_string())?;
            let (document, metadata) = aero_core::identity::ensure(&doc)?;
            on_paste_snapshot();
            let (mut resolver, mut notes) = aero_core::block_refs::vault_snapshot(vault)?;
            let mut disk_versions = HashMap::new();
            for note in &mut notes {
                let Some(editor) = editors.get(&note.path) else { continue };
                let disk = note.before.as_deref().ok_or("事务快照缺少磁盘版本")?;
                if aero_core::hash_bytes(disk) != editor.known.hash {
                    return Err(format!("文件版本冲突：{}", note.path));
                }
                let live = editor.bytes(moving)?;
                if live == disk {
                    continue;
                }
                let overlay = aero_core::Document::from_bytes(&live).map_err(|e| e.to_string())?;
                aero_core::identity::read(&overlay)?;
                let summary = aero_core::links::NoteSummary::from_document(note.path.clone(), &overlay);
                resolver.insert(note.path.clone(), Some(&summary.title), &summary.aliases);
                resolver.register_identity(&note.path, &overlay);
                disk_versions.insert(note.path.clone(), note.before.replace(live.clone()));
                note.after = Some(live);
            }
            let plan = aero_core::clipboard::Relocation::prepare_from_snapshot(
                vault,
                &source,
                &payload,
                &destination,
                &document,
                cut.is_some(),
                (resolver, notes),
            )?;
            match position {
                PastePosition::Text(range) => doc = crate::editor::insert_tree(&doc, range, &plan.source)?,
                PastePosition::AfterBlock { offset, prefix } => {
                    let edit = crate::editor::insert_tree_after(&doc, offset, &prefix, &plan.source);
                    doc.edit(edit).map_err(|e| e.to_string())?;
                }
            }
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
            let mut restorations = HashMap::new();
            if let Some(cut) = cut {
                let source = changes.iter().find(|c| c.path == cut.path).ok_or("剪切来源不在笔记库中")?;
                if source.before != cut.after {
                    return Err("剪切来源已变化，请先撤销源文档中的后续编辑".into());
                }
                let disk = disk_versions.get(&cut.path).unwrap_or(&source.before);
                let recovery = match editors.get(&cut.path) {
                    Some(editor) => editor.projected_bytes()?,
                    None => None,
                }
                .or_else(|| cut.before.clone());
                if recovery.as_ref() != disk.as_ref()
                    && let Some(bytes) = recovery
                {
                    restorations.insert(cut.path.clone(), bytes);
                }
                if let Some(source) = history.iter_mut().find(|c| c.path == cut.path) {
                    source.before = cut.before;
                } else {
                    history.push(cut);
                }
            }
            let touched: HashSet<_> = history.iter().map(|change| &change.path).collect();
            for change in &mut changes {
                let changed = touched.contains(&change.path);
                if changed && editors.get(&change.path).is_some_and(|editor| editor.has_other_cut(moving)) {
                    return Err(format!("{} 还有未完成的剪切，请先撤销或完成该剪切", change.path));
                }
                if let Some(disk) = disk_versions.remove(&change.path) {
                    change.before = disk;
                    if !changed {
                        change.after.clone_from(&change.before);
                    }
                }
            }
            commit_restoring(vault, &journal, changes, control, restorations)?;
            outcome.touched = history.iter().map(|c| c.path.clone()).collect();
            outcome.history.push((token, history, None));
        }
        Operation::CreateChild { parent, before, range, journal } => {
            let (doc, _) = vault.read(&parent).map_err(|e| e.to_string())?;
            if doc.to_bytes() != before {
                return Err("创建位置已变化，请重新执行子文档命令".into());
            }
            let (child, changes) = aero_core::relations::child_at(vault, &parent, &doc, range)?;
            let mut observed: Vec<_> = changes
                .iter()
                .filter(|change| change.before.is_some())
                .map(|change| change.path.clone())
                .collect();
            observed.sort();
            let changes = prepare_transaction(vault, &journal, changes, control, Default::default())?
                .commit_checked(vault, || {
                    let mut notes = vault.notes().map_err(|e| e.to_string())?;
                    let included = notes.binary_search(&child);
                    if let Ok(index) = included {
                        notes.remove(index);
                    } else if vault.exists(&child) {
                        return Err("新建子文档被笔记库忽略规则排除".into());
                    }
                    if notes != observed {
                        return Err("笔记库文件列表已变化，请重新创建子文档".into());
                    }
                    Ok(())
                })?;
            outcome.touched = changes.iter().map(|change| change.path.clone()).collect();
            outcome.history.push((aero_core::identity::new_block_id(), changes, None));
            outcome.open = Some((child, true));
            outcome.show_documents = true;
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
        Operation::SyncReference { request, journal } => {
            return insert_sync(vault, *request, &journal, control);
        }
        Operation::Transaction { token, changes, history_changes, direction, open, journal } => {
            if changes.is_empty() {
                return Ok(outcome);
            }
            let mut disk_changes = changes.clone();
            if direction.is_some() {
                for change in &mut disk_changes {
                    let Some(editor) = editors.get(&change.path) else { continue };
                    if editor.has_other_cut(None)
                        || change.before.as_deref() != Some(editor.bytes(None)?.as_slice())
                    {
                        return Err(format!("{} 的编辑内容已变化，请重试关联操作", change.path));
                    }
                    let (bytes, stamp) = vault.read_bytes(&change.path).map_err(|e| e.to_string())?;
                    if stamp.hash != editor.known.hash {
                        return Err(format!("文件版本冲突：{}", change.path));
                    }
                    change.before = Some(bytes);
                }
            }
            commit(vault, &journal, disk_changes, control)?;
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
            outcome.history.push((
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
        Operation::ImportNoteCopy { source, directory } => {
            let name = source
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| name.to_ascii_lowercase().ends_with(".md") && name.len() > 3)
                .ok_or("请选择 Markdown（.md）笔记")?;
            let bytes = std::fs::read(&source).map_err(|error| format!("{name}：{error}"))?;
            let doc = aero_core::Document::from_bytes(&bytes).map_err(|error| format!("{name}：{error}"))?;
            let canonical = source.canonicalize().map_err(|error| format!("{name}：{error}"))?;
            let origin = VaultPath::from_fs(vault.root(), &canonical)
                .or_else(|_| VaultPath::new(name))
                .map_err(|error| error.to_string())?;
            let mut resolver = aero_core::links::Resolver::new();
            for path in vault.notes().map_err(|error| error.to_string())? {
                let (note, _) = vault.read(&path).map_err(|error| error.to_string())?;
                resolver.insert(
                    path.clone(),
                    Some(&note.title_or(path.stem())),
                    &note.front_matter_aliases(),
                );
                resolver.register_identity(&path, &note);
            }
            let copy = aero_core::clipboard::copy_document(doc, &origin, resolver)?;
            if let Some(control) = control {
                control.begin_commit()?;
            }
            let (path, _) = create(vault, &directory, &name[..name.len() - 3], &copy.to_bytes(), true)?;
            outcome.touched.push(path.clone());
            outcome.open = Some((path, false));
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
        let cut_token = match &operation {
            Operation::Paste { token, cut: Some(_), .. } => Some(token.clone()),
            _ => None,
        };
        let protected = operation.uses_editor_snapshots();
        let editors = Editor::live_editors(vault.id(), cx);
        if let Operation::Transaction { token, direction: Some(redo), changes, .. } = &operation
            && editors.iter().any(|editor| {
                let e = editor.read(cx);
                changes.iter().any(|c| &c.path == e.path())
                    && e.history_transaction(*redo) != Some(token.as_str())
            })
        {
            for editor in &editors {
                editor.update(cx, |e, cx| e.resume_io(cx));
            }
            window.push_notification(Notification::warning("请先撤销参与文档中的后续编辑。"), cx);
            return;
        }
        set_busy(vault.id(), true, cx);
        for editor in &editors {
            if !editor.update(cx, |editor, cx| {
                if protected { editor.suspend_for_transaction(cx) } else { editor.suspend_io(cx) }
            }) {
                for editor in &editors {
                    editor.update(cx, |e, cx| e.resume_io(cx));
                }
                window.push_notification(Notification::warning("笔记仍在保存，请重试文件操作。"), cx);
                set_busy(vault.id(), false, cx);
                operation.cancel(cx);
                return;
            }
        }
        let snapshots = if protected {
            let mut snapshots: HashMap<VaultPath, NoteSnapshot> = HashMap::new();
            for editor in &editors {
                let editor = editor.read(cx);
                let path = editor.path();
                let snapshot = editor.transaction_snapshot();
                if let Some(previous) = snapshots.insert(path.clone(), snapshot)
                    && !previous.agrees_with(&snapshots[path])
                {
                    let error = format!("{} 在多个窗口中有不同的编辑版本", path);
                    for editor in &editors {
                        editor.update(cx, |e, cx| {
                            e.protect_unsaved_transaction_state();
                            e.resume_io(cx);
                        });
                    }
                    set_busy(vault.id(), false, cx);
                    operation.cancel(cx);
                    window.push_notification(Notification::warning(error), cx);
                    self.continue_operation(window, cx);
                    return;
                }
            }
            snapshots
        } else {
            HashMap::new()
        };
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
            let result = worker_diagnostic
                .scope(|| execute_with_paste_snapshot(&vault, operation, Some(&control), &snapshots, || {}));
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
                if let Ok(outcome) = &outcome {
                    for (token, changes, direction) in &outcome.history {
                        super::history::record(open.vault.id(), token, changes, *direction, cx);
                    }
                }
                for editor in &editors {
                    let path = editor.read(cx).path().clone();
                    if outcome.is_err()
                        && let Some(token) = &cut_token
                        && let Some((_, Ok((document, stamp)))) = loaded.iter().find(|(p, _)| p == &path)
                        && editor.update(cx, |e, cx| e.accept_cut_recovery(token, document, *stamp, cx))
                    {
                        continue;
                    }
                    if outcome.is_err()
                        && protected
                        && editor.update(cx, |e, _| e.protect_unsaved_transaction_state())
                    {
                        editor.update(cx, |e, cx| e.resume_io(cx));
                        continue;
                    }
                    if let Ok(outcome) = &outcome
                        && let Some((token, change, direction)) =
                            outcome.history.iter().find_map(|(token, changes, direction)| {
                                let change = changes.iter().find(|c| c.path == path)?;
                                Some((token, change, direction))
                            })
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
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => Outcome { error: Some(error), ..Default::default() },
                };
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
                if let Some((path, created)) = outcome.open {
                    if outcome.show_documents {
                        this.show_sidebar(SidebarTab::Files, cx);
                        if let Some(open) = &this.open {
                            open.tree.update(cx, |tree, cx| tree.set_documents(true, cx));
                        }
                    }
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
    use super::{Operation, PastePosition, execute, finish_on_shutdown};
    use aero_core::{Vault, VaultPath};
    use std::sync::Arc;

    #[test]
    fn paste_preparation_keeps_the_source_on_cancellation_and_version_conflict() {
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
        let source_stamp = vault.create_bytes(&source, &source_before).unwrap();
        vault.create_bytes(&target, b"destination\n").unwrap();
        let token = aero_core::identity::new_block_id();
        let operation = Operation::Paste {
            token: token.clone(),
            payload: Box::new(Payload {
                version: 1,
                vault: vault.id().into(),
                path: source.to_string(),
                document: Some(id),
                cut: Some(token),
            }),
            source: moved,
            destination: target.clone(),
            before: b"destination\n".to_vec(),
            position: PastePosition::Text(11..11),
            cut: Some(FileChange {
                path: source.clone(),
                before: Some(source_before.clone()),
                after: Some(source_cut.clone()),
            }),
            journal: journal.path().into(),
        };
        let snapshot = aero_core::Document::from_bytes(&source_cut).unwrap().snapshot();
        let editors = std::collections::HashMap::from([(
            source.clone(),
            super::NoteSnapshot {
                known: source_stamp,
                text: snapshot,
                bom: false,
                ending: aero_core::LineEnding::Lf,
                pending_cut: None,
            },
        )]);
        let control = super::Control::default();
        let cancelled =
            super::execute_with_paste_snapshot(&vault, operation.clone(), Some(&control), &editors, || {
                control.cancel()
            });
        assert!(matches!(cancelled, Err(message) if message.contains("操作已取消")));
        assert_eq!(vault.read_bytes(&target).unwrap().0, b"destination\n");
        assert_eq!(vault.read_bytes(&source).unwrap().0, source_before);

        let result = super::execute_with_paste_snapshot(&vault, operation, None, &editors, || {
            vault.write_bytes(&target, b"external newer text\n").unwrap();
        });
        assert!(
            result.is_err(),
            "a later scan must not legitimize overwriting a newer destination with the initial editor snapshot"
        );
        assert_eq!(vault.read_bytes(&target).unwrap().0, b"external newer text\n");
        assert_eq!(
            vault.read_bytes(&source).unwrap().0,
            source_before,
            "the move must leave its uncut source on disk throughout planning"
        );
        assert_eq!(std::fs::read_dir(journal.path()).unwrap().count(), 0);
    }

    #[test]
    fn child_excluded_from_the_tree_rolls_back_parent_and_sibling_changes() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        std::fs::write(dir.path().join(".aero/ignore"), "未命名.md\n").unwrap();
        let parent = VaultPath::new("parent.md").unwrap();
        let sibling = VaultPath::new("sibling.md").unwrap();
        let document = aero_core::identity::new_document_id();
        let parent_bytes = format!("---\naero: {{id: {document}}}\n---\n\n/child\n").into_bytes();
        let sibling_bytes = format!("---\naero: {{parent: {document}}}\n---\n\nold sibling\n");
        vault.create_bytes(&parent, &parent_bytes).unwrap();
        vault.create_bytes(&sibling, sibling_bytes.as_bytes()).unwrap();
        let start = parent_bytes.len() - "/child\n".len();
        let result = execute(
            &vault,
            Operation::CreateChild {
                parent: parent.clone(),
                before: parent_bytes.clone(),
                range: start..start + 6,
                journal: dir.path().join(".aero/transactions"),
            },
        );
        assert!(matches!(result, Err(error) if error.contains("忽略规则")));
        assert_eq!(vault.read_bytes(&parent).unwrap().0, parent_bytes);
        assert_eq!(vault.read_bytes(&sibling).unwrap().0, sibling_bytes.as_bytes());
        assert!(!vault.exists(&VaultPath::new("未命名.md").unwrap()));
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

    #[test]
    fn sync_rechecks_the_vault_before_claiming_an_identity() {
        use aero_core::{Vault, VaultPath, identity};
        let dir = tempfile::tempdir().unwrap();
        let journal = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let source = VaultPath::new("source.md").unwrap();
        let destination = VaultPath::new("destination.md").unwrap();
        let collision = VaultPath::new("collision.md").unwrap();
        let document = identity::new_document_id();
        let other_document = identity::new_document_id();
        let primary = identity::new_block_id();
        let alias = identity::new_block_id();
        let other = identity::new_block_id();
        let existing =
            format!("---\naero: {{id: {document}}}\n---\n\nsource {{aero-aliases=\"{alias}\"}} ^{primary}\n");
        let legacy = format!("---\naero: {{id: {document}}}\n---\n\nsource ^old\n");
        let target = b"before /sync after\n";
        vault.create_bytes(&destination, target).unwrap();
        for (body, conflicting, expected) in [
            (&existing, format!("---\naero: {{id: {document}}}\n---\n\nother\n"), "文档身份重复"),
            (&existing, format!("other ^{primary}\n"), "块身份或别名重复"),
            (&existing, format!("other ^{alias}\n"), "块身份或别名重复"),
            (&existing, format!("other {{aero-aliases=\"{primary}\"}} ^{other}\n"), "块身份或别名重复"),
            (&legacy, format!("other {{aero-aliases=\"{document}#old\"}} ^{other}\n"), "块身份或别名重复"),
        ] {
            vault.write_bytes(&source, body.as_bytes()).unwrap();
            vault.write_bytes(&collision, conflicting.as_bytes()).unwrap();
            let request = super::SyncReference {
                source: source.clone(),
                source_before: body.as_bytes().to_vec(),
                block: 1,
                destination: destination.clone(),
                destination_before: target.to_vec(),
                edit: super::SyncEdit::Insert(7..12),
            };
            let result = super::insert_sync(&vault, request, journal.path(), None);
            assert!(matches!(result, Err(error) if error.contains(expected)), "{conflicting}");
            assert_eq!(vault.read_bytes(&source).unwrap().0, body.as_bytes());
            assert_eq!(vault.read_bytes(&destination).unwrap().0, target);
        }

        vault
            .write_bytes(
                &collision,
                format!("---\naero: {{id: {other_document}}}\n---\n\nother ^old\n").as_bytes(),
            )
            .unwrap();
        let request = super::SyncReference {
            source: source.clone(),
            source_before: legacy.as_bytes().to_vec(),
            block: 1,
            destination: destination.clone(),
            destination_before: target.to_vec(),
            edit: super::SyncEdit::Insert(7..12),
        };
        let outcome = super::insert_sync(&vault, request, journal.path(), None).unwrap();
        assert_eq!(outcome.history.len(), 2);
        assert!(vault.read(&source).unwrap().0.to_string().contains(&format!("{document}#old")));
    }
}
