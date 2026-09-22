//! 图片块：接上渲染服务（方案 9.9）与全屏看图（`ui_design.md` 5.6）。

use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::*;

use aero_core::{Edit, VaultPath};

use crate::actions::{CloseImageViewer, PasteAttachment};
use crate::render::{Attachment, RenderService, State};

fn remote_scheme(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once(':') else { return false };
    matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https" | "mailto" | "ftp")
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
}

/// 剪贴板位图的落地后缀；GPUI 在 macOS 上给的是 PNG / TIFF 等有限几种。
fn image_extension(image: &gpui_kit::Image) -> &'static str {
    match image.format {
        gpui_kit::ImageFormat::Png => "png",
        gpui_kit::ImageFormat::Jpeg => "jpg",
        gpui_kit::ImageFormat::Webp => "webp",
        gpui_kit::ImageFormat::Gif => "gif",
        gpui_kit::ImageFormat::Bmp => "bmp",
        gpui_kit::ImageFormat::Tiff => "tiff",
        gpui_kit::ImageFormat::Svg => "svg",
        gpui_kit::ImageFormat::Ico => "ico",
        // 其余（PNM 等）一律按 PNG 落地：剪贴板给的字节自带格式头，解码器按内容识别
        _ => "png",
    }
}

/// 全屏看图。自带焦点与 `ImageViewer` 键位上下文，`Space` / `Esc` 只在浮层里生效，不影响正文输入。
pub(super) struct ImageViewer {
    target: String,
    focus: FocusHandle,
}

impl super::Editor {
    /// 接上渲染服务；没接上时图片块按源码显示（无 vault 的测试上下文就是这样）。
    pub fn connect_images(&mut self, service: Entity<RenderService>, cx: &mut Context<Self>) {
        self.images_subscription = Some(cx.observe(&service, |_, _, cx| cx.notify()));
        self.images = Some(service);
        cx.notify();
    }

    /// `⌘V`：剪贴板里是位图或文件就导入到 `<note>.assets/` 并插入嵌入（`ui_design.md` 7.5）；
    /// 其余情况交还给文本控件按文字粘贴。
    pub(super) fn on_paste_attachment(
        &mut self,
        _: &PasteAttachment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = cx.read_from_clipboard() else {
            cx.propagate();
            return;
        };
        // 文字优先：从浏览器复制的图片往往同时带 HTML / 文字，用户要的是文字
        if item.entries().iter().any(|entry| matches!(entry, ClipboardEntry::String(_))) {
            cx.propagate();
            return;
        }
        let bitmaps: Vec<&gpui_kit::Image> = item
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                ClipboardEntry::Image(image) => Some(image),
                _ => None,
            })
            .collect();
        // Finder 里复制的文件：逐个导入，读不到的跳过并提示
        let files: Vec<std::path::PathBuf> = item
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                ClipboardEntry::ExternalPaths(paths) => Some(paths.paths().to_vec()),
                _ => None,
            })
            .flatten()
            .collect();
        if bitmaps.is_empty() && files.is_empty() {
            cx.propagate();
            return;
        }
        let mut pieces = Vec::new();
        let mut failures = Vec::new();
        for (extension, bytes) in
            bitmaps.into_iter().map(|image| (image_extension(image), image.bytes.clone())).collect::<Vec<_>>()
        {
            match self.import_attachment(None, extension, &bytes, cx) {
                Ok(text) => pieces.push(text),
                Err(error) => failures.push(error),
            }
        }
        self.import_files(&files, &mut pieces, &mut failures, cx);
        self.finish_import(pieces, failures, window, cx);
    }

    /// Finder 拖进编辑区：和 `⌘V` 的文件规则相同（`ui_design.md` 7.6 / 7.5）。
    pub(super) fn drop_external_files(
        &mut self,
        paths: &gpui_kit::ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut pieces = Vec::new();
        let mut failures = Vec::new();
        self.import_files(paths.paths(), &mut pieces, &mut failures, cx);
        self.finish_import(pieces, failures, window, cx);
    }

    /// 文件树拖进编辑区：插入 `[[笔记]]`；按住 ⌥ 插入 `![[笔记]]`。文件夹不接收。
    pub(super) fn drop_tree_note(
        &mut self,
        drag: &crate::file_tree::TreeDrag,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if drag.directory || drag.vault != self.vault.id() {
            return;
        }
        let bare = drag.path.as_str().strip_suffix(".md").unwrap_or(drag.path.as_str());
        let Some(target) = aero_core::links::escape_target(bare) else {
            window.push_notification(
                Notification::error(format!("笔记名 {bare} 无法写进链接")).title("插入链接"),
                cx,
            );
            return;
        };
        let text = if window.modifiers().alt { format!("![[{target}]]") } else { format!("[[{target}]]") };
        self.insert_at_cursor(&text, window, cx);
    }

    fn import_files(
        &mut self,
        files: &[std::path::PathBuf],
        pieces: &mut Vec<String>,
        failures: &mut Vec<String>,
        cx: &mut Context<Self>,
    ) {
        for file in files {
            if file.is_dir() {
                continue;
            }
            let name = file.file_name().map(|n| n.to_string_lossy().into_owned());
            let extension = file.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
            match std::fs::read(file) {
                Ok(bytes) => match self.import_attachment(name.as_deref(), &extension, &bytes, cx) {
                    Ok(text) => pieces.push(text),
                    Err(error) => failures.push(error),
                },
                Err(error) => failures.push(format!("{}：{error}", file.display())),
            }
        }
    }

    fn finish_import(
        &mut self,
        pieces: Vec<String>,
        failures: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for error in &failures {
            log::warn!("import attachment: {error}");
        }
        if let Some(error) = failures.first() {
            window.push_notification(Notification::error(error.clone()).title("导入附件"), cx);
        }
        if pieces.is_empty() {
            return;
        }
        self.insert_at_cursor(&pieces.join("\n\n"), window, cx);
    }

    /// 写入 `<note>.assets/`（create-only，不覆盖已有文件），返回要插入的源码。
    fn import_attachment(
        &mut self,
        name: Option<&str>,
        extension: &str,
        bytes: &[u8],
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        let file = aero_core::attachments::import_path(&self.vault, &self.path, name, extension)
            .ok_or_else(|| format!("无法为 {} 分配附件路径", self.path))?;
        self.vault.create_bytes(&file, bytes).map_err(|e| e.to_string())?;
        // 新文件立刻进附件目录，本次插入的嵌入当帧就能解析
        if let Some(service) = &self.images {
            service.update(cx, |service, cx| service.invalidate(&[file.clone()], cx));
        }
        let target = aero_core::links::escape_target(file.file_name())
            .ok_or_else(|| format!("文件名 {} 无法写进链接", file.file_name()))?;
        Ok(match aero_core::attachments::Kind::for_path(file.as_str()) {
            // 图片 / PDF 用嵌入；其他文件按设计插普通链接
            Some(_) => format!("![[{target}]]"),
            None => format!("[{}]({target})", file.file_name()),
        })
    }

    /// 在光标处插入源码，作为一个撤销组；不在编辑态时插到当前块之后。
    fn insert_at_cursor(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let range = match &self.focused {
            Some(focused) => {
                let selected = focused.textarea.read(cx).selected_range();
                (focused.range.start + selected.start)..(focused.range.start + selected.end)
            }
            None => {
                let end = self.doc.to_string().len();
                end..end
            }
        };
        let text = if self.focused.is_some() { text.to_owned() } else { format!("\n\n{text}\n") };
        let at = range.start + text.len();
        self.structural_edit(Edit::replace(range, text), at, window, cx);
    }

    /// 预览里点到的不是笔记链接：网页原样打开；相对路径按附件找真实文件，避免把 `notes.zip` 交给系统报 -50。
    pub(super) fn open_external_link(&self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        let url = url.trim();
        if remote_scheme(url) {
            cx.open_url(url);
            return;
        }
        if let Some(path) = self.local_file(url) {
            cx.open_url(&crate::paths::file_url(&path));
            return;
        }
        if url.split_once(':').is_some_and(|(scheme, _)| scheme.bytes().all(|b| b.is_ascii_alphabetic())) {
            cx.open_url(url);
            return;
        }
        window.push_notification(Notification::error(format!("找不到文件 {url}")).title("打开文件"), cx);
    }

    fn local_file(&self, url: &str) -> Option<std::path::PathBuf> {
        let target = url.trim().trim_matches(|c| c == '<' || c == '>');
        let target = percent_decode(target);
        let target = target.trim_start_matches("./");
        if target.is_empty() || target.contains("..") {
            return None;
        }
        let mut candidates = Vec::new();
        if let Some(name) = std::path::Path::new(&target).file_name() {
            if let Some(assets) = self.path.assets_dir() {
                candidates.push(format!("{}/{}", assets.as_str(), name.to_string_lossy()));
            }
        }
        let parent = self.path.parent();
        candidates.push(if parent.is_empty() { target.to_owned() } else { format!("{parent}/{target}") });
        candidates.into_iter().find_map(|rel| {
            let path = VaultPath::new(&rel).ok()?;
            let file = self.vault.fs_path(&path);
            file.is_file().then_some(file)
        })
    }

    /// 双击图片。
    pub(super) fn open_image_viewer(&mut self, target: String, window: &mut Window, cx: &mut Context<Self>) {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        self.viewer = Some(ImageViewer { target, focus });
        cx.notify();
    }

    pub(super) fn on_close_image_viewer(
        &mut self,
        _: &CloseImageViewer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.close_image_viewer(window, cx) {
            cx.propagate();
        }
    }

    /// 关掉浮层并把焦点还给编辑器；没开着就返回 false，让按键继续冒泡。
    pub(super) fn close_image_viewer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.viewer.take().is_none() {
            return false;
        }
        self.focus_handle.focus(window, cx);
        cx.notify();
        true
    }

    pub(super) fn render_image_viewer(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let viewer = self.viewer.as_ref()?;
        let service = self.images.clone()?;
        let viewport = window.viewport_size();
        let scale = window.scale_factor();
        let (max_width, max_height) = (f32::from(viewport.width) * 0.9, f32::from(viewport.height) * 0.9);
        let source = self.path.clone();
        let target = viewer.target.clone();
        let state = service.update(cx, |service, cx| match service.attachment(&target, &source, cx) {
            Attachment::Found(file) => service.image(file, max_width as u32, scale, cx),
            Attachment::Loading => State::Pending { pixels: None },
            // 看图期间附件被删 / 变歧义：浮层里说清楚，不静默变空白
            Attachment::Missing | Attachment::Ambiguous => State::Failed("附件已不在".into()),
        });
        let body: AnyElement = match state {
            State::Ready { image, pixels } => {
                let (width, height) = (pixels.width as f32 / scale, pixels.height as f32 / scale);
                // 等比缩放到视口内，小图不放大
                let fit = (max_width / width).min(max_height / height).min(1.);
                img(ImageSource::Render(image)).w(px(width * fit)).h(px(height * fit)).into_any_element()
            }
            State::Pending { .. } => {
                div().text_color(cx.theme().muted_foreground).child("正在载入…").into_any_element()
            }
            State::Failed(error) => div().text_color(cx.theme().danger).child(error).into_any_element(),
        };
        Some(
            deferred(
                anchored().position(point(px(0.), px(0.))).child(
                    div()
                        .id("image-viewer")
                        .debug_selector(|| "image-viewer".to_owned())
                        .track_focus(&viewer.focus)
                        .key_context("ImageViewer")
                        .on_action(cx.listener(Self::on_close_image_viewer))
                        .occlude()
                        .w(viewport.width)
                        .h(viewport.height)
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(hsla(0., 0., 0., 0.85))
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.close_image_viewer(window, cx);
                        }))
                        .child(body),
                ),
            )
            .into_any_element(),
        )
    }
}
