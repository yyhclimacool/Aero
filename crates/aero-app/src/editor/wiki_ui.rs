use super::*;

impl Editor {
    fn wiki_trigger(&self, cx: &App) -> Option<wiki::Trigger> {
        if self.editing_code() {
            return None;
        }
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

    pub(super) fn input_composing(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
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
            && menu.trigger.fragment_target == trigger.fragment_target
            && menu.trigger.block_ref == trigger.block_ref
            && menu.trigger.embed == trigger.embed
        {
            menu.trigger = trigger;
            if menu.headings.as_ref().is_some_and(|h| h.path == self.path) {
                menu.headings = Some(wiki::HeadingCatalog::new(self.path.clone(), &self.doc));
            }
            if menu.blocks.as_ref().is_some_and(|b| b.path == self.path) {
                menu.blocks = Some(wiki::BlockCatalog::new(self.path.clone(), &self.doc, None));
            }
            menu.refilter();
        } else {
            self.wiki_generation += 1;
            let local = trigger.fragment_target.as_deref() == Some("");
            let block_ref = trigger.block_ref;
            self.wiki = Some(wiki::Menu {
                trigger,
                generation: self.wiki_generation,
                catalog: None,
                headings: (local && !block_ref)
                    .then(|| wiki::HeadingCatalog::new(self.path.clone(), &self.doc)),
                blocks: (local && block_ref)
                    .then(|| wiki::BlockCatalog::new(self.path.clone(), &self.doc, None)),
                error: None,
                matches: Vec::new(),
                selected: 0,
            });
            if local {
                self.wiki.as_mut().unwrap().refilter();
            } else {
                cx.emit(EditorEvent::LinkCompletionRequested(self.wiki_generation));
            }
        }
        cx.notify();
    }

    pub fn load_wiki_notes(
        &mut self,
        generation: u64,
        notes: Task<Result<Vec<aero_core::links::NoteSummary>, String>>,
        recent: Vec<VaultPath>,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.wiki.as_ref().filter(|m| m.generation == generation) else { return };
        let fragment_target = menu.trigger.fragment_target.clone();
        let block_ref = menu.trigger.block_ref;
        let embed = menu.trigger.embed;
        let path = self.path.clone();
        let summary = aero_core::links::NoteSummary::from_document(path.clone(), &self.doc);
        let source = fragment_target.as_ref().map(|_| self.doc.to_string());
        let vault = self.vault.clone();
        let task = cx.background_spawn(async move {
            notes.await.and_then(|mut notes| {
                if let Some(note) = notes.iter_mut().find(|note| note.path == path) {
                    *note = summary;
                } else {
                    notes.push(summary);
                }
                let mut catalog = wiki::Catalog::new(notes, recent);
                if embed {
                    catalog.embed_source = Some(path.clone());
                    if fragment_target.is_none() {
                        catalog.attachments =
                            aero_core::attachments::Catalog::read(&vault).map_err(|e| e.to_string())?;
                    }
                }
                let (headings, blocks) = if let Some(target) = fragment_target {
                    use aero_core::links::Resolution;
                    if embed && aero_core::attachments::Kind::for_path(&target).is_some() {
                        return Err("图片和 PDF 不提供标题或块补全；嵌入笔记请使用 .md 文件名".into());
                    }
                    let resolved = match catalog.resolver.resolve(&target, None, path.parent()) {
                        Resolution::Resolved { path, .. } => path,
                        Resolution::Ambiguous { .. } => return Err("笔记名称有歧义，请输入完整路径".into()),
                        Resolution::Unresolved => return Err("找不到目标笔记，请先选择有效的笔记名称".into()),
                    };
                    let (doc, hash) = if resolved == path {
                        (Document::from_text(source.as_deref().unwrap_or_default()), None)
                    } else {
                        let (doc, stamp) = vault.read(&resolved).map_err(|e| e.to_string())?;
                        (doc, Some(stamp.hash))
                    };
                    if block_ref {
                        (None, Some(wiki::BlockCatalog::new(resolved, &doc, hash)))
                    } else {
                        (Some(wiki::HeadingCatalog::new(resolved, &doc)), None)
                    }
                } else {
                    (None, None)
                };
                Ok((catalog, headings, blocks))
            })
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let Some(menu) = this.wiki.as_mut().filter(|m| m.generation == generation) else { return };
                match result {
                    Ok((catalog, headings, blocks)) => {
                        menu.catalog = Some(catalog);
                        menu.headings = headings.map(|h| {
                            if h.path == this.path {
                                wiki::HeadingCatalog::new(this.path.clone(), &this.doc)
                            } else {
                                h
                            }
                        });
                        menu.blocks = blocks.map(|b| {
                            if b.path == this.path {
                                wiki::BlockCatalog::new(this.path.clone(), &this.doc, None)
                            } else {
                                b
                            }
                        });
                        menu.refilter();
                    }
                    Err(err) => menu.error = Some(format!("{err}。Esc 关闭后重试")),
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
        if self.input_composing(window, cx) || !self.confirm_wiki(None, true, window, cx) {
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
        if self.input_composing(window, cx) {
            return true;
        }
        if self.wiki.as_ref().is_some_and(|m| m.trigger.block_ref) {
            self.confirm_block_ref(row, alias, window, cx);
            return true;
        }
        let menu = self.wiki.as_mut().unwrap();
        let (target, label) = if let Some(target) = &menu.trigger.fragment_target {
            let Some(headings) = &menu.headings else { return true };
            let Some(&index) = menu.matches.get(row.unwrap_or(menu.selected)) else {
                menu.error = Some("没有匹配的标题，请修改关键词或按 Esc 关闭".into());
                cx.notify();
                return true;
            };
            if headings.path != self.path && !self.vault.exists(&headings.path) {
                menu.error = Some("目标笔记已被移走或删除，Esc 关闭后重试".into());
                cx.notify();
                return true;
            }
            // 与笔记补全一样写入已消歧的目标；按 front matter 标题输入时也可在索引未就绪时点击。
            let canonical = if target.is_empty() {
                Some(String::new())
            } else {
                menu.catalog.as_ref().and_then(|c| {
                    let index = c.notes.iter().position(|note| note.path == headings.path)?;
                    c.target(index, self.path.parent()).map(|s| aero_core::links::split_target(&s).0)
                })
            };
            let Some(target) = canonical.as_deref().and_then(|t| headings.target(index, t)) else {
                menu.error = Some("该标题含链接语法无法表示的字符，请调整标题".into());
                cx.notify();
                return true;
            };
            (target, headings.entries[index].text.clone())
        } else {
            let Some(catalog) = &menu.catalog else { return true }; // 加载 / 失败不当作无匹配。
            if let Some(&index) = menu.matches.get(row.unwrap_or(menu.selected)) {
                if let Some(attachment) =
                    index.checked_sub(catalog.notes.len()).and_then(|i| catalog.attachments.entries.get(i))
                {
                    if !aero_core::attachments::available(&self.vault, &attachment.path) {
                        menu.error = Some("附件已被移走或删除，Esc 关闭后重试".into());
                        cx.notify();
                        return true;
                    }
                    let Some(target) = catalog.attachments.target(index - catalog.notes.len(), &self.path)
                    else {
                        menu.error = Some("无法生成不歧义的附件路径，请调整文件名".into());
                        cx.notify();
                        return true;
                    };
                    (target, attachment.path.file_name().to_owned())
                } else {
                    let aero_core::links::NoteSummary { path, title, .. } = &catalog.notes[index];
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
                }
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
            }
        };
        let range = menu.trigger.range.clone();
        let embed = menu.trigger.embed;
        let in_table = self.focused_in_table();
        let (replacement, selected) =
            wiki::insertion(&target, alias.then_some(label.as_str()), in_table, embed);
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
        if menu.catalog.is_some() || menu.headings.is_some() || menu.blocks.is_some() {
            for (row, &index) in menu.matches.iter().enumerate() {
                let (title, detail) = if let Some(blocks) = &menu.blocks {
                    let b = &blocks.entries[index];
                    let id =
                        b.persisted_id.as_deref().map_or("确认后生成 ID".into(), |id| format!("^{id}"));
                    (
                        b.excerpt.clone(),
                        format!("{} · {id} · {}", block_kind_label(&b.kind), blocks.path.as_str()),
                    )
                } else if let Some(headings) = &menu.headings {
                    let heading = &headings.entries[index];
                    (heading.text.clone(), format!("H{} · {}", heading.level, headings.path.as_str()))
                } else {
                    let catalog = menu.catalog.as_ref().unwrap();
                    if let Some(attachment) = index
                        .checked_sub(catalog.notes.len())
                        .and_then(|i| catalog.attachments.entries.get(i))
                    {
                        let kind = match attachment.kind {
                            aero_core::attachments::Kind::Image => "图片",
                            aero_core::attachments::Kind::Pdf => "PDF",
                        };
                        (attachment.path.file_name().to_owned(), format!("{kind} · {}", attachment.path))
                    } else {
                        let aero_core::links::NoteSummary { path, title, .. } = &catalog.notes[index];
                        (
                            title.clone(),
                            if menu.trigger.embed { format!("笔记 · {path}") } else { path.to_string() },
                        )
                    }
                };
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
                        .child(div().text_size(px(13.)).truncate().child(title))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(detail),
                        )
                        .into_any_element(),
                );
            }
        }
        let hint = if let Some(error) = &menu.error {
            error.clone()
        } else if menu.trigger.block_ref && menu.blocks.is_none() {
            "正在读取块…".into()
        } else if menu.trigger.block_ref && menu.matches.is_empty() {
            "没有匹配的块 · Esc 关闭".into()
        } else if !menu.trigger.block_ref && menu.trigger.fragment_target.is_some() && menu.headings.is_none()
        {
            "正在读取标题…".into()
        } else if menu.trigger.fragment_target.is_some() && menu.matches.is_empty() {
            "没有匹配的标题 · Esc 关闭".into()
        } else if menu.catalog.is_none() && menu.headings.is_none() && menu.blocks.is_none() {
            if menu.trigger.embed {
                "正在读取笔记与附件…".into()
            } else {
                "正在读取笔记…".into()
            }
        } else if menu.matches.is_empty() {
            if menu.trigger.embed {
                "没有匹配 · ⏎ 插入未解析嵌入".into()
            } else {
                "没有匹配 · ⏎ 插入未解析链接".into()
            }
        } else if menu.trigger.embed {
            "↑↓ 选择 · ⏎ / Tab 插入嵌入 · Esc 关闭".into()
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

fn block_kind_label(kind: &BlockKind) -> &'static str {
    match kind {
        BlockKind::Paragraph => "段落",
        BlockKind::Heading { .. } => "标题",
        BlockKind::List { .. } | BlockKind::ListItem { .. } => "列表",
        BlockKind::CodeFence { .. } | BlockKind::CodeBlock => "代码",
        BlockKind::Table => "表格",
        BlockKind::BlockQuote => "引用",
        BlockKind::Callout { .. } => "提示块",
        BlockKind::MathBlock => "公式",
        _ => "块",
    }
}
