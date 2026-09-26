//! 图片块：接上渲染服务（方案 9.9）与全屏看图（`ui_design.md` 5.6）。

use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::*;

use std::ops::Range;
use std::path::PathBuf;

use aero_core::{Edit, Snapshot, Vault, VaultError, VaultPath};

use crate::actions::{CloseImageViewer, PasteAttachment};
use crate::render::{Attachment, RenderService, State};

/// Chain complete requests, not just writes: repeated pastes keep their order and each is one undo step.
#[derive(Default)]
pub(super) struct Imports {
    pending: usize,
    tail: Option<Task<()>>,
}

impl Imports {
    pub(super) fn is_pending(&self) -> bool {
        self.pending != 0
    }
}

enum ImportInput {
    File(PathBuf),
    Bitmap { extension: &'static str, bytes: Vec<u8> },
}

struct ImportSource {
    snapshot: Snapshot,
    path: VaultPath,
    epoch: u64,
}

#[derive(Default)]
struct Imported {
    paths: Vec<VaultPath>,
    pieces: Vec<String>,
    failures: Vec<String>,
}

/// All filesystem work, including metadata checks and collision allocation, runs on the worker.
fn import_inputs(vault: &Vault, note: &VaultPath, inputs: Vec<ImportInput>) -> Imported {
    let mut imported = Imported::default();
    for input in inputs {
        let result = match input {
            ImportInput::Bitmap { extension, bytes } => {
                import_attachment(vault, note, None, extension, &bytes)
            }
            ImportInput::File(file) => {
                let result = std::fs::metadata(&file).and_then(|metadata| {
                    if metadata.is_dir() {
                        return Ok(None);
                    }
                    if !metadata.is_file() {
                        return Err(std::io::Error::other("不是普通文件"));
                    }
                    std::fs::read(&file).map(Some)
                });
                match result {
                    Ok(None) => continue,
                    Ok(Some(bytes)) => {
                        let name = file.file_name().map(|n| n.to_string_lossy().into_owned());
                        let extension =
                            file.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
                        import_attachment(vault, note, name.as_deref(), &extension, &bytes)
                    }
                    Err(error) => Err(format!("{}：{error}", file.display())),
                }
            }
        };
        match result {
            Ok((path, text)) => {
                imported.paths.push(path);
                imported.pieces.push(text);
            }
            Err(error) => imported.failures.push(error),
        }
    }
    imported
}

fn import_attachment(
    vault: &Vault,
    note: &VaultPath,
    name: Option<&str>,
    extension: &str,
    bytes: &[u8],
) -> Result<(VaultPath, String), String> {
    for _ in 0..100 {
        let file = aero_core::attachments::import_path(vault, note, name, extension)
            .ok_or_else(|| format!("无法为 {note} 分配附件路径"))?;
        let text = attachment_text(note, &file, false)?;
        match vault.create_bytes(&file, bytes) {
            Ok(_) => return Ok((file, text)),
            // Another importer may win between allocation and create-only publication. Allocate again.
            Err(VaultError::AlreadyExists(_)) => continue,
            Err(VaultError::Io { source, .. }) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                continue;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Err(format!("无法为 {note} 分配附件路径"))
}

fn attachment_text(note: &VaultPath, file: &VaultPath, qualified: bool) -> Result<String, String> {
    let name = if qualified { file.as_str() } else { file.file_name() };
    let target =
        aero_core::links::escape_target(name).ok_or_else(|| format!("文件名 {name} 无法写进链接"))?;
    Ok(match aero_core::attachments::Kind::for_path(file.as_str()) {
        Some(_) => format!("![[{target}]]"),
        None => {
            let relative = relative_to_note(note, file.as_str());
            let destination =
                if relative.contains([' ', ')', '<', '>']) { format!("<{relative}>") } else { relative };
            format!("[[{target}]]({destination})")
        }
    })
}

/// Conservative mapping: an overlap invalidates the selection, never the user's replacement text.
/// A broad edit spanning several changes may collapse more than necessary, but cannot delete those changes.
fn map_import_range(range: Range<usize>, old: &str, new: &str) -> Range<usize> {
    let Some(edit) = super::text_ops::minimal_edit(0, old, new) else { return range };
    if edit.range.end <= range.start {
        let start = range.start - edit.range.len() + edit.text.len();
        let end = range.end - edit.range.len() + edit.text.len();
        start..end
    } else if edit.range.start > range.end || (edit.range.start == range.end && !range.is_empty()) {
        range
    } else {
        let after = edit.range.start + edit.text.len();
        after..after
    }
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
        let mut bitmaps = Vec::new();
        let mut files = Vec::new();
        for entry in item.entries {
            match entry {
                ClipboardEntry::Image(image) => {
                    bitmaps
                        .push(ImportInput::Bitmap { extension: image_extension(&image), bytes: image.bytes });
                }
                ClipboardEntry::ExternalPaths(paths) => {
                    files.extend(paths.paths().iter().cloned().map(ImportInput::File));
                }
                _ => {}
            }
        }
        if bitmaps.is_empty() && files.is_empty() {
            cx.propagate();
            return;
        }
        bitmaps.extend(files);
        self.start_import(bitmaps, window, cx);
    }

    /// Finder 拖进编辑区：和 `⌘V` 的文件规则相同（`ui_design.md` 7.6 / 7.5）。
    pub(super) fn drop_external_files(
        &mut self,
        paths: &gpui_kit::ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_import(paths.paths().iter().cloned().map(ImportInput::File).collect(), window, cx);
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

    fn import_source(&self) -> ImportSource {
        ImportSource { snapshot: self.doc.snapshot(), path: self.path.clone(), epoch: self.io.epoch }
    }

    fn start_import(&mut self, inputs: Vec<ImportInput>, window: &mut Window, cx: &mut Context<Self>) {
        if inputs.is_empty() {
            return;
        }
        let origin = self.import_source();
        let range = self.focused.as_ref().map_or_else(
            || self.doc.len()..self.doc.len(),
            |focused| {
                let selected = focused.textarea.read(cx).selected_range();
                focused.range.start + selected.start..focused.range.start + selected.end
            },
        );
        let padding = self.focused.is_none();
        let vault = self.vault.clone();
        // Retain the originating entity even when another tab replaces it in the window.
        let editor = cx.entity();
        let previous = self.imports.tail.take();
        self.imports.pending += 1;
        self.imports.tail = Some(cx.spawn_in(window, async move |_, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let path = origin.path.clone();
            let imported = cx.background_spawn(async move { import_inputs(&vault, &path, inputs) }).await;
            let mut imported = Some(imported);
            loop {
                let current = editor.update(cx, |this, _| this.import_source());
                let old = origin.snapshot.clone();
                let latest = current.snapshot.clone();
                let selection = range.clone();
                let same_version = origin.epoch == current.epoch && old.version() == latest.version();
                let mapped = cx
                    .background_spawn(async move {
                        if same_version {
                            selection
                        } else {
                            map_import_range(
                                selection,
                                &String::from(old.text()),
                                &String::from(latest.text()),
                            )
                        }
                    })
                    .await;
                let applied = editor.update_in(cx, |this, window, cx| {
                    if this.doc.version() != current.snapshot.version()
                        || this.io.epoch != current.epoch
                        || this.path != current.path
                    {
                        return false;
                    }
                    let mut imported = imported.take().unwrap();
                    // Workspace operations are guarded by is_pending. If an external repath bypasses
                    // that guard, report it rather than silently inserting relative links in another note.
                    if this.path != origin.path {
                        imported.failures.push(format!("笔记路径已改变，附件保留在 {}", origin.path));
                        imported.pieces = imported
                            .paths
                            .iter()
                            .filter_map(|path| attachment_text(&this.path, path, true).ok())
                            .collect();
                    }
                    this.finish_import(imported, mapped.clone(), padding, window, cx);
                    true
                });
                match applied {
                    Ok(true) => break,
                    Ok(false) => continue, // Input/reload raced the worker: re-map off-thread, never diff in UI.
                    Err(error) => {
                        // Normal window/tab close is guarded. Still preserve the edit if a window is
                        // removed directly, without requiring a live Window or attempting to focus it.
                        log::warn!("attachment import window closed: {error}");
                        let applied = editor.update(cx, |this, cx| {
                            if this.doc.version() != current.snapshot.version()
                                || this.io.epoch != current.epoch
                            {
                                return false;
                            }
                            this.apply_import(imported.take().unwrap(), mapped, padding, cx);
                            true
                        });
                        if applied {
                            break;
                        }
                    }
                }
            }
            let _ = editor.update(cx, |this, cx| {
                this.imports.pending -= 1;
                cx.emit(super::EditorEvent::Changed);
                cx.notify();
            });
        }));
        cx.emit(super::EditorEvent::Changed);
        cx.notify();
    }

    fn finish_import(
        &mut self,
        imported: Imported,
        range: Range<usize>,
        padding: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(error) = imported.failures.first() {
            window.push_notification(Notification::error(error.clone()).title("导入附件"), cx);
        }
        let focused = self.focus_handle.is_focused(window)
            || self.focused.as_ref().is_some_and(|f| f.textarea.focus_handle(cx).is_focused(window));
        if let Some(at) = self.apply_import(imported, range, padding, cx) {
            if focused {
                self.focus_at(at, None, window, cx);
            }
        }
    }

    fn apply_import(
        &mut self,
        imported: Imported,
        range: Range<usize>,
        padding: bool,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        for error in &imported.failures {
            log::warn!("import attachment: {error}");
        }
        if imported.paths.is_empty() {
            return None;
        }
        if let Some(service) = &self.images {
            service.update(cx, |service, cx| service.invalidate(&imported.paths, cx));
        }
        cx.emit(super::EditorEvent::PathsTouched(imported.paths));
        let text = imported.pieces.join("\n\n");
        let text = if padding { format!("\n\n{text}\n") } else { text };
        let at = range.start + text.len();
        if let Err(error) = self.doc.edit(Edit::replace(range, text)) {
            log::error!("import attachment insertion rejected: {error}");
            return None;
        }
        self.last_edit_at = None;
        self.focused = None;
        self.after_doc_change(cx);
        Some(at)
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

    /// 单方括号链接一律交给浏览器。没有协议时补上 `http://`。
    pub(crate) fn open_external_link(&self, url: &str, _window: &mut Window, cx: &mut Context<Self>) {
        let url = url.trim();
        let url = if has_scheme(url) { url.to_owned() } else { format!("http://{url}") };
        cx.open_url(&url);
    }
}

fn has_scheme(url: &str) -> bool {
    url.split_once(':').is_some_and(|(scheme, rest)| {
        !scheme.is_empty() && !rest.is_empty() && scheme.bytes().all(|b| b.is_ascii_alphabetic())
    })
}

/// 附件相对当前笔记所在目录的路径。笔记在库根时就是库内路径。
fn relative_to_note(note: &VaultPath, file: &str) -> String {
    let parent: Vec<_> = note.parent().split('/').filter(|part| !part.is_empty()).collect();
    let target: Vec<_> = file.split('/').collect();
    let shared = parent.iter().zip(&target).take_while(|(a, b)| a == b).count();
    std::iter::repeat_n("..", parent.len() - shared)
        .chain(target[shared..].iter().copied())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod import_tests {
    use std::sync::Arc;

    use aero_core::{Vault, VaultPath};
    use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext};

    use crate::editor::Editor;

    fn open(cx: &mut TestAppContext, body: &str) -> (tempfile::TempDir, Entity<Editor>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), body).unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let path = VaultPath::new("a.md").unwrap();
        let (doc, stamp) = Editor::load(vault.clone(), path.clone()).unwrap();
        let (editor, _) = cx.add_window_view(|_, cx| Editor::new(vault, path, doc, stamp, cx));
        let handle = *cx.windows().last().unwrap();
        VisualTestContext::from_window(handle, cx).update(|window, cx| {
            window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(editor.clone(), window, cx));
        });
        (dir, editor)
    }

    fn source() -> (tempfile::TempDir, gpui_kit::ExternalPaths) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("shot.png");
        std::fs::write(&file, b"original bytes").unwrap();
        (dir, gpui_kit::ExternalPaths(vec![file].into()))
    }

    fn focus(editor: &Entity<Editor>, range: std::ops::Range<usize>, cx: &mut VisualTestContext) {
        editor.update_in(cx, |e, window, cx| {
            e.focus_unit(0, range.start, window, cx);
            e.focused.as_ref().unwrap().textarea.update(cx, |ta, cx| ta.set_selected_range(range, cx));
        });
    }

    #[gpui_kit::test]
    fn files_defer_reads_and_keep_successes_after_collisions_and_failure(cx: &mut TestAppContext) {
        let (dir, editor) = open(cx, "body\n");
        let (outside, paths) = source();
        std::fs::create_dir(dir.path().join("a.assets")).unwrap();
        std::fs::write(dir.path().join("a.assets/shot.png"), b"existing").unwrap();
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        editor.update_in(cx, |e, window, cx| {
            let files = gpui_kit::ExternalPaths(
                vec![
                    paths.paths()[0].clone(),
                    outside.path().join("missing.png"),
                    outside.path().to_owned(),
                    paths.paths()[0].clone(),
                ]
                .into(),
            );
            e.drop_external_files(&files, window, cx);
            assert!(!dir.path().join("a.assets/shot-2.png").exists(), "copy must be asynchronous");
            assert!(e.has_pending_export(), "pending imports must guard note rename/close/trash");
            std::fs::write(&paths.paths()[0], b"read in background").unwrap();
        });
        cx.run_until_parked();
        editor.update_in(cx, |e, _, _| {
            let text = e.doc.to_string();
            assert!(text.contains("![[shot-2.png]]\n\n![[shot-3.png]]"), "{text}");
            assert!(!text.contains("missing"));
            assert!(!e.has_pending_export());
        });
        assert_eq!(std::fs::read(dir.path().join("a.assets/shot.png")).unwrap(), b"existing");
        for name in ["shot-2.png", "shot-3.png"] {
            assert_eq!(std::fs::read(dir.path().join("a.assets").join(name)).unwrap(), b"read in background");
        }
    }

    #[gpui_kit::test]
    fn failed_import_drains_pending_state_without_changing_source(cx: &mut TestAppContext) {
        let (dir, editor) = open(cx, "body\n");
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        focus(&editor, 0..4, cx);
        editor.update_in(cx, |e, window, cx| {
            let paths = gpui_kit::ExternalPaths(vec![dir.path().join("missing.png")].into());
            e.drop_external_files(&paths, window, cx);
            assert!(e.has_pending_export());
        });
        cx.run_until_parked();
        editor.update_in(cx, |e, _, _| {
            assert!(!e.has_pending_export());
            assert_eq!(e.doc.to_string(), "body\n");
            assert!(!e.doc.can_undo());
        });
    }

    #[gpui_kit::test]
    fn reload_with_reused_version_maps_against_the_new_epoch(cx: &mut TestAppContext) {
        let (_dir, editor) = open(cx, "left SELECT right\n");
        let (_outside, paths) = source();
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        focus(&editor, 5..11, cx);
        editor.update_in(cx, |e, window, cx| {
            let version = e.doc.version();
            e.drop_external_files(&paths, window, cx);
            e.install_document(aero_core::Document::from_text("left NEW right\n"), e.stamp, cx);
            assert_eq!(e.doc.version(), version, "version alone cannot identify a reloaded document");
        });
        cx.run_until_parked();
        editor.update_in(cx, |e, _, _| {
            assert_eq!(e.doc.to_string(), "left NEW![[shot.png]] right\n");
            assert!(!e.has_pending_export());
        });
    }

    #[gpui_kit::test]
    fn bitmap_write_is_deferred_and_undo_is_one_step(cx: &mut TestAppContext) {
        let (dir, editor) = open(cx, "body\n");
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        focus(&editor, 2..2, cx);
        editor.update_in(cx, |e, window, cx| {
            let image = gpui_kit::Image { format: gpui_kit::ImageFormat::Png, bytes: vec![1, 2, 3], id: 77 };
            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_image(&image));
            e.on_paste_attachment(&crate::actions::PasteAttachment, window, cx);
            assert!(!dir.path().join("a.assets").exists(), "bitmap write must be asynchronous");
            assert_eq!(e.doc.to_string(), "body\n");
        });
        cx.run_until_parked();
        editor.update_in(cx, |e, window, cx| {
            let body = e.doc.to_string();
            let name = body.split_once("![[").unwrap().1.split_once("]]").unwrap().0;
            assert_eq!(std::fs::read(dir.path().join("a.assets").join(name)).unwrap(), [1, 2, 3]);
            e.on_undo(&crate::actions::DocUndo, window, cx);
            assert_eq!(e.doc.to_string(), "body\n");
        });
    }

    #[gpui_kit::test]
    fn original_selection_tracks_input_and_not_the_moved_cursor(cx: &mut TestAppContext) {
        for (changed, expected) in [
            ("left SELECT right", "left ![[shot.png]] right\n"),
            ("new left SELECT right", "new left ![[shot.png]] right\n"),
            ("left SELECT right new", "left ![[shot.png]] right new\n"),
            ("left USER right", "left USER![[shot.png]] right\n"),
            ("left SEL新ECT right", "left SEL新![[shot.png]]ECT right\n"),
        ] {
            let (_dir, editor) = open(cx, "left SELECT right\n");
            let (_outside, paths) = source();
            let handle = *cx.windows().last().unwrap();
            let cx = &mut VisualTestContext::from_window(handle, cx);
            focus(&editor, 5..11, cx);
            editor.update_in(cx, |e, window, cx| {
                e.drop_external_files(&paths, window, cx);
                e.focused.as_ref().unwrap().textarea.update(cx, |ta, cx| {
                    ta.set_value(changed, window, cx);
                    ta.set_selected_range(0..0, cx);
                });
                e.on_change(window, cx);
                assert_eq!(e.doc.to_string(), format!("{changed}\n"));
            });
            cx.run_until_parked();
            editor.update_in(cx, |e, window, cx| {
                assert_eq!(e.doc.to_string(), expected);
                e.on_undo(&crate::actions::DocUndo, window, cx);
                assert_eq!(e.doc.to_string(), format!("{changed}\n"), "undo must keep intervening input");
            });
        }
    }

    #[test]
    fn repath_fallback_keeps_image_and_archive_links_pointing_to_imported_files() {
        let note = VaultPath::new("moved/deep/a.md").unwrap();
        let image = VaultPath::new("old/a.assets/shot.png").unwrap();
        let archive = VaultPath::new("old/a.assets/notes 1.zip").unwrap();
        assert_eq!(super::attachment_text(&note, &image, true).unwrap(), "![[old/a.assets/shot.png]]");
        assert_eq!(
            super::attachment_text(&note, &archive, true).unwrap(),
            "[[old/a.assets/notes 1.zip]](<../../old/a.assets/notes 1.zip>)"
        );
    }

    #[test]
    fn caret_and_selection_mapping_respects_utf8_and_boundary_affinity() {
        for (old, new, range, expected) in [
            ("abc", "axc", 1..1, 2..2),
            ("abc", "a新bc", 1..1, 4..4),
            ("abc", "a新bc", 1..2, 4..5),
            ("abc", "ab新c", 1..2, 1..2),
            ("a中文z", "a文字z", 1..1, 7..7),
            ("a中文z", "az", 1..7, 1..1),
            ("left SELECT right", "new left SELECT right new", 5..11, 25..25),
        ] {
            assert_eq!(super::map_import_range(range, old, new), expected, "{old} -> {new}");
        }
    }

    #[gpui_kit::test]
    fn queued_imports_keep_request_order_and_original_insertion_point(cx: &mut TestAppContext) {
        let (_dir, editor) = open(cx, "left right\n");
        let (_outside, paths) = source();
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        focus(&editor, 5..5, cx);
        editor.update_in(cx, |e, window, cx| {
            e.drop_external_files(&paths, window, cx);
            e.drop_external_files(&paths, window, cx);
            e.focused.as_ref().unwrap().textarea.update(cx, |ta, cx| ta.set_selected_range(0..0, cx));
        });
        cx.run_until_parked();
        editor.update_in(cx, |e, window, cx| {
            assert_eq!(e.doc.to_string(), "left ![[shot.png]]![[shot-2.png]]right\n");
            e.on_undo(&crate::actions::DocUndo, window, cx);
            assert_eq!(e.doc.to_string(), "left ![[shot.png]]right\n");
        });
    }

    #[gpui_kit::test]
    fn switching_note_keeps_import_in_origin_without_stealing_focus(cx: &mut TestAppContext) {
        let (dir, editor) = open(cx, "original note\n");
        let (_outside, paths) = source();
        std::fs::write(dir.path().join("b.md"), "other note\n").unwrap();
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        focus(&editor, 9..9, cx);
        let other = editor.update_in(cx, |e, window, cx| {
            e.drop_external_files(&paths, window, cx);
            assert!(e.has_pending_export());
            assert!(e.flush_sync().is_err(), "cannot close or repath while importing");
            let path = VaultPath::new("b.md").unwrap();
            let vault = e.vault.clone();
            let (doc, stamp) = Editor::load(vault.clone(), path.clone()).unwrap();
            let other = cx.new(|cx| Editor::new(vault, path, doc, stamp, cx));
            e.exit_edit(cx);
            window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(other.clone(), window, cx));
            other.update(cx, |e, cx| e.focus_unit(0, 0, window, cx));
            other
        });
        let other_focus = cx.update(|window, cx| window.focused(cx));
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert_eq!(window.focused(cx), other_focus);
            assert_eq!(other.read(cx).doc.to_string(), "other note\n");
            assert_eq!(editor.read(cx).doc.to_string(), "original ![[shot.png]]note\n");
            assert!(editor.read(cx).focused.is_none());
            assert!(!editor.read(cx).has_pending_export());
        });
    }
}

/// 四角拖动手柄。`east` 为右侧两角：向右拖变宽；左侧两角相反。
#[derive(Clone)]
pub(super) struct ImageResizeDrag {
    pub block: aero_core::BlockId,
    pub index: usize,
    pub east: bool,
    pub start_width: f32,
    pub min_width: f32,
    pub max_width: f32,
}

pub(super) struct ImageResize {
    block: aero_core::BlockId,
    index: usize,
    east: bool,
    start_x: f32,
    start_width: f32,
    min_width: f32,
    max_width: f32,
    current: f32,
}

impl super::Editor {
    pub(super) fn image_drag_width(&self, block: aero_core::BlockId, index: usize) -> Option<f32> {
        self.image_resize
            .as_ref()
            .filter(|drag| drag.block == block && drag.index == index)
            .map(|drag| drag.current)
    }

    pub(super) fn start_image_resize(&mut self, drag: &ImageResizeDrag, x: Pixels, cx: &mut Context<Self>) {
        let start_width = drag.start_width.clamp(drag.min_width, drag.max_width);
        self.image_resize = Some(ImageResize {
            block: drag.block,
            index: drag.index,
            east: drag.east,
            start_x: f32::from(x),
            start_width,
            min_width: drag.min_width,
            max_width: drag.max_width,
            current: start_width,
        });
        cx.notify();
    }

    pub(super) fn track_image_resize(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let Some(drag) = self.image_resize.as_mut() else { return };
        let delta = f32::from(x) - drag.start_x;
        let signed = if drag.east { delta } else { -delta };
        drag.current = (drag.start_width + signed).clamp(drag.min_width, drag.max_width);
        cx.notify();
    }

    /// 松手把当前宽度写进 `|宽度`。一次撤销。
    pub(super) fn finish_image_resize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.image_resize.take() else { return };
        cx.notify();
        let width = drag.current.round().max(1.) as u32;
        if (width as f32 - drag.start_width).abs() < 1. {
            return;
        }
        self.set_image_width(drag.block, drag.index, width, window, cx);
    }

    /// 选中图片后改 `|宽度`。并排时只改这一张，改完仍选中它。
    pub(super) fn set_image_width(
        &mut self,
        block: aero_core::BlockId,
        index: usize,
        width: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(range) = self
            .doc
            .blocks()
            .iter()
            .find(|(_, item)| item.id == block)
            .map(|(_, item)| self.doc.content_range(item))
        else {
            return;
        };
        let source = self.doc.slice(range.clone()).to_string();
        let Some(next) = super::preview::image_source_set_width(&source, index, width) else { return };
        if next == source {
            return;
        }
        if let Err(err) = self.doc.edit(Edit::replace(range, next)) {
            log::warn!("改图片宽度失败: {err}");
            return;
        }
        self.after_doc_change(cx);
        if self.unit_index(block).is_some() {
            self.select_image(block, index, window, cx);
        }
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
            State::Ready { image, pixels, .. } => {
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
