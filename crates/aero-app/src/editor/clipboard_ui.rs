use super::*;
use aero_core::transaction::FileChange;
use gpui_kit::component::WindowExt as _;

#[derive(Default)]
struct Cuts(std::collections::HashMap<String, Cut>);
impl Global for Cuts {}
#[derive(Clone)]
struct Cut {
    editor: WeakEntity<Editor>,
    vault: String,
    change: FileChange,
    pending: bool,
    cancelling: bool,
    blocked: Option<String>,
    save: io::CutSave,
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
        let id = self
            .doc
            .blocks()
            .iter()
            .find(|(_, b)| aero_core::properties::owned_range(&self.doc, b.id).as_ref() == Some(&range))
            .map(|(_, b)| b.id);
        if let Some(id) = id {
            self.cut_selection(ops::SelectionPlan::nodes(&self.doc, &[id]), window, cx);
        } else {
            let source = aero_core::containers::standalone(&self.doc, range.clone());
            self.cut_content(source, range.start, vec![Edit::delete(range)], window, cx);
        }
    }

    pub(super) fn cut_selection(
        &mut self,
        plan: ops::SelectionPlan,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(at) = plan.start() else { return };
        self.cut_content(plan.source(&self.doc), at, plan.deletions(&self.doc), window, cx);
    }

    fn cut_content(
        &mut self,
        source: String,
        at: usize,
        delete: Vec<Edit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.io.suspended {
            return;
        }
        let Some(root) = self.io.journal_root.clone() else {
            window.push_notification(
                gpui_kit::component::notification::Notification::error("剪切恢复目录尚未初始化"),
                cx,
            );
            return;
        };
        if self.io.cut.is_some() {
            window.push_notification(
                gpui_kit::component::notification::Notification::warning("请先粘贴或撤销上一次剪切"),
                cx,
            );
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
        let selection = self.selection;
        edits.splice(0..0, delete);
        if let Err(error) = self.doc.edit_transaction(edits, &token) {
            window.push_notification(
                gpui_kit::component::notification::Notification::error(error.to_string()),
                cx,
            );
            return;
        }
        let journal = aero_core::transaction::PendingCut::unpublished(root, self.path.clone(), token.clone());
        let save = io::CutSave {
            token: token.clone(),
            vault: self.vault.clone(),
            journal: std::sync::Arc::new(std::sync::Mutex::new(journal)),
        };
        self.io.cut = Some(save.clone());
        let cut = Cut {
            save,
            editor: cx.weak_entity(),
            vault: self.vault.id().into(),
            pending: true,
            cancelling: false,
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
        if let Some(token) = payload.cut.as_deref()
            && cx
                .try_global::<Cuts>()
                .and_then(|cuts| cuts.0.get(token))
                .is_some_and(|cut| cut.editor.upgrade().is_none())
        {
            Self::complete_cut(token, false, cx);
        }
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
        let selection = self.selection_plan();
        let range = if let Some(plan) = &selection {
            let at = plan.start().unwrap_or(self.doc.len());
            at..at
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
        let pending_cut = cut
            .as_ref()
            .and_then(|_| cx.try_global::<Cuts>()?.0.get(&token))
            .map(|cut| cut.save.journal.clone());
        cx.emit(EditorEvent::TransactionRequested(crate::workspace::files::Operation::Paste {
            token,
            pending_cut,
            payload,
            source,
            destination: self.path.clone(),
            before: self.doc.to_bytes(),
            range,
            selection,
            cut,
            journal: self.io.journal_root.clone().unwrap_or_default(),
        }));
        true
    }

    pub(crate) fn settle_cuts(vault: &str, keep: Option<&str>, cx: &mut App) -> Result<(), String> {
        let tokens = cx
            .try_global::<Cuts>()
            .map(|cuts| {
                cuts.0
                    .iter()
                    .filter(|(token, cut)| cut.vault == vault && Some(token.as_str()) != keep)
                    .map(|(token, _)| token.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for token in tokens {
            if let Some(error) = cx
                .try_global::<Cuts>()
                .and_then(|cuts| cuts.0.get(&token))
                .filter(|cut| !cut.cancelling)
                .and_then(|cut| cut.blocked.clone())
            {
                return Err(error);
            }
            Self::complete_cut(&token, false, cx);
            if let Some(cut) = cx.try_global::<Cuts>().and_then(|cuts| cuts.0.get(&token))
                && !(cut.cancelling && cut.editor.upgrade().is_some())
            {
                return Err(cut.blocked.clone().unwrap_or_else(|| "剪切恢复尚未完成".into()));
            }
        }
        Ok(())
    }

    pub(crate) fn complete_cut(token: &str, success: bool, cx: &mut App) {
        let Some(mut cut) = cx.try_global::<Cuts>().and_then(|cuts| cuts.0.get(token)).cloned() else {
            return;
        };
        if cut.cancelling {
            return;
        }
        if success {
            if let Some(editor) = cut.editor.upgrade() {
                editor.update(cx, |e, _| e.io.cut = None);
            }
            cx.update_global::<Cuts, _>(|cuts, _| {
                cuts.0.remove(token);
            });
            return;
        }
        let snapshot = cut.editor.upgrade().map(|editor| {
            editor.update(cx, |e, cx| {
                let restored = if Some(e.doc.to_bytes()) == cut.change.before
                    || e.doc.history_transaction(true) == Some(token)
                {
                    Ok(e.doc.clone())
                } else {
                    e.doc.without_transaction(token)
                };
                e.save_task = None;
                e.io.checking = None;
                e.io.check_generation += 1;
                e.io.epoch += 1;
                e.io.admitting = false;
                e.io.admitted = None;
                e.io.suspended = true;
                e.io.writing = true;
                e.save_state = SaveState::Saving;
                for presenter in e.presenters.values() {
                    presenter.textarea.update(cx, |s, cx| s.set_disabled(true, cx));
                }
                (restored, e.stamp, e.doc.version(), e.io.epoch, e.io.pending.take())
            })
        });
        cut.cancelling = true;
        cut.blocked = Some("剪切恢复尚未完成".into());
        cx.update_global::<Cuts, _>(|cuts, _| {
            cuts.0.insert(token.into(), cut.clone());
        });
        let save = cut.save.clone();
        let work = snapshot.clone();
        let path = cut.change.path.clone();
        let epoch = snapshot.as_ref().map_or(0, |s| s.3);
        let job = io::WriteJob::spawn(path.clone(), epoch, 0, move || {
            let result = (|| {
                let mut known = work.as_ref().map(|s| s.1);
                if let Some(previous) = work.as_ref().and_then(|s| s.4.as_ref())
                    && let Ok(Some(stamp)) = previous.wait().result
                {
                    known = Some(stamp);
                }
                let mut journal = save.journal.lock().map_err(|_| "剪切恢复锁已损坏".to_owned())?;
                let result = (|| {
                    if let Some((restored, _, _, _, _)) = &work {
                        let restored = restored.as_ref().map_err(Clone::clone)?;
                        let bytes = restored.to_bytes();
                        let stamp = if journal.is_pending() {
                            let change = aero_core::transaction::validate_write(
                                &save.vault,
                                &path,
                                known.unwrap(),
                                bytes.clone(),
                            )?;
                            journal.restore(&save.vault, change, restored.version())?
                        } else {
                            journal.recover_cancel(&save.vault)?;
                            save.vault.read_bytes(&path).map_err(|e| e.to_string())?.1
                        };
                        if save.vault.read_bytes(&path).map_err(|e| e.to_string())?.0 != bytes {
                            return Err("磁盘恢复内容与当前编辑不一致".into());
                        }
                        Ok(Some(stamp))
                    } else {
                        journal.recover_cancel(&save.vault).map(|_| None)
                    }
                })();
                if result.is_err() {
                    journal.freeze()?;
                }
                result
            })();
            io::SaveOutcome { result, conflict: None }
        });
        if let Some(editor) = cut.editor.upgrade() {
            editor.update(cx, |e, _| e.io.pending = Some(job.clone()));
        }
        let wait = cx.background_spawn(async move { job.wait() });
        let token = token.to_owned();
        cx.spawn(async move |cx| {
            let outcome = wait.await;
            let _ = cx.update(|cx| {
                let result = outcome.result.and_then(|stamp| {
                    if let Some(editor) = cut.editor.upgrade()
                        && let Some((restored, _, version, epoch, _)) = snapshot
                    {
                        editor.update(cx, |e, cx| {
                            if e.io.epoch != epoch {
                                return Err("剪切来源版本已变化".into());
                            }
                            let persisted = restored?;
                            let restored = if e.doc.version() == version {
                                persisted.clone()
                            } else if e.doc.history_transaction(true) == Some(token.as_str()) {
                                e.doc.clone()
                            } else {
                                e.doc.without_transaction(&token)?
                            };
                            e.doc = restored;
                            e.doc.take_edits();
                            e.refresh_units();
                            e.update_local_link_names();
                            e.focused = None;
                            e.selection = None;
                            e.selected_node = None;
                            e.io.cut = None;
                            e.io.pending = None;
                            e.io.writing = false;
                            e.after_doc_change(cx);
                            e.save_task = None;
                            e.dirty = e.doc.to_bytes() != persisted.to_bytes();
                            e.save_state = if e.dirty { SaveState::Dirty } else { SaveState::Clean };
                            if let Some(stamp) = stamp {
                                e.stamp = stamp;
                            }
                            e.resume_io(cx);
                            Ok::<_, String>(())
                        })?;
                    }
                    Ok::<_, String>(())
                });
                if let Err(error) = result {
                    let message = format!("剪切恢复存在冲突：{error}。移动令牌已冻结，请先处理源文档。");
                    cut.cancelling = false;
                    cut.blocked = Some(message.clone());
                    if let Some(editor) = cut.editor.upgrade() {
                        editor.update(cx, |e, cx| {
                            e.io.pending = None;
                            e.io.writing = false;
                            e.save_failed(message, cx);
                        });
                    }
                    cx.update_global::<Cuts, _>(|cuts, _| {
                        cuts.0.insert(token, cut);
                    });
                } else {
                    cx.update_global::<Cuts, _>(|cuts, _| {
                        cuts.0.remove(&token);
                    });
                }
            });
        })
        .detach();
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

#[cfg(test)]
mod tests {
    use super::{Editor, EditorEvent};
    use aero_core::{Edit, Vault, VaultPath};

    #[gpui_kit::test]
    fn g11_cut_returns_before_pending_write_and_journal_initialization(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let journal = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "move\n\nkeep\n").unwrap();
        let vault = std::sync::Arc::new(Vault::open(dir.path()).unwrap());
        let path = VaultPath::new("a.md").unwrap();
        let (doc, stamp) = vault.read(&path).unwrap();
        let (editor, cx) = cx.add_window_view(|_, cx| {
            let mut e = Editor::new(vault, path, doc, stamp, cx);
            e.set_journal_root(journal.path().into());
            e
        });
        let (release, gate) = std::sync::mpsc::channel();
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                let done = finished.clone();
                e.io.pending = Some(super::io::WriteJob::spawn(
                    e.path.clone(),
                    e.io.epoch,
                    e.doc.version(),
                    move || {
                        let _ = gate.recv_timeout(std::time::Duration::from_secs(1));
                        done.store(true, std::sync::atomic::Ordering::SeqCst);
                        super::io::SaveOutcome { result: Ok(Some(stamp)), conflict: None }
                    },
                ));
                e.cut_blocks(0..6, window, cx);
            })
        });
        let blocked = finished.load(std::sync::atomic::Ordering::SeqCst);
        let _ = release.send(());
        assert!(!blocked, "cut must not wait for an owned write under the UI lock");
        assert_eq!(std::fs::read_dir(journal.path()).unwrap().count(), 0);
        editor.update(cx, |e, _| {
            e.io.pending.take().unwrap().wait();
            e.flush_sync().unwrap();
        });
        assert_eq!(std::fs::read_dir(journal.path()).unwrap().count(), 1);
    }

    #[gpui_kit::test]
    fn g11_cancel_persists_later_edits_before_retry_becomes_copy(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let journal = tempfile::tempdir().unwrap();
        let block = aero_core::identity::new_block_id();
        let moved = format!("move ^{block}\n\n");
        let original = format!("{moved}keep\n");
        std::fs::write(dir.path().join("a.md"), &original).unwrap();
        let vault = std::sync::Arc::new(Vault::open(dir.path()).unwrap());
        let path = VaultPath::new("a.md").unwrap();
        let (doc, stamp) = vault.read(&path).unwrap();
        let (editor, cx) = cx.add_window_view(|_, cx| {
            let mut e = Editor::new(vault.clone(), path, doc, stamp, cx);
            e.set_journal_root(journal.path().into());
            e
        });
        let (release, gate) = std::sync::mpsc::channel();
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (token, item) = cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                e.cut_blocks(0..moved.len(), window, cx);
                e.flush_sync().unwrap();
                let token = e.io.cut.as_ref().unwrap().token.clone();
                e.doc.edit(Edit::insert(e.doc.len(), "later\n")).unwrap();
                e.after_doc_change(cx);
                let done = finished.clone();
                let stamp = e.stamp;
                e.io.pending = Some(super::io::WriteJob::spawn(e.path.clone(), e.io.epoch, 0, move || {
                    let _ = gate.recv_timeout(std::time::Duration::from_secs(1));
                    done.store(true, std::sync::atomic::Ordering::SeqCst);
                    super::io::SaveOutcome { result: Ok(Some(stamp)), conflict: None }
                }));
                (token, cx.read_from_clipboard().unwrap())
            })
        });
        cx.update(|_, cx| Editor::complete_cut(&token, false, cx));
        let blocked = finished.load(std::sync::atomic::Ordering::SeqCst);
        let _ = release.send(());
        assert!(!blocked, "cancel must await earlier writes in its worker, not on the UI thread");
        assert!(
            editor.read_with(cx, |e, _| e.io.cut.is_some()),
            "token remains owned until worker restoration completes"
        );
        cx.run_until_parked();
        assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), format!("{original}later\n"));
        assert_eq!(std::fs::read_dir(journal.path()).unwrap().count(), 0);
        assert!(editor.read_with(cx, |e, _| e.io.cut.is_none()));
        let copy = std::rc::Rc::new(std::cell::Cell::new(false));
        let _sub = cx.update(|_, cx| {
            let copy = copy.clone();
            let cancelled = token.clone();
            cx.subscribe(&editor, move |_, event, _| {
                if let EditorEvent::TransactionRequested(crate::workspace::files::Operation::Paste {
                    token,
                    source,
                    payload,
                    cut,
                    pending_cut,
                    ..
                }) = event
                {
                    copy.set(cut.is_none() && pending_cut.is_none());
                    assert_ne!(token, &cancelled);
                    let copied = aero_core::clipboard::copy_tree(
                        source,
                        payload,
                        &aero_core::identity::new_document_id(),
                    )
                    .unwrap();
                    let doc = aero_core::Document::from_text(&copied);
                    assert_ne!(doc.blocks().roots[0].persisted_id.as_deref(), Some(block.as_str()));
                    assert!(doc.blocks().roots[0].persisted_id.is_some());
                }
            })
        });
        cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                assert!(e.paste_blocks(&item, window, cx));
            })
        });
        cx.run_until_parked();
        assert!(copy.get(), "cancelled move token must retry as a copy");
        editor.update(cx, |e, _| {
            e.doc.undo();
            assert_eq!(e.doc.to_string(), original);
        });
    }

    #[gpui_kit::test]
    fn g11_undo_reserved_cut_defers_cancellation_without_reborrowing(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::BorrowAppContext as _;
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let journal = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "move\n\nkeep\n").unwrap();
        let vault = std::sync::Arc::new(Vault::open(dir.path()).unwrap());
        let path = VaultPath::new("a.md").unwrap();
        let (doc, stamp) = vault.read(&path).unwrap();
        let (editor, cx) = cx.add_window_view(|_, cx| {
            let mut e = Editor::new(vault, path, doc, stamp, cx);
            e.set_journal_root(journal.path().into());
            e
        });
        cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                e.cut_blocks(0..6, window, cx);
                let token = e.io.cut.as_ref().unwrap().token.clone();
                cx.update_global::<super::Cuts, _>(|cuts, _| cuts.0.get_mut(&token).unwrap().pending = false);
                e.on_undo(&crate::actions::DocUndo, window, cx);
            });
        });
        cx.run_until_parked();
        assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), "move\n\nkeep\n");
        assert!(editor.read_with(cx, |e, _| e.io.cut.is_none()));
    }

    #[gpui_kit::test]
    fn g11_local_undo_persists_source_before_invalidating_cut(cx: &mut gpui_kit::TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let journal = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "move\n\nkeep\n").unwrap();
        let vault = std::sync::Arc::new(Vault::open(dir.path()).unwrap());
        let path = VaultPath::new("a.md").unwrap();
        let (doc, stamp) = vault.read(&path).unwrap();
        let (editor, cx) = cx.add_window_view(|_, cx| {
            let mut e = Editor::new(vault, path, doc, stamp, cx);
            e.set_journal_root(journal.path().into());
            e
        });
        cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                e.cut_blocks(0..6, window, cx);
                e.flush_sync().unwrap();
                e.on_undo(&crate::actions::DocUndo, window, cx);
            })
        });
        cx.run_until_parked();
        assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), "move\n\nkeep\n");
        assert_eq!(std::fs::read_dir(journal.path()).unwrap().count(), 0);
        assert!(editor.read_with(cx, |e, _| e.io.cut.is_none()));
    }
}
