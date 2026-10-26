//! Source selection captures a destination revision and insertion range. Picking
//! a source is read-only; confirmation submits one recoverable workspace edit.
use super::*;
use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::Input;
use std::collections::{BTreeMap, BTreeSet};

use crate::workspace::files::SyncRevision as LiveRevision;

struct LiveSnapshot {
    revision: LiveRevision,
    text: aero_core::Snapshot,
    bom: bool,
    line_ending: aero_core::text::LineEnding,
    hash: u64,
    dirty: bool,
}

impl LiveSnapshot {
    fn bytes(&self, disk: &[u8], hash: u64) -> Result<Vec<u8>, String> {
        let text = self.text.text().to_string();
        let encoded = aero_core::text::SourceText::encode(&text, self.bom, self.line_ending);
        if hash != self.hash && disk != encoded {
            return Err(format!("笔记已变化：{}", self.revision.path));
        }
        if !self.dirty && aero_core::text::SourceText::decode(disk).map_err(|e| e.to_string())?.text == text {
            Ok(disk.to_vec())
        } else {
            Ok(encoded)
        }
    }
}

struct SyncSource {
    path: VaultPath,
    doc: Document,
    bytes: Vec<u8>,
    disk_hash: u64,
    live: Option<LiveRevision>,
}

fn load_source(vault: &Vault, path: VaultPath, overlay: Option<LiveSnapshot>) -> Result<SyncSource, String> {
    let (disk, stamp) = vault.read_bytes(&path).map_err(|e| e.to_string())?;
    let bytes = overlay.as_ref().map_or_else(|| Ok(disk.clone()), |live| live.bytes(&disk, stamp.hash))?;
    let doc = Document::from_bytes(&bytes).map_err(|e| e.to_string())?;
    Ok(SyncSource { path, doc, bytes, disk_hash: stamp.hash, live: overlay.map(|live| live.revision) })
}

pub(super) struct SyncPicker {
    generation: String,
    destination: LiveRevision,
    range: Range<usize>,
    replace: bool,
    input: Entity<InputState>,
    _subscription: Subscription,
    notes: Vec<aero_core::links::NoteSummary>,
    source: Option<Arc<SyncSource>>,
    candidates: Vec<aero_core::block_refs::Candidate>,
    matches: Vec<usize>,
    selected: usize,
    error: Option<String>,
    loading: bool,
}

impl Editor {
    pub(crate) fn sync_revision(&self, entity: EntityId) -> LiveRevision {
        LiveRevision { entity, path: self.path.clone(), version: self.doc.version(), epoch: self.io.epoch }
    }

    #[cfg(test)]
    pub(crate) fn test_sync_revise(&mut self, cx: &mut Context<Self>) {
        self.doc.edit(Edit::insert(0, "later")).unwrap();
        self.doc.undo().unwrap();
        self.after_doc_change(cx);
    }

    #[cfg(test)]
    pub(crate) fn test_sync_picker(
        &mut self,
        source: Option<&VaultPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = source {
            let menu = self.sync_picker.as_mut().unwrap();
            assert!(!menu.loading, "{:?}", menu.error);
            if menu.source.is_none() {
                menu.matches = vec![menu.notes.iter().position(|note| &note.path == path).unwrap()];
            }
            self.confirm_sync(Some(0), window, cx);
        } else {
            let at = self.doc.to_string().find("placeholder").unwrap();
            self.open_sync_picker(at..at + "placeholder".len(), true, window, cx);
        }
    }

    fn sync_snapshot(&self, entity: EntityId) -> LiveSnapshot {
        LiveSnapshot {
            revision: self.sync_revision(entity),
            text: self.doc.snapshot(),
            bom: self.doc.bom,
            line_ending: self.doc.line_ending,
            hash: self.stamp.hash,
            dirty: self.dirty,
        }
    }

    fn sync_revisions(&self, cx: &Context<Self>) -> BTreeMap<EntityId, LiveRevision> {
        let mut revisions = BTreeMap::new();
        revisions.insert(cx.entity_id(), self.sync_revision(cx.entity_id()));
        for editor in Self::live_editors(self.vault.id(), cx) {
            if editor.entity_id() != cx.entity_id() {
                revisions.insert(editor.entity_id(), editor.read(cx).sync_revision(editor.entity_id()));
            }
        }
        revisions
    }

    pub(super) fn open_sync_picker(
        &mut self,
        range: Range<usize>,
        replace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("选择来源文档"));
        let subscription = cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
            InputEvent::Change => this.filter_sync(cx),
            InputEvent::PressEnter { .. } => this.confirm_sync(None, window, cx),
            _ => {}
        });
        let generation = aero_core::identity::new_block_id();
        self.sync_picker = Some(SyncPicker {
            generation: generation.clone(),
            destination: self.sync_revision(cx.entity_id()),
            range,
            replace,
            input: input.clone(),
            _subscription: subscription,
            notes: Vec::new(),
            source: None,
            candidates: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            error: None,
            loading: true,
        });
        self.block_menu = None;
        self.slash = None;
        input.update(cx, |input, cx| input.focus(window, cx));
        let directory = self.link_preview.catalog.clone();
        let vault = self.vault.clone();
        cx.spawn(async move |this, cx| {
            let notes = cx
                .background_spawn(async move {
                    if let Some(directory) = directory {
                        return directory.snapshot().map(|s| s.notes.as_ref().clone());
                    }
                    vault
                        .notes()
                        .map_err(|e| e.to_string())?
                        .into_iter()
                        .map(|path| {
                            let (doc, _) = vault.read(&path).map_err(|e| e.to_string())?;
                            Ok(aero_core::links::NoteSummary::from_document(path, &doc))
                        })
                        .collect::<Result<Vec<_>, String>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let Some(menu) = &mut this.sync_picker else { return };
                if menu.generation != generation {
                    return;
                }
                menu.loading = false;
                match notes {
                    Ok(notes) => menu.notes = notes,
                    Err(error) => menu.error = Some(error),
                }
                this.filter_sync(cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn filter_sync(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.sync_picker else { return };
        let query = menu.input.read(cx).value().to_lowercase();
        menu.matches = if menu.source.is_some() {
            menu.candidates
                .iter()
                .enumerate()
                .filter(|(_, b)| query.is_empty() || b.excerpt.to_lowercase().contains(query.as_str()))
                .map(|(i, _)| i)
                .collect()
        } else {
            menu.notes
                .iter()
                .enumerate()
                .filter(|(_, n)| {
                    query.is_empty()
                        || n.title.to_lowercase().contains(query.as_str())
                        || n.path.as_str().to_lowercase().contains(query.as_str())
                })
                .map(|(i, _)| i)
                .collect()
        };
        menu.selected = 0;
        cx.notify();
    }

    fn confirm_sync(&mut self, row: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = &self.sync_picker else { return };
        if menu.loading {
            return;
        }
        if menu.destination != self.sync_revision(cx.entity_id()) {
            window.push_notification(
                gpui_kit::component::notification::Notification::warning("插入位置已变化，请重新选择来源"),
                cx,
            );
            self.sync_picker = None;
            return;
        }
        let Some(&index) = menu.matches.get(row.unwrap_or(menu.selected)) else { return };
        if menu.source.is_none() {
            let generation = menu.generation.clone();
            let path = menu.notes[index].path.clone();
            let overlay = if path == self.path {
                Some(self.sync_snapshot(cx.entity_id()))
            } else {
                Self::live_editors(self.vault.id(), cx)
                    .into_iter()
                    .filter(|e| e.entity_id() != cx.entity_id())
                    .find_map(|entity| {
                        let e = entity.read(cx);
                        (e.path == path).then(|| e.sync_snapshot(entity.entity_id()))
                    })
            };
            let vault = self.vault.clone();
            self.sync_picker.as_mut().unwrap().loading = true;
            cx.notify();
            cx.spawn_in(window, async move |this, cx| {
                let doc = cx
                    .background_spawn(async move {
                        let source = load_source(&vault, path, overlay)?;
                        let candidates = aero_core::block_refs::candidates(&source.doc);
                        Ok::<_, String>((Arc::new(source), candidates))
                    })
                    .await;
                let _ = this.update_in(cx, |this, window, cx| {
                    let revisions = this.sync_revisions(cx);
                    let Some(menu) = &mut this.sync_picker else { return };
                    if menu.generation != generation {
                        return;
                    }
                    menu.loading = false;
                    match doc {
                        Ok((source, candidates)) => {
                            if revisions.get(&menu.destination.entity) != Some(&menu.destination)
                                || !source.is_current(&revisions)
                            {
                                menu.error = Some("笔记已变化，请重新选择来源".into());
                                cx.notify();
                                return;
                            }
                            menu.candidates = candidates;
                            menu.source = Some(source);
                            menu.input.update(cx, |input, cx| {
                                input.set_value("", window, cx);
                                input.set_placeholder("选择来源块", window, cx);
                            });
                        }
                        Err(error) => menu.error = Some(error),
                    }
                    this.filter_sync(cx);
                });
            })
            .detach();
            return;
        }
        let generation = menu.generation.clone();
        let source = menu.source.as_ref().unwrap().clone();
        let block = menu.candidates[index].block;
        let range = menu.range.clone();
        let replace = menu.replace;
        let revisions = self.sync_revisions(cx);
        if !source.is_current(&revisions) {
            self.sync_picker.as_mut().unwrap().error = Some("来源笔记已变化，请重新选择来源".into());
            cx.notify();
            return;
        }
        let mut live = vec![self.sync_snapshot(cx.entity_id())];
        for editor in Self::live_editors(self.vault.id(), cx) {
            if editor.entity_id() != cx.entity_id() {
                live.push(editor.read(cx).sync_snapshot(editor.entity_id()));
            }
        }
        let destination = self.path.clone();
        let vault = self.vault.clone();
        let menu = self.sync_picker.as_mut().unwrap();
        menu.loading = true;
        menu.error = None;
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let plan = plan_sync(&vault, &source, block, &destination, range, replace, live)?;
                    plan.validate(&vault)?;
                    Ok::<_, String>(plan)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.sync_picker.as_ref().is_some_and(|menu| menu.generation == generation) {
                    return;
                }
                let result = if this.sync_revisions(cx) == revisions {
                    result
                } else {
                    Err("笔记已变化，请重新选择来源".into())
                };
                match result {
                    Ok(plan) => {
                        this.sync_picker = None;
                        cx.emit(EditorEvent::TransactionRequested(
                            crate::workspace::files::Operation::SyncReference {
                                token: aero_core::identity::new_block_id(),
                                changes: plan.changes,
                                history_changes: plan.history_changes,
                                membership: plan.membership,
                                revisions,
                                journal: crate::paths::app_data_dir()
                                    .join("transactions")
                                    .join(this.vault.id()),
                            },
                        ));
                        this.focus_handle.focus(window, cx);
                    }
                    Err(error) => {
                        let menu = this.sync_picker.as_mut().unwrap();
                        menu.loading = false;
                        menu.error = Some(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn copy_sync_for(&mut self, id: BlockId, window: &mut Window, cx: &mut Context<Self>) {
        match self.ensure_id_for(id, window, cx) {
            Ok(id) => {
                if let Ok(meta) = aero_core::identity::read(&self.doc)
                    && let Some(document) = meta.id
                {
                    let label = aero_core::links::readable_alias(&self.title());
                    self.copy_blocks(format!("![[id:{document}#^{id}|{label}]]"), cx);
                }
            }
            Err(error) => {
                window.push_notification(gpui_kit::component::notification::Notification::error(error), cx)
            }
        }
        self.block_menu = None;
    }

    pub(super) fn render_sync_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.sync_picker.as_ref()?;
        let theme = cx.theme().clone();
        let rows = menu.matches.iter().take(80).enumerate().map(|(row, &index)| {
            let label = if menu.source.is_some() {
                menu.candidates[index].excerpt.clone()
            } else {
                format!("{} · {}", menu.notes[index].title, menu.notes[index].path)
            };
            div()
                .id(("sync-source", row))
                .px_2()
                .py_1()
                .text_sm()
                .rounded(px(4.))
                .when(row == menu.selected, |el| el.bg(theme.list_active))
                .hover(|el| el.bg(theme.list_hover))
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| this.confirm_sync(Some(row), window, cx)),
                )
        });
        Some(
            deferred(
                anchored().position(point(px(260.), px(100.))).snap_to_window_with_margin(px(8.)).child(
                    v_flex()
                        .id("sync-picker")
                        .debug_selector(|| "sync-picker".into())
                        .occlude()
                        .w(px(420.))
                        .p_2()
                        .gap_2()
                        .bg(theme.popover)
                        .text_color(theme.popover_foreground)
                        .border_1()
                        .border_color(theme.border)
                        .rounded(px(8.))
                        .shadow_lg()
                        .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            match event.keystroke.key.as_str() {
                                "up" | "down" => {
                                    if let Some(menu) = &mut this.sync_picker
                                        && !menu.matches.is_empty()
                                    {
                                        let delta = if event.keystroke.key == "up" { -1 } else { 1 };
                                        menu.selected = (menu.selected as isize + delta)
                                            .rem_euclid(menu.matches.len() as isize)
                                            as usize;
                                    }
                                    cx.stop_propagation();
                                    cx.notify();
                                }
                                "escape" => {
                                    this.sync_picker = None;
                                    this.focus_handle.focus(window, cx);
                                    cx.stop_propagation();
                                    cx.notify();
                                }
                                _ => {}
                            }
                        }))
                        .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                            this.sync_picker = None;
                            this.focus_handle.focus(window, cx);
                            cx.notify();
                        }))
                        .child(if menu.source.is_some() { "选择来源块" } else { "选择来源文档" })
                        .child(Input::new(&menu.input))
                        .when(menu.loading, |el| el.child("正在读取…"))
                        .when_some(menu.error.clone(), |el, error| el.child(error))
                        .child(
                            v_flex().id("sync-candidates").max_h(px(320.)).overflow_y_scroll().children(rows),
                        ),
                ),
            )
            .into_any_element(),
        )
    }
}

impl SyncSource {
    fn is_current(&self, revisions: &BTreeMap<EntityId, LiveRevision>) -> bool {
        let mut owners = revisions.values().filter(|live| live.path == self.path);
        match &self.live {
            Some(expected) => owners.next() == Some(expected) && owners.next().is_none(),
            None => owners.next().is_none(),
        }
    }
}

struct SyncPlan {
    changes: Vec<aero_core::transaction::FileChange>,
    history_changes: Option<Vec<aero_core::transaction::FileChange>>,
    membership: BTreeSet<VaultPath>,
    disk_hashes: BTreeMap<VaultPath, u64>,
}

impl SyncPlan {
    fn validate(&self, vault: &Vault) -> Result<(), String> {
        if vault.notes().map_err(|e| e.to_string())?.into_iter().collect::<BTreeSet<_>>() != self.membership {
            return Err("笔记列表已变化，请重新选择来源".into());
        }
        for (path, hash) in &self.disk_hashes {
            if vault.read_bytes(path).map_err(|e| e.to_string())?.1.hash != *hash {
                return Err(format!("笔记已变化：{path}"));
            }
        }
        Ok(())
    }
}

fn plan_sync(
    vault: &Vault,
    selected: &SyncSource,
    block: BlockId,
    destination_path: &VaultPath,
    mut range: Range<usize>,
    replace: bool,
    live: Vec<LiveSnapshot>,
) -> Result<SyncPlan, String> {
    let membership: BTreeSet<_> = vault.notes().map_err(|e| e.to_string())?.into_iter().collect();
    let mut snapshots = BTreeMap::new();
    let mut disk_hashes = BTreeMap::new();
    for path in &membership {
        let (bytes, stamp) = vault.read_bytes(path).map_err(|e| e.to_string())?;
        snapshots.insert(path.clone(), bytes);
        disk_hashes.insert(path.clone(), stamp.hash);
    }
    if selected.live.is_none() && disk_hashes.get(&selected.path) != Some(&selected.disk_hash) {
        return Err("来源笔记已变化，请重新选择来源".into());
    }
    let mut live_paths = BTreeSet::new();
    for live in live {
        let path = &live.revision.path;
        if !live_paths.insert(path.clone()) {
            return Err(format!("笔记存在多个编辑版本：{path}"));
        }
        let disk = snapshots.get(path).ok_or_else(|| format!("笔记已不在库中：{path}"))?;
        let bytes = live.bytes(disk, disk_hashes[path])?;
        snapshots.insert(path.clone(), bytes);
    }
    if snapshots.get(&selected.path) != Some(&selected.bytes) {
        return Err("来源笔记已变化，请重新选择来源".into());
    }
    let mut resolver = aero_core::links::Resolver::new();
    for (path, bytes) in &snapshots {
        let doc = Document::from_bytes(bytes).map_err(|e| e.to_string())?;
        aero_core::identity::read(&doc)?;
        resolver.register_identity(path, &doc);
    }
    let mut source = selected.doc.clone();
    let assignment = aero_core::block_refs::permanent_in(&source, block, &selected.path, &resolver)?;
    let mut source_bytes = selected.bytes.clone();
    for edit in &assignment.edits {
        source_bytes = aero_core::block_refs::edit_bytes(&source_bytes, edit)?;
        if selected.path == *destination_path && edit.range.end <= range.start {
            range.start = (range.start as isize + edit.delta()) as usize;
            range.end = (range.end as isize + edit.delta()) as usize;
        }
    }
    source.edit_group(assignment.edits).map_err(|e| e.to_string())?;
    source.reparse();
    let label = aero_core::links::readable_alias(&source.title_or(selected.path.stem()));
    let reference = format!("![[id:{}#^{}|{label}]]", assignment.document, assignment.id);
    let before = snapshots.get(destination_path).ok_or("插入笔记已不在库中")?.clone();
    let mut destination = if selected.path == *destination_path {
        source
    } else {
        Document::from_bytes(&before).map_err(|e| e.to_string())?
    };
    if replace {
        destination.edit(Edit::replace(range, reference)).map_err(|e| e.to_string())?;
    } else {
        destination = insert_tree(&destination, range, &reference)?;
    }
    let destination_change = aero_core::transaction::FileChange {
        path: destination_path.clone(),
        before: Some(before),
        after: Some(destination.to_bytes()),
    };
    // Source identity must not join destination undo.
    let history_changes = (selected.path != *destination_path).then(|| vec![destination_change.clone()]);
    let changes = snapshots
        .into_iter()
        .map(|(path, bytes)| {
            if path == *destination_path {
                destination_change.clone()
            } else {
                let after = if path == selected.path { source_bytes.clone() } else { bytes.clone() };
                aero_core::transaction::FileChange { path, before: Some(bytes), after: Some(after) }
            }
        })
        .collect();
    Ok(SyncPlan { changes, history_changes, membership, disk_hashes })
}

pub(crate) fn insert_tree(doc: &Document, range: Range<usize>, source: &str) -> Result<Document, String> {
    let mut doc = doc.clone();
    let at = range.start;
    doc.edit(Edit::delete(range)).map_err(|e| e.to_string())?;
    doc.reparse();
    let units = units::units(&doc);
    let at = if let Some(unit) = units.iter().find(|u| u.range.start <= at && at <= u.range.end) {
        if !unit.is_text() {
            return Err("请在正文或块间插入同步引用".into());
        }
        let plan = ops::split(&doc, Some(unit), &unit.range, at - unit.range.start);
        let at = plan.focus_offset;
        doc.edit(plan.edit).map_err(|e| e.to_string())?;
        at
    } else {
        if at > 0 && !doc.slice(0..at).to_string().ends_with("\n\n") {
            doc.edit(Edit::insert(at, "\n\n")).map_err(|e| e.to_string())?;
            at + 2
        } else {
            at
        }
    };
    doc.edit(Edit::insert(at, format!("{source}\n\n"))).map_err(|e| e.to_string())?;
    doc.reparse();
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::Editor;
    use crate::workspace::files::Operation;
    use aero_core::{Document, Vault, VaultPath, transaction::FileChange};
    use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext};
    use std::sync::{Arc, Mutex};

    fn pick(
        editor: &Entity<Editor>,
        path: VaultPath,
        replace: bool,
        cx: &mut VisualTestContext,
    ) -> Option<Operation> {
        pick_after(editor, path, replace, cx, |_, _, _| {})
    }

    fn pick_after(
        editor: &Entity<Editor>,
        path: VaultPath,
        replace: bool,
        cx: &mut VisualTestContext,
        after: impl FnOnce(&mut Editor, &mut gpui_kit::Window, &mut gpui_kit::Context<Editor>),
    ) -> Option<Operation> {
        pick_around(editor, path, replace, cx, |_, _, _| {}, after)
    }

    fn pick_around(
        editor: &Entity<Editor>,
        path: VaultPath,
        replace: bool,
        cx: &mut VisualTestContext,
        before: impl FnOnce(&mut Editor, &mut gpui_kit::Window, &mut gpui_kit::Context<Editor>),
        after: impl FnOnce(&mut Editor, &mut gpui_kit::Window, &mut gpui_kit::Context<Editor>),
    ) -> Option<Operation> {
        let observed = Arc::new(Mutex::new(None));
        let _subscription = cx.update(|_, cx| {
            let observed = observed.clone();
            cx.subscribe(editor, move |_, event, _| {
                if let super::EditorEvent::TransactionRequested(operation) = event {
                    *observed.lock().unwrap() = Some(operation.clone());
                }
            })
        });
        cx.update(|window, cx| {
            window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(editor.clone(), window, cx));
            editor.update(cx, |e, cx| {
                let at = e.doc.to_string().find("placeholder").unwrap();
                e.open_sync_picker(at..at + "placeholder".len(), replace, window, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                let menu = e.sync_picker.as_mut().unwrap();
                let index = menu.notes.iter().position(|note| note.path == path).unwrap();
                menu.matches = vec![index];
                e.confirm_sync(Some(0), window, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                before(e, window, cx);
                e.confirm_sync(Some(0), window, cx);
                after(e, window, cx);
            });
        });
        cx.run_until_parked();
        observed.lock().unwrap().take()
    }

    #[gpui_kit::test]
    fn g08_copied_references_escape_readable_title_without_changing_address(cx: &mut TestAppContext) {
        let title = "开发 [记录] | C:\\notes **x**";
        let document = aero_core::identity::new_document_id();
        let block = aero_core::identity::new_block_id();
        let original = format!("---\ntitle: '{title}'\naero:\n  id: {document}\n---\n\nbody ^{block}\n");
        let (_dir, vault, path, editor) = super::super::tests::open(cx, &original);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| editor.update(cx, |e, cx| {
            let id = aero_core::block_refs::candidates(&e.doc)[0].block;
            for sync in [false, true] {
                if sync { e.copy_sync_for(id, window, cx); } else { e.copy_block_link_for(id, window, cx); }
                let copied = cx.read_from_clipboard().unwrap().text().unwrap();
                let (target, fragment, alias) = if sync {
                    let spans = aero_core::links::scan_embeds(&copied);
                    assert_eq!(spans.len(), 1, "{copied}");
                    (spans[0].target.clone(), spans[0].fragment.clone(), spans[0].parameter.clone())
                } else {
                    let spans = aero_core::links::scan(&copied);
                    assert_eq!(spans.len(), 1, "{copied}");
                    (spans[0].target.clone(), spans[0].fragment.clone(), spans[0].alias.clone())
                };
                assert_eq!(target, format!("id:{document}"));
                assert_eq!(fragment, Some(format!("^{block}")));
                assert_eq!(alias.as_deref(), Some(title));
                assert!(copied.contains("\\[记录\\]"), "{copied}");
                assert!(!copied.contains("body"));
            }
            assert_eq!(e.doc.to_string(), original);
        }));
        assert_eq!(vault.read_bytes(&path).unwrap().0, original.as_bytes());
    }

    #[test]
    fn g08_sync_plan_adds_readable_alias_without_content_copy() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let source = VaultPath::new("source.md").unwrap();
        let destination = VaultPath::new("destination.md").unwrap();
        vault.write_bytes(&source, b"---\ntitle: Source title\n---\n\nunique source body\n").unwrap();
        vault.write_bytes(&destination, b"placeholder\n").unwrap();
        let selected = super::load_source(&vault, source.clone(), None).unwrap();
        let block = aero_core::block_refs::candidates(&selected.doc)[0].block;
        let plan = super::plan_sync(&vault, &selected, block, &destination, 0..11, true, vec![]).unwrap();
        let text = String::from_utf8(plan.changes.iter().find(|c| c.path == destination).unwrap().after.clone().unwrap()).unwrap();
        let spans = aero_core::links::scan_embeds(&text);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].parameter.as_deref(), Some("Source title"));
        assert!(!text.contains("unique source body"));
        assert_eq!(vault.read_bytes(&destination).unwrap().0, b"placeholder\n");
    }

    #[gpui_kit::test]
    fn g07_source_metadata_completion_publishes_parsed_identity(cx: &mut TestAppContext) {
        let id = aero_core::identity::new_document_id();
        let text = format!("---\naero:\n  id: {id}\n---\n\nfirst\n\n{}\n", "large ".repeat(45_000));
        let (_dir, vault, path, editor) = super::super::tests::open(cx, &text);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| editor.update(cx, |e, cx| e.test_focus_unit(0, window, cx)));
        cx.update(|_, cx| {
            editor.update(cx, |e, cx| {
                let before = vault.read_bytes(&path).unwrap().0;
                let candidate = aero_core::block_refs::candidates(&e.doc).pop().unwrap();
                let mut resolver = aero_core::links::Resolver::new();
                resolver.register_identity(&path, &e.doc);
                let assignment =
                    aero_core::block_refs::permanent_in(&e.doc, candidate.block, &path, &resolver).unwrap();
                let mut after = before.clone();
                for edit in assignment.edits {
                    after = aero_core::block_refs::edit_bytes(&after, &edit).unwrap();
                }
                assert!(e.suspend_io(cx));
                let stamp = vault.write_bytes(&path, &after).unwrap();
                e.finish_source_metadata(
                    &FileChange { path: path.clone(), before: Some(before), after: Some(after) },
                    Some(stamp),
                    cx,
                )
                .unwrap();
                assert!(
                    !e.doc.is_parse_stale(),
                    "completion must publish the new identity, not an old parse"
                );
                assert_eq!(
                    aero_core::block_refs::candidates(&e.doc).last().unwrap().persisted_id.as_deref(),
                    Some(assignment.id.as_str())
                );
            })
        });
    }

    #[gpui_kit::test]
    fn g07_source_metadata_conflict_retains_buffer_and_history(cx: &mut TestAppContext) {
        let (_dir, vault, path, editor) = super::super::tests::open(cx, "source\n");
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|_, cx| {
            editor.update(cx, |e, cx| {
                e.doc.edit(aero_core::Edit::insert(0, "ordinary ")).unwrap();
                e.after_doc_change(cx);
                e.flush_sync().unwrap();
                let before = e.doc.to_bytes();
                let id = aero_core::identity::new_document_id();
                let after = format!("---\naero:\n  id: {id}\n---\n\nordinary source\n").into_bytes();
                let stamp = vault.write_bytes(&path, &after).unwrap();
                for raw_conflict in [false, true] {
                    let change = FileChange {
                        path: path.clone(),
                        before: Some(if raw_conflict {
                            b"ordinary source\r\n".to_vec()
                        } else {
                            b"stale source\n".to_vec()
                        }),
                        after: Some(after.clone()),
                    };
                    assert!(e.finish_source_metadata(&change, Some(stamp), cx).is_err());
                    assert_eq!(e.doc.to_bytes(), before);
                    assert!(e.io.suspended);
                }
                let wrong_stamp = vault.write_bytes(&path, b"external\n").unwrap();
                assert!(
                    e.finish_source_metadata(
                        &FileChange { path: path.clone(), before: Some(before.clone()), after: Some(after) },
                        Some(wrong_stamp),
                        cx
                    )
                    .is_err()
                );
                assert_eq!(e.doc.to_bytes(), before);
                e.doc.undo().unwrap();
                e.after_doc_change(cx);
                assert_eq!(e.doc.to_string(), "source\n");
            })
        });
        cx.run_until_parked();
        assert_eq!(vault.read_bytes(&path).unwrap().0, b"external\n");
    }

    #[gpui_kit::test]
    fn sync_confirm_is_nonblocking_and_cancel_discards_pending_plan(cx: &mut TestAppContext) {
        let (_dir, vault, destination, editor) = super::super::tests::open(cx, "placeholder\n");
        let path = VaultPath::new("source.md").unwrap();
        vault.write_bytes(&path, b"source\n").unwrap();
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let operation = pick_after(&editor, path.clone(), true, cx, |e, _, _| {
            assert!(
                e.sync_picker.as_ref().is_some_and(|menu| menu.loading),
                "confirmation must leave a cancellable loading menu"
            );
            e.sync_picker = None;
        });
        assert!(operation.is_none());
        assert_eq!(vault.read_bytes(&path).unwrap().0, b"source\n");
        assert_eq!(vault.read_bytes(&destination).unwrap().0, b"placeholder\n");
    }

    #[gpui_kit::test]
    fn sync_confirm_discards_stale_destination_or_menu(cx: &mut TestAppContext) {
        let (_dir, vault, _, editor) = super::super::tests::open(cx, "placeholder\n");
        let path = VaultPath::new("source.md").unwrap();
        vault.write_bytes(&path, b"source\n").unwrap();
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        for change_document in [false, true] {
            let operation = pick_after(&editor, path.clone(), true, cx, |e, window, cx| {
                if change_document {
                    e.doc.edit(aero_core::Edit::insert(e.doc.len(), "new\n")).unwrap();
                } else {
                    e.open_sync_picker(0..0, true, window, cx);
                }
            });
            assert!(operation.is_none(), "stale confirmation must not emit");
        }
        assert_eq!(vault.read_bytes(&path).unwrap().0, b"source\n");
    }

    #[gpui_kit::test]
    fn sync_confirm_preserves_unchanged_mixed_endings_source(cx: &mut TestAppContext) {
        let (_dir, vault, _, editor) = super::super::tests::open(cx, "placeholder\n");
        let path = VaultPath::new("source.md").unwrap();
        let document = aero_core::identity::new_document_id();
        let block = aero_core::identity::new_block_id();
        let bytes = format!("\u{feff}---\r\naero:\n  id: {document}\r\n---\n\nsource ^{block}\n\nlast\r\n")
            .into_bytes();
        vault.write_bytes(&path, &bytes).unwrap();
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let operation = pick_after(&editor, path.clone(), true, cx, |_, _, _| {})
            .expect("mixed endings are not a source conflict");
        let Operation::SyncReference { ref changes, .. } = operation else { unreachable!() };
        let source = changes.iter().find(|change| change.path == path).unwrap();
        assert_eq!(source.before.as_ref(), Some(&bytes));
        assert_eq!(source.after.as_ref(), Some(&bytes));
        apply(&vault, operation);
        assert_eq!(vault.read_bytes(&path).unwrap().0, bytes);
    }

    #[gpui_kit::test]
    fn sync_confirm_never_overrides_selected_source_with_new_live_revision(cx: &mut TestAppContext) {
        let (_dir, vault, _, editor) = super::super::tests::open(cx, "placeholder\n");
        let path = VaultPath::new("source.md").unwrap();
        vault.write_bytes(&path, b"source\n").unwrap();
        let (doc, stamp) = vault.read(&path).unwrap();
        let source = cx.new(|cx| Editor::new(vault.clone(), path.clone(), doc, stamp, cx));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        for during in [false, true] {
            let change = |cx: &mut gpui_kit::Context<Editor>| {
                source.update(cx, |e, cx| {
                    e.doc.edit(aero_core::Edit::insert(e.doc.len(), "new\n")).unwrap();
                    e.doc.reparse();
                    e.dirty = true;
                    e.publish_live(cx);
                });
            };
            let operation = pick_around(
                &editor,
                path.clone(),
                true,
                cx,
                |_, _, cx| {
                    if !during {
                        change(cx);
                    }
                },
                |_, _, cx| {
                    if during {
                        change(cx);
                    }
                },
            );
            assert!(operation.is_none(), "source revision changed, during={during}");
        }
        assert_eq!(vault.read_bytes(&path).unwrap().0, b"source\n");
    }

    #[gpui_kit::test]
    fn sync_confirm_rejects_disk_source_changed_since_selection(cx: &mut TestAppContext) {
        let (_dir, vault, _, editor) = super::super::tests::open(cx, "placeholder\n");
        let path = VaultPath::new("source.md").unwrap();
        vault.write_bytes(&path, b"source\n").unwrap();
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let operation = pick_around(
            &editor,
            path.clone(),
            true,
            cx,
            |_, _, _| {
                vault.write_bytes(&path, b"new source\n").unwrap();
            },
            |_, _, _| {},
        );
        assert!(operation.is_none());
        assert_eq!(vault.read_bytes(&path).unwrap().0, b"new source\n");
    }

    #[gpui_kit::test]
    fn sync_confirm_identity_edits_keep_unedited_source_bytes(cx: &mut TestAppContext) {
        let (_dir, vault, _, editor) = super::super::tests::open(cx, "placeholder\n");
        let path = VaultPath::new("source.md").unwrap();
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        for body in ["source", "source ^old"] {
            let original = format!("\u{feff}{body}\n\nlast\r\n").into_bytes();
            vault.write_bytes(&path, &original).unwrap();
            let operation = pick_after(&editor, path.clone(), true, cx, |_, _, _| {}).unwrap();
            let Operation::SyncReference { ref changes, ref history_changes, .. } = operation else {
                unreachable!()
            };
            let source = changes.iter().find(|c| c.path == path).unwrap();
            assert_eq!(source.before.as_ref(), Some(&original));
            let after = source.after.as_ref().unwrap();
            assert!(after.starts_with(b"\xef\xbb\xbf"));
            assert!(after.ends_with(b"\n\nlast\r\n"), "unedited endings must survive: {after:?}");
            let metadata_only: Vec<_> = changes
                .iter()
                .filter(|c| {
                    c.before != c.after && !history_changes.as_ref().unwrap().iter().any(|h| h.path == c.path)
                })
                .collect();
            assert_eq!(metadata_only.len(), 1);
            assert_eq!(metadata_only[0].path, path);
            assert_eq!(vault.read_bytes(&path).unwrap().0, original);
        }
    }

    #[test]
    fn sync_plan_submission_rejects_changed_managed_note_membership() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let source = VaultPath::new("source.md").unwrap();
        let destination = VaultPath::new("destination.md").unwrap();
        vault.write_bytes(&source, b"source\n").unwrap();
        vault.write_bytes(&destination, b"placeholder\n").unwrap();
        let selected = super::load_source(&vault, source.clone(), None).unwrap();
        let block = aero_core::block_refs::candidates(&selected.doc)[0].block;
        let plan = super::plan_sync(&vault, &selected, block, &destination, 0..11, true, vec![]).unwrap();
        assert!(plan.validate(&vault).is_ok());
        let added = VaultPath::new("added.md").unwrap();
        vault.write_bytes(&added, b"new owner\n").unwrap();
        assert!(plan.validate(&vault).unwrap_err().contains("列表"));
        std::fs::remove_file(vault.fs_path(&added)).unwrap();
        std::fs::rename(vault.fs_path(&source), vault.fs_path(&added)).unwrap();
        assert!(plan.validate(&vault).unwrap_err().contains("列表"));
        assert_eq!(vault.read_bytes(&destination).unwrap().0, b"placeholder\n");
    }

    fn apply(vault: &Vault, mut operation: Operation) {
        let journal = tempfile::tempdir().unwrap();
        match &mut operation {
            Operation::Transaction { journal: path, .. } | Operation::SyncReference { journal: path, .. } => {
                *path = journal.path().into();
            }
            _ => panic!("expected transaction"),
        }
        crate::workspace::files::execute_test(vault, operation).unwrap();
    }

    #[gpui_kit::test]
    fn cross_document_sync_undo_keeps_new_source_identity(cx: &mut TestAppContext) {
        let (_dir, vault, destination, editor) = super::super::tests::open(cx, "placeholder\n");
        let path = VaultPath::new("source.md").unwrap();
        vault.write_bytes(&path, b"source\n").unwrap();
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let operation = pick(&editor, path.clone(), false, cx).expect("sync request");
        let Operation::SyncReference { ref changes, ref history_changes, .. } = operation else {
            unreachable!()
        };
        // This is the same fallback used by the workspace history consumer.
        let history = history_changes
            .clone()
            .unwrap_or_else(|| changes.iter().filter(|c| c.before != c.after).cloned().collect());
        apply(&vault, operation);
        let assigned = vault.read_bytes(&path).unwrap().0;
        let source = Document::from_bytes(&assigned).unwrap();
        let document = aero_core::identity::read(&source).unwrap().id.unwrap();
        let block = aero_core::block_refs::candidates(&source)[0].persisted_id.clone().unwrap();
        assert!(aero_core::identity::is_block_id(&block));
        let inserted = aero_core::links::scan_embeds(&vault.read(&destination).unwrap().0.to_string());
        assert_eq!(inserted[0].target, format!("id:{document}"));
        assert_eq!(inserted[0].fragment, Some(format!("^{block}")));
        assert_eq!(inserted[0].parameter.as_deref(), Some("source"));
        apply(
            &vault,
            Operation::Transaction {
                token: "undo-sync".into(),
                changes: history.iter().map(FileChange::reverse).collect(),
                history_changes: None,
                direction: Some(false),
                open: None,
                journal: Default::default(),
            },
        );
        assert_eq!(vault.read_bytes(&destination).unwrap().0, b"placeholder\n");
        assert_eq!(vault.read_bytes(&path).unwrap().0, assigned, "undo must leave source identity");
        assert_eq!(history.len(), 1);
        apply(
            &vault,
            Operation::Transaction {
                token: "redo-sync".into(),
                changes: history,
                history_changes: None,
                direction: Some(true),
                open: None,
                journal: Default::default(),
            },
        );
        assert_eq!(vault.read_bytes(&path).unwrap().0, assigned);
    }

    #[gpui_kit::test]
    fn same_document_sync_undo_groups_identity_and_insertion(cx: &mut TestAppContext) {
        let original = "source\n\nplaceholder\n";
        let (_dir, vault, path, editor) = super::super::tests::open(cx, original);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let operation = pick(&editor, path.clone(), false, cx).expect("sync request");
        let Operation::SyncReference { ref changes, ref history_changes, .. } = operation else {
            unreachable!()
        };
        assert!(history_changes.is_none(), "same document keeps grouped history");
        let history: Vec<_> = changes.iter().filter(|c| c.before != c.after).cloned().collect();
        apply(&vault, operation);
        assert!(vault.read(&path).unwrap().0.to_string().contains("![[id:"));
        apply(
            &vault,
            Operation::Transaction {
                token: "undo-sync".into(),
                changes: history.iter().map(FileChange::reverse).collect(),
                history_changes: None,
                direction: Some(false),
                open: None,
                journal: Default::default(),
            },
        );
        assert_eq!(vault.read_bytes(&path).unwrap().0, original.as_bytes());
    }

    #[gpui_kit::test]
    fn sync_picker_validates_unchanged_source_and_catalog_observations(cx: &mut TestAppContext) {
        let (_dir, vault, destination, editor) = super::super::tests::open(cx, "placeholder\n");
        let path = VaultPath::new("source.md").unwrap();
        let other = VaultPath::new("other.md").unwrap();
        let document = aero_core::identity::new_document_id();
        let block = aero_core::identity::new_block_id();
        let original = format!("---\naero:\n  id: {document}\n---\n\nsource ^{block}\n");
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        for changed in [&path, &other] {
            vault.write_bytes(&path, original.as_bytes()).unwrap();
            vault.write_bytes(&other, b"other\n").unwrap();
            let mut operation = pick(&editor, path.clone(), true, cx).unwrap();
            let Operation::SyncReference { journal: path, .. } = &mut operation else { unreachable!() };
            let journal = tempfile::tempdir().unwrap();
            *path = journal.path().into();
            vault.write_bytes(changed, b"external change\n").unwrap();
            let result = crate::workspace::files::execute_test(&vault, operation);
            assert!(result.is_err(), "must validate observed {changed}");
            assert_eq!(vault.read_bytes(&destination).unwrap().0, b"placeholder\n");
        }
    }

    #[gpui_kit::test]
    fn sync_picker_checks_live_identity_snapshots_not_only_disk(cx: &mut TestAppContext) {
        let (_dir, vault, _, editor) = super::super::tests::open(cx, "placeholder\n");
        let path = VaultPath::new("source.md").unwrap();
        let other = VaultPath::new("other.md").unwrap();
        let document = aero_core::identity::new_document_id();
        let source = format!("---\naero:\n  id: {document}\n---\n\nsource\n");
        vault.write_bytes(&path, source.as_bytes()).unwrap();
        vault.write_bytes(&other, b"other\n").unwrap();
        let (doc, stamp) = vault.read(&other).unwrap();
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let _other_editor = cx.update(|_, cx| {
            let other = cx.new(|cx| Editor::new(vault.clone(), other.clone(), doc, stamp, cx));
            other.update(cx, |e, cx| {
                e.doc.edit(aero_core::Edit::replace(0..e.doc.len(), source)).unwrap();
                e.doc.reparse();
                e.publish_live(cx);
            });
            other
        });
        assert!(pick(&editor, path.clone(), true, cx).is_none());
        assert_eq!(vault.read_bytes(&other).unwrap().0, b"other\n", "live source was not saved");
    }

    #[gpui_kit::test]
    fn sync_picker_rejects_vault_wide_duplicate_source_identities(cx: &mut TestAppContext) {
        let (_dir, vault, _, editor) = super::super::tests::open(cx, "placeholder\n");
        let path = VaultPath::new("source.md").unwrap();
        let other = VaultPath::new("other.md").unwrap();
        let document = aero_core::identity::new_document_id();
        let other_document = aero_core::identity::new_document_id();
        let block = aero_core::identity::new_block_id();
        let other_block = aero_core::identity::new_block_id();
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        for (name, source_body, other_id, other_body) in [
            ("document", "source".to_owned(), document.clone(), "other".to_owned()),
            ("block", format!("source ^{block}"), other_document.clone(), format!("other ^{block}")),
            (
                "block alias",
                format!("source ^{block}"),
                other_document.clone(),
                format!("other {{aero-aliases=\"{block}\"}} ^{other_block}"),
            ),
            (
                "legacy alias",
                "source ^old".to_owned(),
                other_document.clone(),
                format!("other {{aero-aliases=\"{document}#old\"}} ^{other_block}"),
            ),
            (
                "source alias",
                format!("source {{aero-aliases=\"{other_block}\"}} ^{block}"),
                other_document.clone(),
                format!("other ^{other_block}"),
            ),
        ] {
            let source_text = format!("---\naero:\n  id: {document}\n---\n\n{source_body}\n");
            vault.write_bytes(&path, source_text.as_bytes()).unwrap();
            vault
                .write_bytes(
                    &other,
                    format!("---\naero:\n  id: {other_id}\n---\n\n{other_body}\n").as_bytes(),
                )
                .unwrap();
            let operation = pick(&editor, path.clone(), true, cx);
            assert!(operation.is_none(), "must reject duplicated {name}");
            assert_eq!(vault.read_bytes(&path).unwrap().0, source_text.as_bytes());
        }
    }
}
