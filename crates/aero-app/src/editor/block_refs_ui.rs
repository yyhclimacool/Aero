use super::*;

impl Editor {
    /// 调用前已检查光标、菜单与 IME；单个外部文件校验并原子落盘后再插入引用。
    pub(super) fn confirm_block_ref(
        &mut self,
        row: Option<usize>,
        alias: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu = self.wiki.as_ref().unwrap();
        let Some(blocks) = &menu.blocks else { return };
        let result = (|| -> Result<_, String> {
            let index = *menu
                .matches
                .get(row.unwrap_or(menu.selected))
                .ok_or("没有匹配的块，请修改关键词或按 Esc 关闭")?;
            let candidate = &blocks.entries[index];
            let target = if menu.trigger.fragment_target.as_deref() == Some("") {
                String::new()
            } else {
                let catalog = menu.catalog.as_ref().ok_or("笔记目录尚未加载")?;
                let index =
                    catalog.notes.iter().position(|note| note.path == blocks.path).ok_or("目标笔记已变化")?;
                catalog.target(index, self.path.parent()).ok_or("无法生成不歧义的链接，请调整笔记名称")?
            };
            let (id, edit, touched) = if blocks.path == self.path {
                let assignment = aero_core::block_refs::prepare(&self.doc, candidate.block)?;
                (assignment.id, assignment.edit, None)
            } else {
                let hash = blocks.hash.ok_or("目标笔记缺少版本信息，请重新打开补全")?;
                let (id, changed) =
                    aero_core::block_refs::assign_on_disk(&self.vault, &blocks.path, hash, candidate.block)?;
                (id, None, changed.then(|| blocks.path.clone()))
            };
            Ok((format!("{target}#^{id}"), candidate.excerpt.clone(), edit, touched))
        })();
        let (target, label, id_edit, touched) = match result {
            Ok(value) => value,
            Err(error) => {
                self.wiki.as_mut().unwrap().error = Some(error);
                cx.notify();
                return;
            }
        };
        let range = self.wiki.as_ref().unwrap().trigger.range.clone();
        let embed = self.wiki.as_ref().unwrap().trigger.embed;
        let in_table = self.focused_in_table();
        let (replacement, selected) =
            wiki::insertion(&target, alias.then_some(label.as_str()), in_table, embed);
        let mut at = range.start;
        if let Some(edit) = &id_edit
            && edit.range.start < at
        {
            at = (at as isize + edit.delta()) as usize;
        }
        let mut edits = vec![Edit::replace(range, replacement.clone())];
        edits.extend(id_edit);
        edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
        if let Err(err) = self.doc.edit_group(edits) {
            // 范围已由菜单和 prepare 验证；保留错误以防未来调用方破坏这一前提。
            self.wiki.as_mut().unwrap().error = Some(err.to_string());
            if let Some(path) = touched {
                cx.emit(EditorEvent::PathsTouched(vec![path]));
            }
            cx.notify();
            return;
        }
        self.wiki = None;
        self.slash = None;
        self.focused = None;
        self.after_doc_change(cx);
        self.focus_at(at + replacement.len(), None, window, cx);
        if let (Some(selected), Some(f)) = (selected, &self.focused) {
            let range = at + selected.start - f.range.start..at + selected.end - f.range.start;
            f.textarea.update(cx, |ta, cx| ta.set_selected_range(range, cx));
        }
        if let Some(path) = touched {
            cx.emit(EditorEvent::PathsTouched(vec![path]));
        }
    }
}
