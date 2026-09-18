use super::*;

impl Editor {
    fn tag_trigger(&self, cx: &App) -> Option<aero_core::tags::Completion> {
        let f = self.focused.as_ref()?;
        let ta = f.textarea.read(cx);
        if !ta.selected_range().is_empty() {
            return None;
        }
        let mut trigger = aero_core::tags::completion(&ta.value(), ta.cursor())?;
        trigger.range = trigger.range.start + f.range.start..trigger.range.end + f.range.start;
        trigger.cursor += f.range.start;
        Some(trigger)
    }

    pub(super) fn tag_is_current(&self, cx: &App) -> bool {
        self.tag_menu.as_ref().is_some_and(|m| self.tag_trigger(cx).as_ref() == Some(&m.trigger))
    }

    pub(super) fn update_tags(&mut self, cx: &mut Context<Self>) {
        let Some(trigger) = self.tag_trigger(cx).filter(|_| self.wiki.is_none()) else {
            self.tag_menu = None;
            return;
        };
        if let Some(menu) = &mut self.tag_menu
            && menu.trigger.range.start == trigger.range.start
        {
            menu.trigger = trigger;
            menu.refilter();
        } else {
            self.tag_generation += 1;
            self.tag_menu = Some(tags::Menu {
                trigger,
                generation: self.tag_generation,
                candidates: None,
                error: None,
                matches: Vec::new(),
                selected: 0,
            });
            cx.emit(EditorEvent::TagCompletionRequested(self.tag_generation));
        }
        cx.notify();
    }

    /// 排除光标下尚未写完的标签；当前笔记里的其他未保存标签仍可参加补全。
    pub fn tag_completion_source(&self) -> String {
        let mut source = self.doc.to_string();
        if let Some(menu) = &self.tag_menu {
            source.replace_range(menu.trigger.range.clone(), "");
        }
        source
    }

    pub fn load_tags(
        &mut self,
        generation: u64,
        task: Task<Result<Vec<aero_index::TagCount>, String>>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let Some(menu) = this.tag_menu.as_mut().filter(|m| m.generation == generation) else {
                    return;
                };
                match result {
                    Ok(candidates) => menu.set_candidates(candidates),
                    Err(err) => menu.error = Some(format!("无法读取标签：{err}。Esc 关闭后重试")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn move_tag(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        if !self.tag_is_current(cx) {
            self.tag_menu = None;
            return false;
        }
        let menu = self.tag_menu.as_mut().unwrap();
        if !menu.matches.is_empty() {
            menu.selected = (menu.selected as isize + delta).rem_euclid(menu.matches.len() as isize) as usize;
        }
        cx.notify();
        true
    }

    pub(super) fn confirm_tag(
        &mut self,
        row: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.tag_is_current(cx) {
            self.tag_menu = None;
            return false;
        }
        if self.input_composing(window, cx) {
            return true;
        }
        let menu = self.tag_menu.as_ref().unwrap();
        let Some(candidates) = &menu.candidates else { return true }; // 加载 / 失败不能当成空结果。
        let Some(&index) = menu.matches.get(row.unwrap_or(menu.selected)) else {
            self.tag_menu = None;
            cx.notify();
            return false; // 新标签已经是合法源码，Enter / Tab 保持普通编辑行为。
        };
        let replacement = format!("#{}", candidates[index].tag);
        let range = menu.trigger.range.clone();
        let cursor = range.start + replacement.len();
        self.tag_menu = None;
        self.structural_edit(Edit::replace(range, replacement), cursor, window, cx);
        true
    }

    pub(super) fn render_tag_menu(&self, cx: &Context<Self>) -> AnyElement {
        let (Some(menu), Some(f)) = (&self.tag_menu, &self.focused) else { return div().into_any_element() };
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
        if let Some(candidates) = &menu.candidates {
            for (row, &index) in menu.matches.iter().enumerate() {
                let candidate = &candidates[index];
                rows.push(
                    h_flex()
                        .id(("tag-row", row))
                        .debug_selector(move || format!("tag-row-{row}"))
                        .w_full()
                        .h(px(30.))
                        .px_2()
                        .gap_2()
                        .rounded(px(4.))
                        .cursor_pointer()
                        .justify_between()
                        .when(row == menu.selected, |el| el.bg(theme.list_active))
                        .hover(|el| el.bg(theme.list_hover))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.confirm_tag(Some(row), window, cx);
                            }),
                        )
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(13.))
                                .truncate()
                                .child(format!("#{}", candidate.tag)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .child(format!("{} 篇", candidate.note_count)),
                        )
                        .into_any_element(),
                );
            }
        }
        let hint = if let Some(err) = &menu.error {
            err.clone()
        } else if menu.candidates.is_none() {
            "正在读取标签…".into()
        } else if menu.matches.is_empty() {
            "没有匹配 · 可继续输入新标签".into()
        } else {
            "↑↓ 选择 · ⏎ / Tab 插入 · Esc 关闭".into()
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
                    .id("tag-menu")
                    .debug_selector(|| "tag-menu".into())
                    .occlude()
                    .w(px(300.))
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
