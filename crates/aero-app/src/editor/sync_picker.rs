//! Source selection captures a destination revision and insertion range. Picking
//! a source is read-only; confirmation submits one recoverable workspace edit.
use super::*;
use crate::actions::ConfirmSyncSource;
use crate::workspace::files::{Operation, SyncEdit, SyncReference};
use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::Input;

pub(super) struct SyncPicker {
    generation: String,
    revision: u64,
    range: Range<usize>,
    replace: bool,
    input: Entity<InputState>,
    _subscription: Subscription,
    notes: Vec<aero_core::links::NoteSummary>,
    source: Option<(VaultPath, Document)>,
    candidates: Vec<aero_core::block_refs::Candidate>,
    matches: Vec<usize>,
    selected: usize,
    error: Option<String>,
    loading: bool,
}

impl Editor {
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
            revision: self.doc.version(),
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
        if menu.revision != self.doc.version() {
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
            let overlays = self.identity_overlays(cx);
            let vault = self.vault.clone();
            if let Some(menu) = &mut self.sync_picker {
                menu.loading = true;
                menu.error = None;
            }
            cx.spawn_in(window, async move |this, cx| {
                let doc = cx
                    .background_spawn(async move {
                        let (resolver, doc) = super::live_sources::identity_source(&vault, &path, overlays)?;
                        let document = aero_core::identity::read(&doc)?.id;
                        if let Some(id) = &document {
                            resolver.check_document_identity(&path, id)?;
                        }
                        let candidates = aero_core::block_refs::candidates_matching(&doc, |block| {
                            resolver.check_block_identity(&path, document.as_deref(), block).is_ok()
                        });
                        Ok::<_, String>((path, doc, candidates))
                    })
                    .await;
                let _ = this.update_in(cx, |this, window, cx| {
                    let Some(menu) = &mut this.sync_picker else { return };
                    if menu.generation != generation {
                        return;
                    }
                    menu.loading = false;
                    match doc {
                        Ok((path, doc, candidates)) => {
                            if candidates.is_empty() {
                                menu.error = Some("文档中没有身份唯一的可引用块".into());
                            }
                            menu.candidates = candidates;
                            menu.source = Some((path, doc));
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
        let Some((path, source)) = &menu.source else { return };
        let candidate = &menu.candidates[index];
        let Some(block) = source.blocks().iter().position(|(_, block)| block.id == candidate.block) else {
            return;
        };
        let edit = if menu.replace {
            SyncEdit::Replace(menu.range.clone())
        } else {
            SyncEdit::Insert(menu.range.clone())
        };
        let request = SyncReference {
            source: path.clone(),
            source_before: source.to_bytes(),
            block,
            destination: self.path.clone(),
            destination_before: self.doc.to_bytes(),
            edit,
        };
        self.sync_picker = None;
        cx.emit(EditorEvent::TransactionRequested(Operation::SyncReference {
            request: Box::new(request),
            journal: crate::paths::app_data_dir().join("transactions").join(self.vault.id()),
        }));
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    pub(super) fn copy_sync_for(&mut self, id: BlockId, window: &mut Window, cx: &mut Context<Self>) {
        self.copy_identity_for(id, super::toolbar::IdentityCopy::Sync, window, cx);
        self.block_menu = None;
        cx.notify();
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
                        .key_context("SyncPicker")
                        .on_action(cx.listener(|this, _: &ConfirmSyncSource, window, cx| {
                            this.confirm_sync(None, window, cx);
                        }))
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
