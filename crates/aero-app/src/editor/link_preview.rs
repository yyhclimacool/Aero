use super::*;
use crate::search::SearchService;
use aero_core::links::{Resolution, Resolver};
use gpui_kit::base::text::{LinkStyle, LinkUnderline};

#[derive(Default)]
pub(super) struct LinkPreview {
    resolver: Option<Resolver>,
    resolver_generation: u64,
    resolver_task: Option<Task<()>>,
    subscription: Option<Subscription>,
    window_subscriptions: Option<[Subscription; 2]>,
    scroll_at_hover: Option<Point<Pixels>>,
    hovered: Option<(SharedString, Bounds<Pixels>)>,
    generation: u64,
    task: Option<Task<()>>,
    close_task: Option<Task<()>>,
    popup: Option<Popup>,
    in_popup: bool,
}

struct Popup {
    title: String,
    body: String,
    bounds: Bounds<Pixels>,
}

fn excerpt(doc: &Document, fragment: Option<&str>) -> Result<String, String> {
    let range = fragment_range(doc, fragment)?;
    let text = aero_core::index::indexable_leaves(doc)
        .into_iter()
        .filter(|b| {
            let r = doc.range_of(b);
            r.start >= range.start && r.start < range.end
        })
        .map(|b| aero_core::index::plain_text(&doc.slice(doc.content_range(b)).to_string(), &b.kind))
        .collect::<Vec<_>>()
        .join("\n\n");
    let text = text.trim();
    Ok(if fragment.is_none_or(str::is_empty) { text.chars().take(300).collect() } else { text.to_owned() })
}

/// 链接 / 嵌入的片段在目标文档里的源码区间：`^id` 是那一块，`#标题` 是该章节（含子标题、
/// 不越过同级），没有片段就是整篇。悬停预览取它的纯文本，嵌入取它的源码。
pub(super) fn fragment_range(doc: &Document, fragment: Option<&str>) -> Result<Range<usize>, String> {
    Ok(if let Some(id) = fragment.and_then(|f| f.strip_prefix('^')) {
        let id = aero_core::block_refs::resolve(doc, id)?;
        let block = doc.blocks().iter().find(|(_, b)| b.id == id).unwrap().1;
        doc.range_of(block)
    } else if let Some(heading) = fragment.filter(|f| !f.is_empty()) {
        let blocks: Vec<_> = doc.blocks().iter().collect();
        let matches = |b: &Block, exact: bool| {
            if !matches!(b.kind, BlockKind::Heading { .. }) {
                return false;
            }
            let text = doc.slice(doc.content_range(b)).to_string();
            if exact {
                text.trim_start_matches('#').trim() == heading.trim()
            } else {
                heading_key(&aero_core::index::plain_text(&text, &b.kind)) == heading_key(heading)
            }
        };
        let index = blocks
            .iter()
            .position(|(_, b)| matches(b, true))
            .or_else(|| blocks.iter().position(|(_, b)| matches(b, false)))
            .ok_or_else(|| format!("没有标题「{heading}」"))?;
        let (depth, block) = blocks[index];
        let BlockKind::Heading { level } = block.kind else { unreachable!() };
        let end = blocks[index + 1..]
            .iter()
            .find(|(d, b)| {
                *d < depth
                    || (*d == depth && matches!(b.kind, BlockKind::Heading { level: next } if next <= level))
            })
            .map_or(doc.len(), |(_, b)| doc.range_of(b).start);
        doc.range_of(block).start..end
    } else {
        0..doc.len()
    })
}

impl Editor {
    pub fn connect_link_preview(&mut self, service: Entity<SearchService>, cx: &mut Context<Self>) {
        self.link_preview.subscription = Some(cx.observe(&service, |this, service, cx| {
            this.refresh_link_resolver(&service, cx);
        }));
        self.refresh_link_resolver(&service, cx);
    }

    fn refresh_link_resolver(&mut self, service: &Entity<SearchService>, cx: &mut Context<Self>) {
        // 目录一变，之前"解析不到"的嵌入可能就能解析了
        self.refresh_embeds(cx);
        let hovered = self.link_preview.hovered.clone();
        let scroll_at_hover = self.link_preview.scroll_at_hover;
        self.cancel_link_preview(cx);
        self.link_preview.hovered = hovered;
        self.link_preview.scroll_at_hover = scroll_at_hover;
        self.link_preview.resolver_generation += 1;
        let generation = self.link_preview.resolver_generation;
        let task = service.read(cx).link_resolver(self.vault.clone(), cx);
        self.link_preview.resolver_task = Some(cx.spawn(async move |this, cx| {
            let resolver = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.link_preview.resolver_generation != generation {
                    return;
                }
                this.link_preview.resolver = Some(resolver);
                this.update_local_link_names();
                if this.link_preview.hovered.is_some() {
                    this.schedule_link_preview(cx);
                }
                cx.notify();
            });
        }));
    }

    pub(super) fn update_local_link_names(&mut self) {
        let deleted = self.is_deleted();
        if let Some(resolver) = &mut self.link_preview.resolver {
            if deleted {
                resolver.remove(&self.path);
            } else {
                resolver.insert(
                    self.path.clone(),
                    Some(&self.doc.title_or(self.path.stem())),
                    &self.doc.front_matter_aliases(),
                );
            }
        }
    }

    pub(super) fn repath_link_preview(&mut self, old_path: &VaultPath, cx: &mut Context<Self>) {
        self.cancel_link_preview(cx);
        self.link_preview.resolver_generation += 1;
        self.link_preview.resolver_task = None;
        if let Some(resolver) = &mut self.link_preview.resolver {
            resolver.remove(old_path);
        }
        self.update_local_link_names();
    }

    pub(super) fn preview_link_style(&self, url: &str, cx: &App) -> LinkStyle {
        let Some((target, fragment)) = aero_core::links::decode_note_url(url) else {
            return LinkStyle { color: Some(cx.theme().link), underline: LinkUnderline::Solid };
        };
        let resolution = if target.is_empty() {
            if fragment.as_deref().is_some_and(|fragment| excerpt(&self.doc, Some(fragment)).is_err()) {
                Resolution::Unresolved
            } else {
                Resolution::Resolved { path: self.path.clone(), fragment }
            }
        } else if let Some(resolver) = &self.link_preview.resolver {
            resolver.resolve(&target, fragment.as_deref(), self.path.parent())
        } else {
            return LinkStyle { color: Some(cx.theme().link), underline: LinkUnderline::Solid };
        };
        match resolution {
            Resolution::Resolved { .. } => {
                LinkStyle { color: Some(cx.theme().link), underline: LinkUnderline::Solid }
            }
            Resolution::Unresolved => {
                LinkStyle { color: Some(cx.theme().muted_foreground), underline: LinkUnderline::Dashed }
            }
            Resolution::Ambiguous { .. } => {
                LinkStyle { color: Some(cx.theme().warning), underline: LinkUnderline::Dashed }
            }
        }
    }

    pub(super) fn hover_link(
        &mut self,
        url: &SharedString,
        entered: bool,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = (url.clone(), bounds);
        if !entered {
            if self.link_preview.hovered.as_ref() != Some(&key) {
                return;
            }
            self.link_preview.hovered = None;
            if self.link_preview.popup.is_none() {
                self.cancel_link_preview(cx);
            } else {
                self.schedule_link_close(cx);
            }
            return;
        }
        if self.link_preview.hovered.as_ref() == Some(&key) {
            return;
        }
        self.cancel_link_preview(cx);
        self.link_preview.window_subscriptions.get_or_insert_with(|| {
            [
                cx.observe_window_bounds(window, |this, _, cx| this.cancel_link_preview(cx)),
                cx.observe_window_activation(window, |this, _, cx| this.cancel_link_preview(cx)),
            ]
        });
        self.link_preview.scroll_at_hover = Some(self.scroll.offset());
        self.link_preview.hovered = Some(key);
        self.schedule_link_preview(cx);
    }

    pub(super) fn validate_link_preview_scroll(&mut self, cx: &mut Context<Self>) {
        if self.link_preview.scroll_at_hover.is_some_and(|offset| offset != self.scroll.offset()) {
            self.cancel_link_preview(cx);
        }
    }

    fn schedule_link_preview(&mut self, cx: &mut Context<Self>) {
        self.link_preview.generation += 1;
        let generation = self.link_preview.generation;
        self.link_preview.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(500)).await;
            let Ok(Some(task)) = this.update(cx, |this, cx| {
                if this.link_preview.generation != generation {
                    return None;
                }
                let (url, bounds) = this.link_preview.hovered.clone()?;
                Some(this.load_link_preview(url, bounds, cx))
            }) else {
                return;
            };
            let popup = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.link_preview.generation == generation && this.link_preview.hovered.is_some() {
                    this.link_preview.popup = Some(popup);
                    cx.notify();
                }
            });
        }));
    }

    /// 嵌入用的目标解析：与悬停预览、点击跳转同一套解析器与优先级。
    /// 笔记目录还没读进来时返回 `None`——那是"稍等"，不是"找不到"。
    pub(super) fn resolve_link_target(&self, target: &str, fragment: Option<&str>) -> Option<Resolution> {
        if target.is_empty() {
            return Some(Resolution::Resolved {
                path: self.path.clone(),
                fragment: fragment.map(str::to_owned),
            });
        }
        let resolver = self.link_preview.resolver.as_ref()?;
        Some(resolver.resolve(target, fragment, self.path.parent()))
    }

    /// 测试用：直接装一个笔记目录解析器，不必起真索引。
    #[cfg(test)]
    pub(super) fn install_test_resolver(&mut self, notes: &[(&str, Option<&str>)], cx: &mut Context<Self>) {
        let mut resolver = Resolver::new();
        for (path, title) in notes {
            resolver.insert(VaultPath::new(path).unwrap(), *title, &[]);
        }
        self.link_preview.resolver = Some(resolver);
        self.refresh_embeds(cx);
        cx.notify();
    }

    fn load_link_preview(&self, url: SharedString, bounds: Bounds<Pixels>, cx: &App) -> Task<Popup> {
        let Some((target, fragment)) = aero_core::links::decode_note_url(&url) else {
            return cx.background_spawn(async move {
                Popup { title: "外部链接".into(), body: url.to_string(), bounds }
            });
        };
        let resolution = if target.is_empty() {
            Resolution::Resolved { path: self.path.clone(), fragment }
        } else if let Some(resolver) = &self.link_preview.resolver {
            resolver.resolve(&target, fragment.as_deref(), self.path.parent())
        } else {
            return cx.background_spawn(async move {
                Popup { title: target, body: "正在读取笔记目录…".into(), bounds }
            });
        };
        match resolution {
            Resolution::Resolved { path, fragment } if path == self.path => {
                let title = self.title();
                let body = excerpt(&self.doc, fragment.as_deref()).unwrap_or_else(|error| error);
                cx.background_spawn(async move { Popup { title, body, bounds } })
            }
            Resolution::Resolved { path, fragment } => {
                let vault = self.vault.clone();
                cx.background_spawn(async move {
                    match vault.read(&path) {
                        Ok((doc, _)) => Popup {
                            title: doc.title_or(path.stem()),
                            body: excerpt(&doc, fragment.as_deref()).unwrap_or_else(|error| error),
                            bounds,
                        },
                        Err(error) => Popup {
                            title: path.as_str().into(),
                            body: format!("无法读取笔记：{error}"),
                            bounds,
                        },
                    }
                })
            }
            Resolution::Unresolved => cx.background_spawn(async move {
                Popup { title: target, body: "未解析链接：目标笔记不存在".into(), bounds }
            }),
            Resolution::Ambiguous { candidates } => cx.background_spawn(async move {
                Popup {
                    title: format!("歧义链接：{target}"),
                    body: format!(
                        "请使用完整路径消歧：\n{}",
                        candidates.iter().map(VaultPath::as_str).collect::<Vec<_>>().join("\n")
                    ),
                    bounds,
                }
            }),
        }
    }

    pub(super) fn cancel_link_preview(&mut self, cx: &mut Context<Self>) {
        self.link_preview.generation += 1;
        self.link_preview.task = None;
        self.link_preview.close_task = None;
        self.link_preview.hovered = None;
        self.link_preview.scroll_at_hover = None;
        self.link_preview.in_popup = false;
        if self.link_preview.popup.take().is_some() {
            cx.notify();
        }
    }

    fn schedule_link_close(&mut self, cx: &mut Context<Self>) {
        self.link_preview.close_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(150)).await;
            let _ = this.update(cx, |this, cx| {
                if !this.link_preview.in_popup && this.link_preview.hovered.is_none() {
                    this.cancel_link_preview(cx);
                }
            });
        }));
    }

    fn hover_link_popup(&mut self, entered: bool, cx: &mut Context<Self>) {
        self.link_preview.in_popup = entered;
        if entered {
            self.link_preview.close_task = None;
        } else {
            self.schedule_link_close(cx);
        }
    }

    pub(super) fn render_link_preview(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let popup = self.link_preview.popup.as_ref()?;
        let theme = cx.theme();
        Some(
            deferred(
                anchored()
                    .position(point(popup.bounds.left(), popup.bounds.bottom() + px(4.)))
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        v_flex()
                            .id("link-preview")
                            .debug_selector(|| "link-preview".into())
                            .occlude()
                            .w(px(360.))
                            .max_w_full()
                            .p_3()
                            .gap_2()
                            .rounded(px(8.))
                            .shadow_lg()
                            .bg(theme.popover)
                            .text_color(theme.popover_foreground)
                            .border_1()
                            .border_color(theme.border)
                            .on_hover(cx.listener(|this, hovered, _, cx| this.hover_link_popup(*hovered, cx)))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(|_, _, cx| cx.stop_propagation())
                            .child(
                                div().text_sm().font_weight(FontWeight::SEMIBOLD).child(popup.title.clone()),
                            )
                            .child(
                                div()
                                    .id("link-preview-body")
                                    .max_h(px(280.))
                                    .overflow_y_scroll()
                                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                                    .text_sm()
                                    .line_height(relative(1.6))
                                    .child(popup.body.clone()),
                            ),
                    ),
            )
            .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use aero_core::Document;

    use super::super::Editor;
    use super::excerpt;
    use gpui_kit::{Bounds, TestAppContext, point, px};
    use std::{sync::Arc, time::Duration};

    #[gpui_kit::test]
    fn hover_waits_500ms_and_cancels_before_opening(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "body\n").unwrap();
        let vault = Arc::new(aero_core::Vault::open(dir.path()).unwrap());
        let path = aero_core::VaultPath::new("a.md").unwrap();
        let (doc, stamp) = vault.read(&path).unwrap();
        let (editor, cx) = cx.add_window_view(|_, cx| Editor::new(vault, path, doc, stamp, cx));
        let bounds = Bounds::from_corners(point(px(20.), px(20.)), point(px(100.), px(45.)));
        let url = aero_core::links::encode_note_url("", None).into();
        cx.update(|window, cx| editor.update(cx, |e, cx| e.hover_link(&url, true, bounds, window, cx)));
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(499));
        cx.run_until_parked();
        cx.update(|_, cx| assert!(editor.read(cx).link_preview.popup.is_none()));
        cx.update(|window, cx| editor.update(cx, |e, cx| e.hover_link(&url, false, bounds, window, cx)));
        cx.executor().advance_clock(Duration::from_millis(10));
        cx.run_until_parked();
        cx.update(|_, cx| assert!(editor.read(cx).link_preview.popup.is_none()));
        cx.update(|window, cx| editor.update(cx, |e, cx| e.hover_link(&url, true, bounds, window, cx)));
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(editor.read(cx).link_preview.popup.as_ref().unwrap().body, "body"));
        cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                e.hover_link(&url, false, bounds, window, cx);
                e.hover_link_popup(true, cx);
            })
        });
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        cx.update(|_, cx| assert!(editor.read(cx).link_preview.popup.is_some()));
        cx.update(|_, cx| editor.update(cx, |e, cx| e.hover_link_popup(false, cx)));
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        cx.update(|_, cx| assert!(editor.read(cx).link_preview.popup.is_none()));
    }

    #[gpui_kit::test]
    fn preview_renderers_decorate_missing_links_and_open_from_real_hover(cx: &mut TestAppContext) {
        for source in [
            "[[missing]]\n",
            "> [!note] Links\n> [[missing]]\n",
            "==highlight== [[missing]]\n",
            "| links |\n| --- |\n| ==highlight== [[missing]] |\n",
        ] {
            cx.update(gpui_kit::init);
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("a.md"), source).unwrap();
            let vault = Arc::new(aero_core::Vault::open(dir.path()).unwrap());
            let path = aero_core::VaultPath::new("a.md").unwrap();
            let (doc, stamp) = vault.read(&path).unwrap();
            let (editor, visual) = cx.add_window_view(|_, cx| Editor::new(vault, path, doc, stamp, cx));
            visual.update(|_, cx| {
                editor.update(cx, |e, cx| {
                    e.link_preview.resolver = Some(aero_core::links::Resolver::default());
                    cx.notify();
                })
            });
            visual.run_until_parked();
            let position = visual.update(|window, cx| {
                use gpui_kit::component::ActiveTheme as _;
                window.draw(cx).clear(cx);
                let color = cx.theme().muted_foreground;
                let scale = window.scale_factor();
                let quad = window
                    .painted_quads()
                    .into_iter()
                    .find(|q| q.background.as_solid() == Some(color) && q.bounds.size.height.0 <= scale * 1.1)
                    .expect("missing links must paint dashed underlines in every preview renderer");
                point(px(quad.bounds.left().0 / scale + 1.), px(quad.bounds.top().0 / scale - 4.))
            });
            visual.update(|window, cx| window.simulate_mouse_move(position, cx));
            visual.run_until_parked();
            visual.executor().advance_clock(Duration::from_millis(500));
            visual.run_until_parked();
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                assert!(editor.read(cx).link_preview.popup.as_ref().unwrap().body.contains("不存在"));
            });
        }
    }

    #[gpui_kit::test]
    fn local_alias_changes_cancel_hover_and_preview_uses_unsaved_document(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "disk body\n").unwrap();
        let vault = Arc::new(aero_core::Vault::open(dir.path()).unwrap());
        let path = aero_core::VaultPath::new("a.md").unwrap();
        let (doc, stamp) = vault.read(&path).unwrap();
        let (editor, cx) = cx.add_window_view(|_, cx| Editor::new(vault, path, doc, stamp, cx));
        let bounds = Bounds::from_corners(point(px(20.), px(20.)), point(px(100.), px(45.)));
        let url = aero_core::links::encode_note_url("", None).into();
        cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                e.link_preview.resolver = Some(aero_core::links::Resolver::default());
                e.hover_link(&url, true, bounds, window, cx);
                e.doc
                    .edit(aero_core::Edit::replace(
                        0..e.doc.len(),
                        "---\naliases: local-name\n---\n\nunsaved body\n",
                    ))
                    .unwrap();
                e.after_doc_change(cx);
                e.save_task = None;
                assert!(e.link_preview.hovered.is_none());
                assert!(matches!(
                    e.link_preview.resolver.as_ref().unwrap().resolve("local-name", None, ""),
                    aero_core::links::Resolution::Resolved { .. }
                ));
                e.hover_link(&url, true, bounds, window, cx);
            })
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(editor.read(cx).link_preview.popup.as_ref().unwrap().body, "unsaved body")
        });
        assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), "disk body\n");
    }

    #[gpui_kit::test]
    fn link_preview_lifecycle_cancels_pending_results(cx: &mut TestAppContext) {
        for action in ["focus", "reload", "rename", "picker", "escape", "browse-escape", "delete"] {
            cx.update(gpui_kit::init);
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("a.md"), "---\naliases: original\n---\n\nbody\n").unwrap();
            let vault = Arc::new(aero_core::Vault::open(dir.path()).unwrap());
            let path = aero_core::VaultPath::new("a.md").unwrap();
            let (doc, stamp) = vault.read(&path).unwrap();
            let (editor, visual) = cx.add_window_view(|_, cx| Editor::new(vault, path, doc, stamp, cx));
            let bounds = Bounds::from_corners(point(px(20.), px(20.)), point(px(100.), px(45.)));
            let url = aero_core::links::encode_note_url("", None).into();
            visual.update(|window, cx| editor.update(cx, |e, cx| {
                e.link_preview.resolver = Some(aero_core::links::Resolver::default());
                e.update_local_link_names();
                e.hover_link(&url, true, bounds, window, cx);
                match action {
                    "focus" => e.focus_at(e.doc.len() - 2, None, window, cx),
                    "reload" => {
                        e.reload(b"---\naliases: replacement\n---\n\nnew body\n", stamp, cx);
                        let resolver = e.link_preview.resolver.as_ref().unwrap();
                        assert!(matches!(resolver.resolve("original", None, ""), aero_core::links::Resolution::Unresolved));
                        assert!(matches!(resolver.resolve("replacement", None, ""), aero_core::links::Resolution::Resolved { .. }));
                    }
                    "rename" => {
                        e.set_path(aero_core::VaultPath::new("new.md").unwrap(), cx);
                        let resolver = e.link_preview.resolver.as_ref().unwrap();
                        assert!(matches!(resolver.resolve("a", None, ""), aero_core::links::Resolution::Unresolved));
                        assert!(matches!(resolver.resolve("original", None, ""), aero_core::links::Resolution::Resolved { path, .. } if path.as_str() == "new.md"));
                    }
                    "picker" => e.set_picker_open(true, window, cx),
                    "escape" => e.on_escape(&gpui_kit::base::input::Escape, window, cx),
                    "browse-escape" => e.on_blur_editor(&crate::actions::BlurEditor, window, cx),
                    "delete" => e.mark_deleted(cx),
                    _ => unreachable!(),
                }
                assert!(e.link_preview.hovered.is_none(), "{action} must cancel hover");
            }));
            visual.run_until_parked();
            visual.executor().advance_clock(Duration::from_millis(600));
            visual.run_until_parked();
            visual.update(|_, cx| {
                assert!(editor.read(cx).link_preview.popup.is_none(), "{action} must reject delayed results")
            });
        }
    }

    #[gpui_kit::test]
    fn link_preview_cancels_when_window_or_scroll_changes(cx: &mut TestAppContext) {
        for change in ["resize", "scroll", "deactivate"] {
            cx.update(gpui_kit::init);
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("a.md"), "paragraph\n\n".repeat(80)).unwrap();
            let vault = Arc::new(aero_core::Vault::open(dir.path()).unwrap());
            let path = aero_core::VaultPath::new("a.md").unwrap();
            let (doc, stamp) = vault.read(&path).unwrap();
            let (editor, visual) = cx.add_window_view(|_, cx| Editor::new(vault, path, doc, stamp, cx));
            visual.update(|window, cx| {
                window.activate_window();
                window.draw(cx).clear(cx);
            });
            visual.run_until_parked();
            visual.update(|window, _| assert!(window.is_window_active()));
            let bounds = Bounds::from_corners(point(px(20.), px(20.)), point(px(100.), px(45.)));
            let url = aero_core::links::encode_note_url("", None).into();
            visual
                .update(|window, cx| editor.update(cx, |e, cx| e.hover_link(&url, true, bounds, window, cx)));
            visual.run_until_parked();
            visual.executor().advance_clock(Duration::from_millis(500));
            visual.run_until_parked();
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                assert!(editor.read(cx).link_preview.popup.is_some());
            });
            match change {
                "resize" => visual.simulate_resize(gpui_kit::size(px(720.), px(600.))),
                "scroll" => visual.update(|_, cx| {
                    editor.update(cx, |e, cx| {
                        e.scroll.set_offset(point(px(0.), px(-60.)));
                        cx.notify();
                    })
                }),
                "deactivate" => visual.deactivate_window(),
                _ => unreachable!(),
            }
            visual.run_until_parked();
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                assert!(editor.read(cx).link_preview.popup.is_none(), "{change} must close the popup");
            });
        }
    }

    #[gpui_kit::test]
    fn local_aliases_follow_restored_files_and_save_as(cx: &mut TestAppContext) {
        use aero_core::links::{Resolution, Resolver};
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("a.md");
        let parked = dir.path().join("parked");
        std::fs::write(&original, "---\naliases: local-name\n---\n\nbody\n").unwrap();
        let vault = Arc::new(aero_core::Vault::open(dir.path()).unwrap());
        let path = aero_core::VaultPath::new("a.md").unwrap();
        let (doc, stamp) = vault.read(&path).unwrap();
        let (editor, visual) = cx.add_window_view(|_, cx| Editor::new(vault, path, doc, stamp, cx));
        visual.update(|_, cx| {
            editor.update(cx, |e, _| {
                e.link_preview.resolver = Some(Resolver::new());
                e.update_local_link_names();
            })
        });
        std::fs::rename(&original, &parked).unwrap();
        visual.update(|_, cx| {
            editor.update(cx, |e, cx| {
                e.check_external(cx);
                assert!(e.is_deleted());
                assert!(matches!(
                    e.link_preview.resolver.as_ref().unwrap().resolve("local-name", None, ""),
                    Resolution::Unresolved
                ));
            })
        });
        std::fs::rename(&parked, &original).unwrap();
        visual.update(|_, cx| editor.update(cx, |e, cx| {
            e.check_external(cx);
            assert!(!e.is_deleted());
            assert!(matches!(e.link_preview.resolver.as_ref().unwrap().resolve("local-name", None, ""), Resolution::Resolved { path, .. } if path.as_str() == "a.md"));
        }));
        std::fs::rename(&original, &parked).unwrap();
        visual.update(|_, cx| editor.update(cx, |e, cx| {
            e.check_external(cx);
            e.save_as(aero_core::VaultPath::new("recovered.md").unwrap(), cx).unwrap();
            let resolver = e.link_preview.resolver.as_ref().unwrap();
            assert!(matches!(resolver.resolve("a", None, ""), Resolution::Unresolved));
            assert!(matches!(resolver.resolve("local-name", None, ""), Resolution::Resolved { path, .. } if path.as_str() == "recovered.md"));
        }));
        std::fs::rename(dir.path().join("recovered.md"), dir.path().join("parked-recovered")).unwrap();
        visual.update(|_, cx| {
            editor.update(cx, |e, cx| {
                e.check_external(cx);
                assert!(e.is_deleted());
                e.restore_deleted(cx);
            })
        });
        visual.run_until_parked();
        visual.update(|_, cx| {
            let e = editor.read(cx);
            assert!(dir.path().join("recovered.md").is_file());
            assert!(matches!(e.link_preview.resolver.as_ref().unwrap().resolve("local-name", None, ""), Resolution::Resolved { path, .. } if path.as_str() == "recovered.md"));
        });
    }

    #[gpui_kit::test]
    fn link_preview_rejects_old_hover_leave_and_opens_latest_fragment(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "## First\nold body\n\n## Second\nnew body\n").unwrap();
        let vault = Arc::new(aero_core::Vault::open(dir.path()).unwrap());
        let path = aero_core::VaultPath::new("a.md").unwrap();
        let (doc, stamp) = vault.read(&path).unwrap();
        let (editor, cx) = cx.add_window_view(|_, cx| Editor::new(vault, path, doc, stamp, cx));
        let bounds = Bounds::from_corners(point(px(20.), px(20.)), point(px(100.), px(45.)));
        let first = aero_core::links::encode_note_url("", Some("First")).into();
        let second = aero_core::links::encode_note_url("", Some("Second")).into();
        cx.update(|window, cx| editor.update(cx, |e, cx| e.hover_link(&first, true, bounds, window, cx)));
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(499));
        cx.run_until_parked();
        cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                e.hover_link(&second, true, bounds, window, cx);
                e.hover_link(&first, false, bounds, window, cx);
            })
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        cx.update(|_, cx| assert!(editor.read(cx).link_preview.popup.is_none()));
        cx.executor().advance_clock(Duration::from_millis(499));
        cx.run_until_parked();
        cx.update(|_, cx| {
            let popup = editor.read(cx).link_preview.popup.as_ref().unwrap();
            assert!(popup.body.contains("new body"));
            assert!(!popup.body.contains("old body"));
        });
    }

    #[gpui_kit::test]
    fn missing_and_ambiguous_previews_are_distinct_and_never_create_files(cx: &mut TestAppContext) {
        use aero_core::links::Resolver;
        use gpui_kit::base::text::LinkUnderline;
        use gpui_kit::component::ActiveTheme as _;
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        for name in ["a", "b"] {
            std::fs::write(dir.path().join(format!("{name}.md")), "---\naliases: shared\n---\n\nbody\n")
                .unwrap();
        }
        let vault = Arc::new(aero_core::Vault::open(dir.path()).unwrap());
        let path = aero_core::VaultPath::new("a.md").unwrap();
        let (doc, stamp) = vault.read(&path).unwrap();
        let mut resolver = Resolver::new();
        for path in vault.notes().unwrap() {
            resolver.insert(path, None, &["shared".into()]);
        }
        let (editor, visual) = cx.add_window_view(|_, cx| Editor::new(vault.clone(), path, doc, stamp, cx));
        visual.update(|_, cx| editor.update(cx, |e, _| e.link_preview.resolver = Some(resolver)));
        let bounds = Bounds::from_corners(point(px(20.), px(20.)), point(px(100.), px(45.)));
        for (target, expected) in [("missing", "不存在"), ("shared", "完整路径消歧"), ("a", "body")]
        {
            let url: gpui_kit::SharedString = aero_core::links::encode_note_url(target, None).into();
            visual.update(|window, cx| {
                editor.update(cx, |e, cx| {
                    let style = e.preview_link_style(&url, cx);
                    assert_eq!(
                        style.underline,
                        if target == "a" { LinkUnderline::Solid } else { LinkUnderline::Dashed }
                    );
                    assert_eq!(
                        style.color,
                        Some(match target {
                            "missing" => cx.theme().muted_foreground,
                            "shared" => cx.theme().warning,
                            _ => cx.theme().link,
                        })
                    );
                    e.hover_link(&url, true, bounds, window, cx);
                })
            });
            visual.run_until_parked();
            visual.executor().advance_clock(Duration::from_millis(500));
            visual.run_until_parked();
            visual.update(|_, cx| {
                assert!(editor.read(cx).link_preview.popup.as_ref().unwrap().body.contains(expected))
            });
        }
        assert_eq!(vault.notes().unwrap().len(), 2);
    }

    #[test]
    fn note_excerpt_omits_front_matter_and_limits_unicode_characters() {
        let doc = Document::from_text(&format!(
            "---\ntitle: secret\naliases: [hidden]\n---\n\n{}",
            "中😀".repeat(200)
        ));
        let text = excerpt(&doc, None).unwrap();
        assert_eq!(text.chars().count(), 300);
        assert!(!text.contains("secret"));
        assert!(!text.contains("hidden"));
    }

    #[test]
    fn heading_excerpt_stops_at_the_next_peer_but_keeps_subheadings() {
        let doc = Document::from_text(
            "# Intro\nbefore\n\n## **目标**\ninside\n\n### Child\nnested\n\n## After\noutside\n",
        );
        let text = excerpt(&doc, Some("目标")).unwrap();
        assert!(text.contains("inside"));
        assert!(text.contains("nested"));
        assert!(!text.contains("before"));
        assert!(!text.contains("outside"));
        assert!(excerpt(&doc, Some("missing")).is_err());
    }

    #[test]
    fn block_excerpt_is_exact_and_rejects_duplicate_ids() {
        let doc = Document::from_text("first ^one\n\nsecond ^two\n\nthird\n");
        let text = excerpt(&doc, Some("^two")).unwrap();
        assert!(text.contains("second"));
        assert!(!text.contains("first"));
        assert!(!text.contains("third"));
        assert!(excerpt(&doc, Some("^missing")).is_err());
        let duplicate = Document::from_text("first ^same\n\nsecond ^same\n");
        assert!(excerpt(&duplicate, Some("^same")).is_err());
    }

    #[test]
    fn nested_heading_excerpt_does_not_escape_its_container() {
        let doc = Document::from_text("> ## Nested\n> inside\n\noutside\n");
        let text = excerpt(&doc, Some("Nested")).unwrap();
        assert!(text.contains("inside"));
        assert!(!text.contains("outside"));
    }
}
