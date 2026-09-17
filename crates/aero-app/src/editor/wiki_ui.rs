use super::*;

impl Editor {
    fn wiki_trigger(&self, cx: &App) -> Option<wiki::Trigger> {
        let f = self.focused.as_ref()?;
        let ta = f.textarea.read(cx);
        if !ta.selected_range().is_empty() {
            return None;
        }
        let mut trigger = wiki::trigger(&ta.value(), ta.cursor())?;
        trigger.range = trigger.range.start + f.range.start..trigger.range.end + f.range.start;
        trigger.cursor += f.range.start;
        Some(trigger)
    }

    pub(super) fn wiki_is_current(&self, cx: &App) -> bool {
        self.wiki.as_ref().is_some_and(|m| self.wiki_trigger(cx).as_ref() == Some(&m.trigger))
    }

    pub(super) fn wiki_composing(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.focused
            .as_ref()
            .is_some_and(|f| f.textarea.update(cx, |ta, cx| ta.marked_text_range(window, cx).is_some()))
    }

    pub(super) fn update_wiki(&mut self, cx: &mut Context<Self>) {
        let Some(trigger) = self.wiki_trigger(cx) else {
            self.wiki = None;
            return;
        };
        if let Some(menu) = &mut self.wiki
            && menu.trigger.range.start == trigger.range.start
        {
            menu.trigger = trigger;
            menu.refilter();
        } else {
            self.wiki_generation += 1;
            self.wiki = Some(wiki::Menu {
                trigger,
                generation: self.wiki_generation,
                catalog: None,
                error: None,
                matches: Vec::new(),
                selected: 0,
            });
            cx.emit(EditorEvent::LinkCompletionRequested(self.wiki_generation));
        }
        cx.notify();
    }

    pub fn load_wiki_notes(
        &mut self,
        generation: u64,
        notes: Task<Result<Vec<(VaultPath, String)>, String>>,
        recent: Vec<VaultPath>,
        cx: &mut Context<Self>,
    ) {
        let path = self.path.clone();
        let title = self.doc.title_or(path.stem());
        let task = cx.background_spawn(async move {
            notes.await.map(|mut notes| {
                if let Some((_, t)) = notes.iter_mut().find(|(p, _)| p == &path) {
                    *t = title;
                }
                wiki::Catalog::new(notes, recent)
            })
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let Some(menu) = this.wiki.as_mut().filter(|m| m.generation == generation) else { return };
                match result {
                    Ok(catalog) => {
                        menu.catalog = Some(catalog);
                        menu.refilter();
                    }
                    Err(err) => menu.error = Some(format!("无法读取笔记：{err}。Esc 关闭后重试")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn move_wiki(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        if !self.wiki_is_current(cx) {
            self.wiki = None;
            return false;
        }
        let menu = self.wiki.as_mut().unwrap();
        if !menu.matches.is_empty() {
            menu.selected = (menu.selected as isize + delta).rem_euclid(menu.matches.len() as isize) as usize;
        }
        cx.notify();
        true
    }

    pub(super) fn on_link_alias(&mut self, _: &LinkAlias, window: &mut Window, cx: &mut Context<Self>) {
        if self.wiki_composing(window, cx) || !self.confirm_wiki(None, true, window, cx) {
            cx.propagate();
        }
    }

    pub(super) fn confirm_wiki(
        &mut self,
        row: Option<usize>,
        alias: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.wiki_is_current(cx) {
            self.wiki = None;
            return false;
        }
        if self.wiki_composing(window, cx) {
            return true;
        }
        let menu = self.wiki.as_mut().unwrap();
        let Some(catalog) = &menu.catalog else { return true }; // 正在读取 / 失败时不能当成「没有匹配」。
        let (target, label) = if let Some(&index) = menu.matches.get(row.unwrap_or(menu.selected)) {
            let (path, title) = &catalog.notes[index];
            if !self.vault.exists(path) {
                menu.error = Some("该笔记已被移走或删除，Esc 关闭后重新输入 [[ 刷新".into());
                cx.notify();
                return true;
            }
            let Some(target) = catalog.target(index, self.path.parent()) else {
                menu.error = Some("无法为该笔记生成不歧义的链接，请先调整文件名".into());
                cx.notify();
                return true;
            };
            (target, title.clone())
        } else {
            let query = menu.trigger.query.trim();
            let Some(target) = aero_core::links::escape_target(query)
                .filter(|_| aero_core::links::creation_path(self.path.parent(), query).is_some())
            else {
                menu.error = Some("请输入有效的笔记名称".into());
                cx.notify();
                return true;
            };
            (target, query.to_owned())
        };
        let range = menu.trigger.range.clone();
        let in_table =
            self.focused_unit().is_some_and(|ix| matches!(self.units[ix].kind, BlockKind::Table { .. }));
        let (replacement, selected) = wiki::insertion(&target, alias.then_some(label.as_str()), in_table);
        let at = range.start;
        self.wiki = None;
        self.slash = None;
        self.structural_edit(Edit::replace(range, replacement.clone()), at + replacement.len(), window, cx);
        if let (Some(selected), Some(f)) = (selected, &self.focused) {
            let start = at + selected.start - f.range.start;
            let end = at + selected.end - f.range.start;
            f.textarea.update(cx, |ta, cx| ta.set_selected_range(start..end, cx));
        }
        true
    }

    pub(super) fn render_wiki_menu(&self, cx: &Context<Self>) -> AnyElement {
        let (Some(menu), Some(f)) = (&self.wiki, &self.focused) else { return div().into_any_element() };
        let theme = cx.theme();
        let ta = f.textarea.read(cx);
        let position = ta.cursor_layout().map_or_else(
            || {
                let b = ta.input_bounds();
                b.origin + point(px(0.), b.size.height + px(4.))
            },
            |(b, height)| b.origin + point(px(0.), height + px(4.)),
        );
        let mut rows = Vec::new();
        if let Some(catalog) = &menu.catalog {
            for (row, &index) in menu.matches.iter().enumerate() {
                let (path, title) = &catalog.notes[index];
                rows.push(
                    v_flex()
                        .id(("wiki-row", row))
                        .debug_selector(move || format!("wiki-row-{row}"))
                        .w_full()
                        .px_2()
                        .py_1()
                        .rounded(px(4.))
                        .cursor_pointer()
                        .when(row == menu.selected, |el| el.bg(theme.list_active))
                        .hover(|el| el.bg(theme.list_hover))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.confirm_wiki(Some(row), false, window, cx);
                            }),
                        )
                        // 不让 mouse-up 再落到块预览的 on_click，改变刚插入后的光标。
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(div().text_size(px(13.)).truncate().child(title.clone()))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(path.as_str().to_owned()),
                        )
                        .into_any_element(),
                );
            }
        }
        let hint = if let Some(error) = &menu.error {
            error.clone()
        } else if menu.catalog.is_none() {
            "正在读取笔记…".into()
        } else if menu.matches.is_empty() {
            "没有匹配 · ⏎ 插入未解析链接".into()
        } else {
            "↑↓ 选择 · ⏎ / Tab 插入 · ⇧⏎ 编辑别名 · Esc 关闭".into()
        };
        rows.push(
            div()
                .px_2()
                .py_1()
                .text_size(px(11.))
                .text_color(theme.muted_foreground)
                .child(hint)
                .into_any_element(),
        );
        deferred(
            anchored().position(position).snap_to_window_with_margin(px(8.)).child(
                v_flex()
                    .id("wiki-menu")
                    .debug_selector(|| "wiki-menu".into())
                    .occlude()
                    .w(px(360.))
                    .p_1()
                    .gap(px(1.))
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    .border_1()
                    .border_color(theme.border)
                    .rounded(px(8.))
                    .shadow_lg()
                    .children(rows),
            ),
        )
        .into_any_element()
    }
}
