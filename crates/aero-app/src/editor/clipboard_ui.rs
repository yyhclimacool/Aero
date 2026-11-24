use super::*;
use aero_core::transaction::FileChange;
use gpui_kit::component::WindowExt as _;

#[derive(Default)]
struct Cuts(std::collections::HashMap<String, Cut>);
impl Global for Cuts {}
struct Cut {
    editor: WeakEntity<Editor>,
    vault: String,
    change: FileChange,
    pending: bool,
    blocked: Option<String>,
}

impl Editor {
    pub(super) fn copy_blocks(&self, text: String, cx: &mut Context<Self>) {
        let payload = aero_core::clipboard::Payload {
            version: 1,
            vault: self.vault.id().into(),
            path: self.path.to_string(),
            document: aero_core::identity::read(&self.doc).ok().and_then(|m| m.id),
            cut: None,
        };
        cx.write_to_clipboard(ClipboardItem::new_string_with_json_metadata(text, payload));
    }

    pub(super) fn cut_blocks(&mut self, range: Range<usize>, window: &mut Window, cx: &mut Context<Self>) {
        if self.io.suspended {
            return;
        }
        let (document, mut edits) = match aero_core::identity::ensure(&self.doc) {
            Ok(result) => result,
            Err(error) => {
                window.push_notification(gpui_kit::component::notification::Notification::error(error), cx);
                return;
            }
        };
        let token = aero_core::identity::new_block_id();
        let before = self.doc.to_bytes();
        let source = aero_core::containers::standalone(&self.doc, range.clone());
        let at = range.start;
        let selection = self.selection;
        let delete = selection
            .filter(|_| self.selected_node.is_none())
            .map(|s| ops::delete_units(&self.doc, &self.units, s.first(), s.last()))
            .unwrap_or_else(|| Edit::delete(range));
        edits.insert(0, delete);
        if let Err(error) = self.doc.edit_transaction(edits, &token) {
            window.push_notification(
                gpui_kit::component::notification::Notification::error(error.to_string()),
                cx,
            );
            return;
        }
        let cut = Cut {
            editor: cx.weak_entity(),
            vault: self.vault.id().into(),
            pending: true,
            blocked: None,
            change: FileChange {
                path: self.path.clone(),
                before: Some(before),
                after: Some(self.doc.to_bytes()),
            },
        };
        if !cx.has_global::<Cuts>() {
            cx.set_global(Cuts::default());
        }
        cx.update_global::<Cuts, _>(|cuts, _| {
            cuts.0.insert(token.clone(), cut);
        });
        cx.write_to_clipboard(ClipboardItem::new_string_with_json_metadata(
            source,
            aero_core::clipboard::Payload {
                version: 1,
                vault: self.vault.id().into(),
                path: self.path.to_string(),
                document: Some(document),
                cut: Some(token),
            },
        ));
        self.focused = None;
        self.selection = None;
        self.selected_node = None;
        self.last_edit_at = None;
        self.after_doc_change(cx);
        if let Some(s) = selection
            && !self.units.is_empty()
        {
            let ix = s.first().min(self.units.len() - 1);
            self.select_blocks(ix, ix, window, cx);
        } else {
            self.focus_at(at.min(self.doc.len()), None, window, cx);
        }
    }

    pub(super) fn paste_blocks(
        &mut self,
        item: &ClipboardItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(payload) = item
            .metadata()
            .and_then(|s| serde_json::from_str::<aero_core::clipboard::Payload>(s).ok())
            .filter(|p| p.version == 1)
        else {
            return false;
        };
        let Some(source) = item.text() else { return false };
        if self.io.suspended {
            return true;
        }
        self.ensure_current_parse();
        if let Some(error) = payload
            .cut
            .as_ref()
            .and_then(|t| cx.try_global::<Cuts>()?.0.get(t))
            .filter(|cut| cut.vault == self.vault.id())
            .and_then(|cut| cut.blocked.clone())
        {
            window.push_notification(gpui_kit::component::notification::Notification::error(error), cx);
            return true;
        }
        let cut = payload
            .cut
            .as_ref()
            .and_then(|token| cx.try_global::<Cuts>()?.0.get(token))
            .filter(|cut| cut.pending && cut.vault == self.vault.id())
            .map(|cut| (cut.editor.clone(), cut.change.clone()));
        let cut = if let Some((weak, mut change)) = cut {
            let token = payload.cut.as_deref().unwrap();
            let restore = |doc: &Document| {
                doc.without_transaction(token).map(|restored| (restored.to_bytes(), doc.to_bytes()))
            };
            let restored = if weak.entity_id() == cx.entity_id() {
                restore(&self.doc)
            } else {
                weak.upgrade()
                    .ok_or_else(|| "剪切来源已关闭，请重新剪切".to_owned())
                    .and_then(|editor| restore(&editor.read(cx).doc))
            };
            match restored {
                Ok((before, after)) => {
                    change.before = Some(before);
                    change.after = Some(after);
                    Some(change)
                }
                Err(error) => {
                    window.push_notification(
                        gpui_kit::component::notification::Notification::warning(error),
                        cx,
                    );
                    return true;
                }
            }
        } else {
            None
        };
        let range = if let Some(id) = self.selected_node {
            aero_core::properties::owned_range(&self.doc, id).unwrap_or(self.doc.len()..self.doc.len())
        } else if let Some(s) = self.selection {
            ops::units_span(&self.doc, &self.units, s.first(), s.last())
        } else if let Some(f) = &self.focused {
            let r = f.textarea.read(cx).selected_range();
            f.range.start + r.start..f.range.start + r.end
        } else {
            self.doc.len()..self.doc.len()
        };
        let token =
            if cut.is_some() { payload.cut.clone().unwrap() } else { aero_core::identity::new_block_id() };
        if let Some(change) = &cut {
            cx.update_global::<Cuts, _>(|cuts, _| {
                if let Some(cut) = cuts.0.get_mut(&token) {
                    cut.pending = false;
                    cut.change = change.clone();
                }
            });
        }
        cx.emit(EditorEvent::TransactionRequested(crate::workspace::files::Operation::Paste {
            token,
            payload: Box::new(payload),
            source,
            destination: self.path.clone(),
            before: self.doc.to_bytes(),
            range,
            cut,
            journal: crate::paths::app_data_dir().join("transactions").join(self.vault.id()),
        }));
        true
    }

    pub(crate) fn complete_cut(token: &str, success: bool, cx: &mut App) {
        let cut = if cx.has_global::<Cuts>() {
            cx.update_global::<Cuts, _>(|cuts, _| cuts.0.remove(token))
        } else {
            None
        };
        if !success && let Some(mut cut) = cut {
            let restored =
                cut.editor.upgrade().ok_or_else(|| "剪切来源已关闭，恢复未完成".to_owned()).and_then(
                    |editor| {
                        editor.update(cx, |e, cx| {
                            if Some(e.doc.to_bytes()) == cut.change.before {
                                return Ok(());
                            }
                            let restored = e.doc.without_transaction(token)?;
                            e.doc = restored;
                            e.doc.take_edits();
                            e.refresh_units();
                            e.update_local_link_names();
                            e.focused = None;
                            e.selection = None;
                            e.selected_node = None;
                            e.after_doc_change(cx);
                            Ok::<_, String>(())
                        })
                    },
                );
            if let Err(error) = restored {
                cut.blocked = Some(format!("剪切恢复存在冲突：{error}。移动令牌已冻结，请先处理源文档。"));
                cx.update_global::<Cuts, _>(|cuts, _| {
                    cuts.0.insert(token.into(), cut);
                });
            }
        }
    }

    pub(crate) fn accept_cut_recovery(
        &mut self,
        token: &str,
        loaded: &Document,
        stamp: FileStamp,
        cx: &mut Context<Self>,
    ) -> bool {
        let Ok(restored) = self.doc.without_transaction(token) else { return false };
        if restored.to_bytes() != loaded.to_bytes() {
            return false;
        }
        self.doc = restored;
        self.doc.take_edits();
        self.refresh_units();
        self.update_local_link_names();
        self.focused = None;
        self.selection = None;
        self.selected_node = None;
        self.after_doc_change(cx);
        self.save_task = None;
        self.dirty = false;
        self.save_state = SaveState::Clean;
        self.stamp = stamp;
        self.resume_io(cx);
        true
    }
}
