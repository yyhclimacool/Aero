use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use aero_core::rich_content::{mermaid_source, pdf_embed, pdf_source_with_page};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;

use super::*;
use crate::actions::{OpenPdf, PdfNextPage, PdfPreviousPage};
use crate::render::{Attachment, State};

const WIDTH: u32 = 680;
const DIAGRAM_DEBOUNCE: Duration = Duration::from_millis(300);

#[derive(Default)]
pub(super) struct RichUi {
    preview: Option<DiagramPreview>,
    preview_task: Option<Task<()>>,
    generation: u64,
    pub export_menu: Option<BlockId>,
    export_task: Option<Task<()>>,
    export_cancel: Option<Arc<AtomicBool>>,
    export_result: Option<(BlockId, Result<VaultPath, String>)>,
    pub pdf_readers: RefCell<HashMap<BlockId, Entity<super::pdf_reader::PdfReader>>>,
}

struct DiagramPreview {
    block: BlockId,
    requested: String,
    shown: Option<String>,
}

impl RichUi {
    pub fn export_in_progress(&self) -> bool {
        self.export_cancel.as_ref().is_some_and(|cancel| !cancel.load(Ordering::Acquire))
    }

    pub fn clear_preview(&mut self) {
        self.preview_task = None;
        self.preview = None;
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn cancel_export(&mut self) {
        if let Some(cancel) = self.export_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        self.export_task = None;
        self.export_menu = None;
        self.export_result = None;
    }
}

impl Drop for RichUi {
    fn drop(&mut self) {
        self.cancel_export();
    }
}

impl Editor {
    /// Only the settled source is ever sent to the rendering service while editing.
    pub(super) fn sync_diagram_preview(&mut self, immediate: bool, cx: &mut Context<Self>) {
        let current = self.focused.as_ref().and_then(|f| {
            let id = f.id?;
            let source = self.doc.slice(f.range.clone()).to_string();
            mermaid_source(&source)?;
            Some((id, source))
        });
        let Some((block, source)) = current else {
            if self.rich.preview.is_some() {
                self.rich.clear_preview();
            }
            return;
        };
        if self.rich.preview.as_ref().is_some_and(|p| p.block == block && p.requested == source) {
            return;
        }
        let shown = self.rich.preview.as_ref().filter(|p| p.block == block).and_then(|p| p.shown.clone());
        self.rich.clear_preview();
        self.rich.preview = Some(DiagramPreview {
            block,
            requested: source.clone(),
            shown: if immediate { Some(source.clone()) } else { shown },
        });
        if immediate {
            return;
        }
        let generation = self.rich.generation;
        self.rich.preview_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DIAGRAM_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                if this.rich.generation != generation {
                    return;
                }
                if let Some(preview) = &mut this.rich.preview
                    && preview.block == block
                    && preview.requested == source
                {
                    preview.shown = Some(source);
                    cx.notify();
                }
            });
        }));
    }

    pub(super) fn render_diagram_preview(&self, block: BlockId, cx: &mut Context<Self>) -> AnyElement {
        let body = self
            .rich
            .preview
            .as_ref()
            .filter(|p| p.block == block)
            .and_then(|p| p.shown.as_deref())
            .map(|source| self.render_diagram(source, block, false, cx))
            .unwrap_or_else(|| div().text_xs().child("等待输入完成…").into_any_element());
        div().w_full().debug_selector(|| "mermaid-live-preview".into()).child(body).into_any_element()
    }

    pub(super) fn render_diagram(
        &self,
        source: &str,
        block: BlockId,
        interactive: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(body) = mermaid_source(source) else { return rich_source("mermaid-source", source, cx) };
        let Some(service) = &self.images else { return rich_source("mermaid-source", source, cx) };
        let dark = cx.theme().mode.is_dark();
        let font = cx.theme().font_family.clone();
        let state = service.update(cx, |s, cx| s.diagram(&body, WIDTH, self.scale, dark, &font, cx));
        let image = match state {
            State::Ready { image, pixels, .. } => {
                let width = pixels.width as f32 / self.scale;
                let height = pixels.height as f32 / self.scale;
                div()
                    .id(("mermaid", block.0))
                    .debug_selector(|| "mermaid-block".into())
                    .w_full()
                    .max_w(px(width))
                    .aspect_ratio(width / height)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        if event.is_right_click() {
                            return;
                        }
                        cx.stop_propagation();
                        if !interactive {
                            return;
                        }
                        this.select_block_by_id(block, window, cx);
                        if matches!(event, ClickEvent::Mouse(click) if click.down.click_count >= 2) {
                            this.rich.export_menu = Some(block);
                            cx.notify();
                        }
                    }))
                    .child(
                        img(ImageSource::Render(image)).debug_selector(|| "mermaid-image".into()).size_full(),
                    )
                    .into_any_element()
            }
            State::Pending { pixels } => {
                rich_placeholder("mermaid-placeholder", WIDTH, pixels, self.scale, cx)
            }
            State::Failed(error) => self.rich_error("mermaid", source, error, cx),
        };
        let mut result = v_flex().w_full().gap_1().child(image);
        if interactive && self.rich.export_menu == Some(block) {
            result = result.child(
                h_flex()
                    .debug_selector(|| "mermaid-export-menu".into())
                    .gap_1()
                    .children(
                        [("mermaid-export-png", "导出 PNG", true), ("mermaid-export-svg", "导出 SVG", false)]
                            .map(|(id, label, png)| {
                                div()
                                    .debug_selector(move || id.into())
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                    .child(Button::new(id).ghost().xsmall().label(label).on_click(
                                        cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.export_diagram(block, png, window, cx);
                                        }),
                                    ))
                            }),
                    )
                    .child(Button::new("mermaid-export-close").ghost().xsmall().label("取消").on_click(
                        cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.rich.export_menu = None;
                            cx.notify();
                        }),
                    )),
            );
        }
        if let Some((id, status)) = &self.rich.export_result
            && *id == block
            && interactive
        {
            result = result.child(
                div()
                    .debug_selector(|| "mermaid-export-result".into())
                    .text_xs()
                    .text_color(if status.is_ok() { cx.theme().muted_foreground } else { cx.theme().danger })
                    .child(match status {
                        Ok(path) => format!("已导出：{path}"),
                        Err(error) => error.clone(),
                    }),
            );
        }
        result.into_any_element()
    }

    pub(super) fn export_diagram(
        &mut self,
        block: BlockId,
        png: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.rich.export_menu != Some(block) {
            return;
        }
        let Some(unit) = self.units.iter().find(|u| u.id == block) else { return };
        let Some(source) = mermaid_source(&self.doc.slice(unit.range.clone()).to_string()) else { return };
        self.rich.cancel_export();
        let cancel = Arc::new(AtomicBool::new(false));
        self.rich.export_cancel = Some(cancel.clone());
        let (vault, note) = (self.vault.clone(), self.path.clone());
        let dark = cx.theme().mode.is_dark();
        let font = cx.theme().font_family.to_string();
        let text_system = cx.text_system().clone();
        self.rich.export_task = Some(cx.spawn_in(window, async move |this, cx| {
            let rendered = cx
                .background_spawn(async move {
                    crate::render::export_diagram(&source, dark, &font, text_system, png)
                })
                .await;
            // Validate on the UI thread after rendering, before scheduling any disk write.
            let current = this
                .update(cx, |this, _| {
                    Arc::ptr_eq(&this.vault, &vault) && this.path == note && !cancel.load(Ordering::Acquire)
                })
                .unwrap_or(false);
            if !current {
                return;
            }
            let result = match rendered {
                Err(error) => Err(error),
                Ok(bytes) => {
                    let (vault, note, cancel) = (vault.clone(), note.clone(), cancel.clone());
                    cx.background_spawn(async move {
                        if cancel.load(Ordering::Acquire) {
                            return Err("导出已取消".into());
                        }
                        let extension = if png { "png" } else { "svg" };
                        let path =
                            aero_core::attachments::import_path(&vault, &note, Some("diagram"), extension)
                                .ok_or_else(|| "无法分配图表附件路径".to_owned())?;
                        if cancel.load(Ordering::Acquire) {
                            return Err("导出已取消".into());
                        }
                        vault.create_bytes(&path, &bytes).map_err(|e| e.to_string())?;
                        Ok(path)
                    })
                    .await
                }
            };
            let _ = this.update_in(cx, |this, window, cx| {
                if !Arc::ptr_eq(&this.vault, &vault) || this.path != note || cancel.load(Ordering::Acquire) {
                    return;
                }
                match &result {
                    Ok(path) => {
                        if let Some(service) = &this.images {
                            service.update(cx, |s, cx| s.invalidate(std::slice::from_ref(path), cx));
                        }
                        cx.emit(EditorEvent::PathsTouched(vec![path.clone()]));
                    }
                    Err(error) => {
                        window.push_notification(Notification::error(error.clone()).title("导出图表"), cx)
                    }
                }
                cancel.store(true, Ordering::Release);
                this.rich.export_result = Some((block, result));
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Root paragraphs can edit their fragment. Nested previews are deliberately read-only.
    pub(super) fn render_pdf(
        &self,
        source: &str,
        block: BlockId,
        origin: Option<&VaultPath>,
        interactive: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(embed) = pdf_embed(source) else { return rich_source("pdf-source", source, cx) };
        let page = match embed.page {
            Ok(page) => page,
            Err(error) => return self.rich_error("pdf", source, error.into(), cx),
        };
        let Some(service) = &self.images else { return rich_source("pdf-source", source, cx) };
        let origin = origin.unwrap_or(&self.path);
        let width = embed.width.unwrap_or(WIDTH).clamp(24, 4096);
        if interactive && self.pdf_root_source(block).as_deref() == Some(source) {
            match service.update(cx, |s, cx| s.attachment(&embed.target, origin, cx)) {
                Attachment::Found(path) => {
                    let mut readers = self.rich.pdf_readers.borrow_mut();
                    let reader = readers.entry(block).or_insert_with(|| {
                        let editor = cx.weak_entity();
                        cx.new(|cx| {
                            super::pdf_reader::PdfReader::new(
                                editor,
                                block,
                                path.clone(),
                                source.to_owned(),
                                page,
                                width,
                                service.clone(),
                                cx,
                            )
                        })
                    });
                    reader.update(cx, |reader, cx| reader.sync(path, source.to_owned(), page, width, cx));
                    return reader.clone().into_any_element();
                }
                Attachment::Loading => {
                    return rich_placeholder("pdf-placeholder", width, None, self.scale, cx);
                }
                Attachment::Missing => {
                    return self.rich_error("pdf", source, format!("找不到附件 {}", embed.target).into(), cx);
                }
                Attachment::Ambiguous => {
                    return self.rich_error(
                        "pdf",
                        source,
                        format!("有多个同名附件 {}，请写出所在目录", embed.target).into(),
                        cx,
                    );
                }
            }
        }
        let state = service.update(cx, |s, cx| match s.attachment(&embed.target, origin, cx) {
            // 显示始终受正文列宽约束，不为 |4096 的预设分配不可见的 HiDPI 像素。
            Attachment::Found(path) => s.pdf(path, page, width.min(WIDTH), self.scale, cx),
            Attachment::Loading => State::Pending { pixels: None },
            Attachment::Missing => State::Failed(format!("找不到附件 {}", embed.target).into()),
            Attachment::Ambiguous => {
                State::Failed(format!("有多个同名附件 {}，请写出所在目录", embed.target).into())
            }
        });
        match state {
            State::Ready { image, pixels, pages } => {
                let logical = width as f32;
                let height = logical * pixels.height as f32 / pixels.width as f32;
                let (target, origin) = (embed.target.clone(), origin.clone());
                let bar = h_flex().gap_2().child(
                    div()
                        .debug_selector(move || format!("pdf-page-{page}-of-{}", pages.unwrap_or(0)))
                        .text_xs()
                        .child(format!("{page} / {}", pages.map_or("?".into(), |n| n.to_string()))),
                );
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(bar)
                    .child(
                        div()
                            .id(("pdf", block.0))
                            .debug_selector(|| "pdf-block".into())
                            .w_full()
                            .max_w(px(logical))
                            .aspect_ratio(logical / height)
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                                if event.is_right_click() {
                                    return;
                                }
                                cx.stop_propagation();
                                if matches!(event, ClickEvent::Mouse(click) if click.down.click_count >= 2) {
                                    this.open_pdf_target(&target, &origin, window, cx);
                                }
                            }))
                            .child(
                                img(ImageSource::Render(image))
                                    .debug_selector(|| "pdf-image".into())
                                    .size_full(),
                            ),
                    )
                    .into_any_element()
            }
            State::Pending { pixels } => rich_placeholder("pdf-placeholder", width, pixels, self.scale, cx),
            State::Failed(error) => self.rich_error("pdf", source, error, cx),
        }
    }

    fn pdf_root_source(&self, block: BlockId) -> Option<String> {
        let b = self.doc.blocks().roots.iter().find(|b| b.id == block && b.kind == BlockKind::Paragraph)?;
        let source = self.doc.slice(self.doc.content_range(b)).to_string();
        pdf_embed(&source)?;
        Some(source)
    }

    fn selected_pdf(&self) -> Option<BlockId> {
        if self.focused.is_some() {
            return None;
        }
        let selection = self.selection?;
        if selection.first() != selection.last() {
            return None;
        }
        let block = self.units.get(selection.head)?.id;
        self.pdf_root_source(block)?;
        Some(block)
    }

    pub(super) fn commit_pdf_page(
        &mut self,
        block: BlockId,
        page: u32,
        total: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if page == 0 || page > total {
            return false;
        }
        let Some(source) = self.pdf_root_source(block) else { return false };
        let Some(embed) = pdf_embed(&source) else { return false };
        let Some(service) = &self.images else { return false };
        let info = service.update(cx, |s, cx| match s.attachment(&embed.target, &self.path, cx) {
            Attachment::Found(path) => s.pdf_info(path, cx),
            _ => crate::render::PdfInfoState::Pending,
        });
        let crate::render::PdfInfoState::Ready(info) = info else { return false };
        if info.pages.len() != total as usize {
            return false;
        }
        let Some(replacement) = pdf_source_with_page(&source, page) else { return false };
        let Some(unit) = self.units.iter().find(|u| u.id == block) else { return false };
        let edit = minimal_edit(unit.range.start, &source, &replacement);
        self.select_block_by_id(block, window, cx);
        let Some(edit) = edit else { return true };
        if let Err(error) = self.doc.edit(edit) {
            log::warn!("PDF 翻页失败：{error}");
            return false;
        }
        self.last_edit_at = None;
        self.after_doc_change(cx);
        true
    }

    fn step_pdf(&mut self, block: BlockId, delta: i32, window: &mut Window, cx: &mut Context<Self>) {
        if self.focused.is_some() {
            return;
        }
        let Some(reader) = self.rich.pdf_readers.borrow().get(&block).cloned() else { return };
        let Some(total) = reader.read(cx).page_count() else { return };
        let Some(next) = reader.read(cx).current_page().checked_add_signed(delta) else { return };
        if self.commit_pdf_page(block, next, total, window, cx) {
            reader.update(cx, |reader, cx| reader.go_to_page(next, window, cx));
        }
    }

    pub(super) fn on_pdf_previous(
        &mut self,
        _: &PdfPreviousPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(block) = self.selected_pdf() {
            self.step_pdf(block, -1, window, cx);
        } else {
            cx.propagate();
        }
    }

    pub(super) fn on_pdf_next(&mut self, _: &PdfNextPage, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(block) = self.selected_pdf() {
            self.step_pdf(block, 1, window, cx);
        } else {
            cx.propagate();
        }
    }

    pub(super) fn on_open_pdf(&mut self, _: &OpenPdf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(block) = self.selected_pdf() else {
            cx.propagate();
            return;
        };
        let Some(embed) = self.pdf_root_source(block).and_then(|s| pdf_embed(&s)) else { return };
        self.open_pdf_target(&embed.target, &self.path.clone(), window, cx);
    }

    fn open_pdf_target(&self, target: &str, origin: &VaultPath, window: &mut Window, cx: &mut Context<Self>) {
        let Some(service) = &self.images else { return };
        let Attachment::Found(path) = service.update(cx, |s, cx| s.attachment(target, origin, cx)) else {
            return;
        };
        self.open_pdf_reader_source(&path, window, cx);
    }

    pub(super) fn open_pdf_reader_source(
        &self,
        path: &VaultPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = path.clone();
        let (vault, note) = (self.vault.clone(), self.path.clone());
        cx.spawn_in(window, async move |this, cx| {
            let v = vault.clone();
            let file = cx
                .background_spawn(async move {
                    aero_core::attachments::available(&v, &path).then(|| v.fs_path(&path))
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if !Arc::ptr_eq(&this.vault, &vault) || this.path != note {
                    return;
                }
                if let Some(file) = file {
                    cx.open_url(&crate::paths::file_url(&file));
                } else {
                    window.push_notification(Notification::error("PDF 文件已不可用").title("打开 PDF"), cx);
                }
            });
        })
        .detach();
    }

    fn rich_error(
        &self,
        kind: &'static str,
        source: &str,
        error: SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selector = if kind == "pdf" { "pdf-source" } else { "mermaid-source" };
        let service = self.images.clone();
        let detail = error.clone();
        v_flex()
            .w_full()
            .gap_1()
            .child(rich_source(selector, source, cx))
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .child(
                        div()
                            .id(SharedString::from(format!("{kind}-error")))
                            .debug_selector(move || format!("{kind}-error"))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(cx.theme().danger)
                            .tooltip(move |window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new(detail.clone()).build(window, cx)
                            })
                            .child(error),
                    )
                    .child(
                        div()
                            .debug_selector(move || format!("{kind}-retry"))
                            .flex_none()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(
                                Button::new(SharedString::from(format!("{kind}-retry")))
                                    .ghost()
                                    .xsmall()
                                    .label("重试")
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        if let Some(service) = &service {
                                            service.update(cx, |s, cx| s.retry(cx));
                                        }
                                    }),
                            ),
                    ),
            )
            .into_any_element()
    }
}

fn rich_source(selector: &'static str, source: &str, cx: &App) -> AnyElement {
    div()
        .w_full()
        .debug_selector(move || selector.into())
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(px(13.))
        .text_color(cx.theme().muted_foreground)
        .child(source.to_owned())
        .into_any_element()
}

fn rich_placeholder(
    selector: &'static str,
    width: u32,
    pixels: Option<Size<u32>>,
    scale: f32,
    cx: &App,
) -> AnyElement {
    let width = pixels.map_or(width.min(WIDTH) as f32, |p| p.width as f32 / scale);
    let height = pixels.map_or(width * 0.5625, |p| p.height as f32 / scale);
    div()
        .debug_selector(move || selector.into())
        .w(px(width))
        .h(px(height))
        .max_w_full()
        .bg(cx.theme().muted)
        .rounded(px(6.))
        .into_any_element()
}
