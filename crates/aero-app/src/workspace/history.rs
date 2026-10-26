//! Session-wide transaction history. File bytes remain the source of truth;
//! records only connect document undo boundaries across tabs and windows.
use super::*;
use aero_core::transaction::FileChange;

#[derive(Default)]
struct History {
    records: std::collections::HashMap<(String, String), Vec<FileChange>>,
    latest: std::collections::HashMap<(String, VaultPath), (String, bool)>,
}
impl Global for History {}

pub(crate) fn contains(vault: &str, token: &str, cx: &App) -> bool {
    cx.try_global::<History>().is_some_and(|h| h.records.contains_key(&(vault.into(), token.into())))
}

pub(super) fn record(
    vault: &str,
    token: &str,
    changes: &[FileChange],
    direction: Option<bool>,
    cx: &mut App,
) {
    if !cx.has_global::<History>() {
        cx.set_global(History::default());
    }
    cx.update_global::<History, _>(|history, _| {
        if direction.is_none() {
            history.records.insert((vault.into(), token.into()), changes.to_vec());
        }
        for change in changes {
            history
                .latest
                .insert((vault.into(), change.path.clone()), (token.into(), direction != Some(false)));
        }
    });
}

pub(crate) fn attach(vault: &str, path: &VaultPath, doc: &mut aero_core::Document, stamp: aero_core::FileStamp, cx: &App) {
    let Some(history) = cx.try_global::<History>() else { return };
    let Some((token, applied)) = history.latest.get(&(vault.into(), path.clone())) else { return };
    let Some(changes) = history.records.get(&(vault.into(), token.clone())) else { return };
    let Some(change) = changes.iter().find(|c| &c.path == path) else { return };
    if (if *applied { &change.after } else { &change.before }).as_ref().is_some_and(|bytes| {
        aero_core::hash_bytes(bytes) == stamp.hash
            && aero_core::SourceText::decode(bytes).is_ok_and(|source| source.text == doc.to_string())
    }) {
        let after = change
            .after
            .as_deref()
            .and_then(|b| aero_core::Document::from_bytes(b).ok())
            .unwrap_or_default()
            .to_string();
        let mut base = change
            .before
            .as_deref()
            .and_then(|b| aero_core::Document::from_bytes(b).ok())
            .unwrap_or_default();
        let _ = base.edit_transaction(vec![aero_core::Edit::replace(0..base.len(), after)], token);
        base.reparse();
        if !*applied {
            base.undo();
            base.reparse();
        }
        *doc = base;
    }
}

#[cfg(test)]
mod tests {
    use super::{attach, record};

    #[gpui_kit::test]
    fn mixed_line_endings_attach_to_the_recorded_transaction(cx: &mut gpui_kit::TestAppContext) {
        let path = aero_core::VaultPath::new("sibling.md").unwrap();
        let before = b"---\r\naero:\n  order: 1024\r\n---\nbody\r\n";
        let after = b"---\r\naero:\n  order: 2048\r\n---\nbody\r\n";
        let change = aero_core::transaction::FileChange {
            path: path.clone(), before: Some(before.to_vec()), after: Some(after.to_vec()),
        };
        cx.update(|cx| {
            record("vault", "token", &[change], None, cx);
            let mut doc = aero_core::Document::from_bytes(after).unwrap();
            let stamp = aero_core::FileStamp { mtime: std::time::SystemTime::UNIX_EPOCH, size: after.len() as u64, hash: aero_core::hash_bytes(after) };
            attach("vault", &path, &mut doc, stamp, cx);
            assert_eq!(doc.history_transaction(false), Some("token"));
            doc.undo();
            assert_eq!(doc.to_string(), aero_core::Document::from_bytes(before).unwrap().to_string());
        });
    }
}

impl Workspace {
    pub(super) fn request_history(
        &mut self,
        token: &str,
        redo: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = &self.open else { return };
        let Some(changes) = cx
            .try_global::<History>()
            .and_then(|h| h.records.get(&(open.vault.id().into(), token.into())))
            .cloned()
        else {
            window.push_notification(Notification::warning("此关联操作的会话记录已不可用。"), cx);
            return;
        };
        for editor in Editor::live_editors(open.vault.id(), cx) {
            let editor = editor.read(cx);
            if changes.iter().any(|c| &c.path == editor.path())
                && editor.history_transaction(redo) != Some(token)
            {
                let path = editor.path().clone();
                window.push_notification(
                    Notification::warning(format!(
                        "请先撤销「{}」中的后续编辑，再撤销关联操作。点击前往该文档。",
                        path.stem()
                    ))
                    .autohide(false)
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.open_note(path.clone(), window, cx)),
                    ),
                    cx,
                );
                return;
            }
        }
        self.request_operation(
            PendingOperation::File(files::Operation::Transaction {
                token: token.into(),
                changes: if redo { changes } else { changes.iter().map(FileChange::reverse).collect() },
                history_changes: None,
                direction: Some(redo),
                open: None,
                journal: self.data_dir.join("transactions").join(open.vault.id()),
            }),
            window,
            cx,
        );
    }
}
