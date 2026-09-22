//! 左侧文件树：笔记与目录、拖放移动、行内重命名。
//!
//! 只显示文件夹与 `.md`（扩展名隐藏）；`*.assets/` 与其他文件由 `Vault::notes()` 过滤掉。
//! 文件夹按名称排序，`daily/` 例外按日期倒序。行高固定，用 VirtualList 承载大 vault。

use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use aero_core::{Vault, VaultPath};
use gpui_kit::assets::IconName;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Icon, VirtualListScrollHandle, h_flex, v_flex, v_virtual_list};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::actions::{CopyNotePath, RevealInFinder, TrashNote};

pub const ROW_HEIGHT: f32 = 28.;

pub enum FileTreeEvent {
    Open(VaultPath),
    Move { from: VaultPath, to: VaultPath },
}

actions!(file_tree, [RenameSelection]);

#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = file_tree, no_json)]
pub struct RenameEntry(pub String);

struct RenameInput {
    path: VaultPath,
    directory: bool,
    input: Entity<gpui_kit::base::input::InputState>,
    _subscription: Subscription,
}

/// 文件树里拖出来的一项。编辑区接受笔记（插入链接），树内接受移动。
#[derive(Clone)]
pub(crate) struct TreeDrag {
    pub(crate) vault: String,
    pub(crate) path: VaultPath,
    pub(crate) directory: bool,
}

impl TreeDrag {
    fn target(&self, vault: &str, directory: &str) -> Option<VaultPath> {
        if self.vault != vault
            || (self.directory
                && (directory == self.path.as_str() || directory.starts_with(&format!("{}/", self.path))))
        {
            return None;
        }
        let target = if directory.is_empty() {
            self.path.file_name().to_owned()
        } else {
            format!("{directory}/{}", self.path.file_name())
        };
        let target = VaultPath::new(&target).ok()?;
        (target != self.path).then_some(target)
    }
}

impl Render for TreeDrag {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_1()
            .rounded_md()
            .bg(cx.theme().popover)
            .text_sm()
            .child(self.path.file_name().to_owned())
    }
}

impl EventEmitter<FileTreeEvent> for FileTree {}

/// 内存中的目录树。`path` 是 vault 内的相对目录（根为空串）。
#[derive(Default)]
struct Folder {
    name: String,
    path: String,
    folders: Vec<Folder>,
    notes: Vec<VaultPath>,
}

impl Folder {
    fn build(notes: &[VaultPath], folders: &[VaultPath]) -> Folder {
        let mut root = Folder::default();
        for folder in folders {
            root.ensure_dir(folder.as_str());
        }
        for note in notes {
            root.ensure_dir(note.parent()).notes.push(note.clone());
        }
        root.sort();
        root
    }

    fn ensure_dir(&mut self, dir: &str) -> &mut Folder {
        let mut cur = self;
        let mut prefix = String::new();
        for seg in dir.split('/').filter(|seg| !seg.is_empty()) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(seg);
            let idx = match cur.folders.iter().position(|f| f.name == seg) {
                Some(i) => i,
                None => {
                    cur.folders.push(Folder {
                        name: seg.to_owned(),
                        path: prefix.clone(),
                        ..Default::default()
                    });
                    cur.folders.len() - 1
                }
            };
            cur = &mut cur.folders[idx];
        }
        cur
    }

    fn sort(&mut self) {
        self.folders.sort_by_key(|f| f.name.to_lowercase());
        // daily/ 按日期（文件名）倒序，其余按名称
        if self.name == "daily" {
            self.notes.sort_by(|a, b| b.stem().cmp(a.stem()));
        } else {
            self.notes.sort_by_key(|n| n.stem().to_lowercase());
        }
        for f in &mut self.folders {
            f.sort();
        }
    }
}

#[derive(Clone, Debug)]
enum Row {
    Folder {
        path: String,
        name: String,
        depth: usize,
        expanded: bool,
    },
    Note {
        path: VaultPath,
        depth: usize,
    },
    /// `<note>.assets/` 里的附件；默认折叠，点笔记行上的 📎 展开（`ui_design.md` 4.1）
    Attachment {
        path: VaultPath,
        depth: usize,
    },
}

pub struct FileTree {
    vault: Arc<Vault>,
    root: Folder,
    /// 展开的文件夹路径，以及展开了附件的笔记路径（笔记路径带 `.md`，不会与文件夹撞）
    expanded: HashSet<String>,
    /// 笔记 → 它 `<note>.assets/` 下的附件
    attachments: BTreeMap<VaultPath, Vec<VaultPath>>,
    /// 引用计数报告（后台扫描得来）；没有就不显示"未引用"
    references: Option<aero_core::attachments::References>,
    rows: Vec<Row>,
    row_sizes: Rc<Vec<Size<Pixels>>>,
    selected: Option<VaultPath>,
    selected_folder: Option<VaultPath>,
    rename_input: Option<RenameInput>,
    scroll: VirtualListScrollHandle,
    /// 点击行后树持有焦点，`⌘⌫` 等树内快捷键（上下文 `FileTree`）才有处可去
    focus_handle: FocusHandle,
    pub error: Option<String>,
}

impl FileTree {
    pub fn new(vault: Arc<Vault>, cx: &mut Context<Self>) -> Self {
        let mut this = FileTree {
            vault,
            root: Folder::default(),
            expanded: HashSet::new(),
            attachments: BTreeMap::new(),
            references: None,
            rows: Vec::new(),
            row_sizes: Rc::new(Vec::new()),
            selected: None,
            selected_folder: None,
            rename_input: None,
            scroll: VirtualListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            error: None,
        };
        this.rescan();
        // 默认展开第一层，让新用户一眼看到内容
        for f in &this.root.folders {
            this.expanded.insert(f.path.clone());
        }
        this.rebuild_rows();
        this
    }

    /// 重新枚举磁盘上的笔记和目录（包括空目录）与每篇笔记的附件。
    pub fn rescan(&mut self) {
        match self.vault.notes().and_then(|notes| self.vault.folders().map(|folders| (notes, folders))) {
            Ok((notes, folders)) => {
                self.root = Folder::build(&notes, &folders);
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        // 附件只按后缀枚举、不读内容；枚举失败不影响笔记树
        self.attachments.clear();
        if let Ok(catalog) = aero_core::attachments::Catalog::read(&self.vault) {
            for entry in catalog.entries {
                if let Some(note) = owner_note(&entry.path) {
                    self.attachments.entry(note).or_default().push(entry.path);
                }
            }
        }
        self.rebuild_rows();
    }

    /// 后台扫出来的引用计数（工作区送进来）：用来标出没人引用的附件。
    pub fn set_references(&mut self, references: aero_core::attachments::References, cx: &mut Context<Self>) {
        self.references = Some(references);
        cx.notify();
    }

    /// 笔记行上的 📎：展开 / 收起它的附件。
    fn toggle_attachments(&mut self, note: &VaultPath, cx: &mut Context<Self>) {
        if !self.expanded.remove(note.as_str()) {
            self.expanded.insert(note.as_str().to_owned());
        }
        self.rebuild_rows();
        cx.notify();
    }

    /// 树里是否有这篇笔记（上次 `rescan` 时磁盘上的、且没被忽略规则过滤掉）。
    #[cfg(test)]
    pub fn contains(&self, path: &VaultPath) -> bool {
        let mut cur = &self.root;
        for seg in path.parent().split('/').filter(|s| !s.is_empty()) {
            match cur.folders.iter().find(|f| f.name == seg) {
                Some(f) => cur = f,
                None => return false,
            }
        }
        cur.notes.contains(path)
    }

    /// 当前选中的文件夹（选中的是笔记时取其所在目录），用于新建笔记的位置规则（3.5）。
    pub fn selected_dir(&self) -> Option<String> {
        self.selected_folder
            .as_ref()
            .map(ToString::to_string)
            .or_else(|| self.selected.as_ref().map(|p| p.parent().to_owned()))
    }

    pub fn selected(&self) -> Option<&VaultPath> {
        self.selected.as_ref()
    }

    pub fn selected_folder(&self) -> Option<&VaultPath> {
        self.selected_folder.as_ref()
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.focus_handle.focus(window, cx);
    }

    pub fn select(&mut self, path: Option<VaultPath>, cx: &mut Context<Self>) {
        self.selected_folder = None;
        // 展开到该笔记
        if let Some(p) = &path {
            let mut prefix = String::new();
            for seg in p.parent().split('/').filter(|s| !s.is_empty()) {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(seg);
                self.expanded.insert(prefix.clone());
            }
        }
        self.selected = path;
        self.rebuild_rows();
        if let Some(sel) = &self.selected
            && let Some(ix) =
                self.rows.iter().position(|r| matches!(r, Row::Note { path, .. } if path == sel))
        {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
        }
        cx.notify();
    }

    fn rebuild_rows(&mut self) {
        fn walk(
            f: &Folder,
            depth: usize,
            expanded: &HashSet<String>,
            attachments: &BTreeMap<VaultPath, Vec<VaultPath>>,
            out: &mut Vec<Row>,
        ) {
            for sub in &f.folders {
                let is_open = expanded.contains(&sub.path);
                out.push(Row::Folder {
                    path: sub.path.clone(),
                    name: sub.name.clone(),
                    depth,
                    expanded: is_open,
                });
                if is_open {
                    walk(sub, depth + 1, expanded, attachments, out);
                }
            }
            for n in &f.notes {
                out.push(Row::Note { path: n.clone(), depth });
                if expanded.contains(n.as_str()) {
                    for file in attachments.get(n).into_iter().flatten() {
                        out.push(Row::Attachment { path: file.clone(), depth: depth + 1 });
                    }
                }
            }
        }
        let mut rows = Vec::new();
        walk(&self.root, 0, &self.expanded, &self.attachments, &mut rows);
        self.row_sizes = Rc::new(vec![size(px(200.), px(ROW_HEIGHT)); rows.len()]);
        self.rows = rows;
    }

    fn toggle_folder(&mut self, path: &str, cx: &mut Context<Self>) {
        self.selected_folder = VaultPath::new(path).ok();
        self.selected = None;
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_owned());
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn open(&mut self, path: VaultPath, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(path.clone());
        self.selected_folder = None;
        self.focus_handle.focus(window, cx);
        cx.emit(FileTreeEvent::Open(path));
        cx.notify();
    }

    fn start_rename(&mut self, path: VaultPath, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::base::input::{InputEvent, InputState};
        let directory = self.vault.fs_path(&path).is_dir();
        let name = if directory { path.file_name() } else { path.stem() };
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |s, cx| {
            s.set_value(name, window, cx);
            s.set_selected_range(0..name.len(), cx);
            s.focus(window, cx);
        });
        let subscription = cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
            InputEvent::PressEnter { .. } => this.finish_rename(window, cx),
            InputEvent::Blur => {
                this.rename_input = None;
                cx.notify();
            }
            _ => {}
        });
        self.rename_input = Some(RenameInput { path, directory, input, _subscription: subscription });
        cx.notify();
    }

    fn finish_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.rename_input.as_ref() else { return };
        let value = edit.input.read(cx).value();
        let name = value.trim();
        if name.is_empty()
            || name.starts_with('.')
            || name.ends_with(".assets")
            || name.contains(['/', '\\'])
            || name.chars().any(char::is_control)
        {
            self.error = Some("请输入有效文件名，不能包含路径或隐藏目录名".into());
            cx.notify();
            return;
        }
        let name =
            if edit.directory || name.ends_with(".md") { name.to_owned() } else { format!("{name}.md") };
        let target =
            if edit.path.parent().is_empty() { name } else { format!("{}/{name}", edit.path.parent()) };
        let to = VaultPath::new(&target).expect("validated entry name");
        let from = edit.path.clone();
        self.rename_input = None;
        self.error = None;
        self.focus_handle.focus(window, cx);
        if from != to {
            cx.emit(FileTreeEvent::Move { from, to });
        }
        cx.notify();
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let hover = theme.list_hover;
        let active = theme.list_active;
        let warning = theme.warning;
        let indent = |depth: usize| px(8. + depth as f32 * 14.);
        let row_path = match &self.rows[ix] {
            Row::Folder { path, .. } => path.as_str(),
            // 附件不参与行内重命名（改名要连着引用一起改，见 11 节）
            Row::Note { path, .. } | Row::Attachment { path, .. } => path.as_str(),
        };
        if let Some(edit) = self.rename_input.as_ref().filter(|edit| edit.path.as_str() == row_path) {
            return div()
                .id("file-tree-rename-input")
                .debug_selector(|| "file-tree-rename-input".into())
                .h(px(ROW_HEIGHT))
                .child(gpui_kit::component::input::Input::new(&edit.input))
                .into_any_element();
        }

        match self.rows[ix].clone() {
            Row::Folder { path, name, depth, expanded } => h_flex()
                .id(("folder", ix))
                .debug_selector({
                    let path = path.clone();
                    move || format!("file-tree-folder:{path}")
                })
                .h(px(ROW_HEIGHT))
                .pl(indent(depth))
                .pr_2()
                .gap_1()
                .items_center()
                .rounded_md()
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .text_sm()
                .child(
                    Icon::new(if expanded { IconName::ChevronDown } else { IconName::ChevronRight })
                        .size(px(14.))
                        .text_color(muted),
                )
                .child(Icon::new(IconName::Folder).size(px(14.)).text_color(muted))
                .child(div().flex_1().truncate().child(name))
                .on_drag(
                    TreeDrag {
                        vault: self.vault.id().into(),
                        path: VaultPath::new(&path).expect("folder path"),
                        directory: true,
                    },
                    |drag, _, _, cx| cx.new(|_| drag.clone()),
                )
                .can_drop({
                    let vault = self.vault.id().to_owned();
                    let path = path.clone();
                    move |value, _, _| {
                        value.downcast_ref::<TreeDrag>().is_some_and(|d| d.target(&vault, &path).is_some())
                    }
                })
                .drag_over::<TreeDrag>({
                    let vault = self.vault.id().to_owned();
                    let path = path.clone();
                    move |style, drag, _, _| {
                        if drag.target(&vault, &path).is_some() { style.bg(active) } else { style }
                    }
                })
                .on_drop(cx.listener({
                    let path = path.clone();
                    move |this, drag: &TreeDrag, _, cx| {
                        if let Some(to) = drag.target(this.vault.id(), &path) {
                            cx.emit(FileTreeEvent::Move { from: drag.path.clone(), to });
                        }
                    }
                }))
                .on_click(cx.listener({
                    let path = path.clone();
                    move |this, _, window, cx| {
                        this.focus_handle.focus(window, cx);
                        this.toggle_folder(&path, cx);
                    }
                }))
                .context_menu({
                    let path = path.clone();
                    move |menu, _, _| menu.menu("重命名", Box::new(RenameEntry(path.clone())))
                })
                .into_any_element(),
            Row::Note { path, depth } => {
                let selected = self.selected.as_ref() == Some(&path);
                let rel = path.as_str().to_owned();
                h_flex()
                    .id(("note", ix))
                    .debug_selector({
                        let rel = rel.clone();
                        move || format!("file-tree-note:{rel}")
                    })
                    .h(px(ROW_HEIGHT))
                    .pl(indent(depth) + px(18.))
                    .pr_2()
                    .gap_1()
                    .items_center()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |s| s.bg(active))
                    .when(!selected, |s| s.hover(move |s| s.bg(hover)))
                    .text_sm()
                    .child(Icon::new(IconName::FileText).size(px(14.)).text_color(muted))
                    .child(div().flex_1().truncate().child(path.stem().to_owned()))
                    // 📎 与附件数：点它展开 / 收起附件，不打开笔记（4.1）
                    .when_some(self.attachments.get(&path).map(Vec::len).filter(|n| *n > 0), |el, count| {
                        let note = path.clone();
                        el.child(
                            div()
                                .id(("clip", ix))
                                .debug_selector({
                                    let note = note.clone();
                                    move || format!("file-tree-clip:{note}")
                                })
                                .px_1()
                                .rounded_md()
                                .text_xs()
                                .text_color(muted)
                                .hover(move |s| s.bg(hover))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.toggle_attachments(&note, cx);
                                }))
                                .child(format!("📎{count}")),
                        )
                    })
                    .on_drag(
                        TreeDrag { vault: self.vault.id().into(), path: path.clone(), directory: false },
                        |drag, _, _, cx| cx.new(|_| drag.clone()),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| this.open(path.clone(), window, cx)))
                    // 右键菜单（4.1 的 M1a 子集；重命名走标题，新建走 ⌘N）
                    .context_menu(move |menu, _, _| {
                        menu.menu("重命名", Box::new(RenameEntry(rel.clone())))
                            .menu("移到废纸篓", Box::new(TrashNote(rel.clone())))
                            .separator()
                            .menu("在 Finder 中显示", Box::new(RevealInFinder(rel.clone())))
                            .menu("复制路径", Box::new(CopyNotePath(rel.clone())))
                    })
                    .into_any_element()
            }
            Row::Attachment { path, depth } => {
                // 报告还没算出来时不显示"未引用"，免得把正常附件误标成孤儿
                let unreferenced = self.references.as_ref().is_some_and(|r| r.count(&path) == 0);
                let file = self.vault.fs_path(&path);
                h_flex()
                    .id(("attachment", ix))
                    .debug_selector({
                        let rel = path.as_str().to_owned();
                        move || format!("file-tree-attachment:{rel}")
                    })
                    .h(px(ROW_HEIGHT))
                    .pl(indent(depth) + px(32.))
                    .pr_2()
                    .gap_1()
                    .items_center()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .text_xs()
                    .text_color(muted)
                    .child(div().flex_1().truncate().child(path.file_name().to_owned()))
                    .when(unreferenced, |el| {
                        el.child(
                            div()
                                .debug_selector({
                                    let rel = path.as_str().to_owned();
                                    move || format!("file-tree-orphan:{rel}")
                                })
                                .text_color(warning)
                                .child("未引用"),
                        )
                    })
                    // 双击用系统应用打开（只读，4.1）
                    .on_click(move |event, _, cx| {
                        if matches!(event, ClickEvent::Mouse(click) if click.down.click_count >= 2) {
                            cx.open_url(&crate::paths::file_url(&file));
                        }
                    })
                    .into_any_element()
            }
        }
    }
}

/// `notes/a.assets/x.png` → `notes/a.md`；不在某篇笔记附件目录里的附件返回 `None`。
fn owner_note(path: &VaultPath) -> Option<VaultPath> {
    let at = path.as_str().find(".assets/")?;
    VaultPath::new(&format!("{}.md", &path.as_str()[..at])).ok()
}

impl Render for FileTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let list = v_flex()
            .id("file-tree-rows")
            .relative()
            .flex_1()
            .min_h(px(0.))
            .px_1()
            .child(
                v_virtual_list(cx.entity().clone(), "rows", self.row_sizes.clone(), |this, range, _, cx| {
                    range.map(|ix| this.render_row(ix, cx)).collect()
                })
                .track_scroll(&self.scroll),
            )
            .vertical_scrollbar(&self.scroll);

        v_flex()
            .id("file-tree")
            .track_focus(&self.focus_handle)
            .key_context("FileTree")
            .on_action(cx.listener(|this, _: &RenameSelection, window, cx| {
                if let Some(path) = this.selected_folder.as_ref().or(this.selected.as_ref()).cloned() {
                    this.start_rename(path, window, cx);
                }
            }))
            .on_action(cx.listener(|this, action: &RenameEntry, window, cx| {
                if let Ok(path) = VaultPath::new(&action.0) {
                    this.start_rename(path, window, cx);
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && this.rename_input.is_some() {
                    this.rename_input = None;
                    this.focus_handle.focus(window, cx);
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .size_full()
            .when_some(self.error.clone(), |this, err| {
                this.child(div().p_3().text_xs().text_color(cx.theme().danger).child(err))
            })
            .when(self.rows.is_empty() && self.error.is_none(), |this| {
                this.child(
                    div().p_3().text_sm().text_color(muted).child("这个 vault 还没有笔记。按 ⌘N 新建一篇。"),
                )
            })
            .child(
                div()
                    .id("file-tree-root")
                    .debug_selector(|| "file-tree-root".into())
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(muted)
                    .child("Vault 根目录")
                    .can_drop({
                        let vault = self.vault.id().to_owned();
                        move |value, _, _| {
                            value.downcast_ref::<TreeDrag>().is_some_and(|d| d.target(&vault, "").is_some())
                        }
                    })
                    .drag_over::<TreeDrag>({
                        let vault = self.vault.id().to_owned();
                        move |style, drag, _, cx| {
                            if drag.target(&vault, "").is_some() {
                                style.bg(cx.theme().list_active)
                            } else {
                                style
                            }
                        }
                    })
                    .on_drop(cx.listener(|this, drag: &TreeDrag, _, cx| {
                        if let Some(to) = drag.target(this.vault.id(), "") {
                            cx.emit(FileTreeEvent::Move { from: drag.path.clone(), to });
                        }
                    })),
            )
            .child(list)
    }
}

/// 供新建笔记时选择不冲突的文件名。
pub fn unique_note_name(existing: &[VaultPath], dir: &str, base: &str) -> VaultPath {
    let taken: BTreeMap<&str, ()> = existing.iter().map(|p| (p.as_str(), ())).collect();
    let mut n = 0;
    loop {
        let name = if n == 0 { format!("{base}.md") } else { format!("{base} {n}.md") };
        let full = if dir.is_empty() { name } else { format!("{dir}/{name}") };
        if !taken.contains_key(full.as_str()) {
            return VaultPath::new(&full).expect("generated note path is valid");
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::{FileTree, Row};
    use aero_core::Vault;
    use gpui_kit::{Entity, Modifiers, TestAppContext, VisualTestContext};
    use std::sync::Arc;

    fn setup(cx: &mut TestAppContext) -> (tempfile::TempDir, Arc<Vault>, Entity<FileTree>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("source/child")).unwrap();
        std::fs::create_dir_all(dir.path().join("empty")).unwrap();
        std::fs::write(dir.path().join("source/a.md"), "# A\n").unwrap();
        std::fs::write(dir.path().join("root.md"), "# Root\n").unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let (tree, _) = cx.add_window_view(|_, cx| FileTree::new(vault.clone(), cx));
        (dir, vault, tree)
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        draw(cx);
        let bounds = cx.debug_bounds(selector).expect(selector);
        cx.simulate_click(bounds.center(), Modifiers::default());
        draw(cx);
    }

    #[gpui_kit::test]
    fn file_tree_shows_empty_directories(cx: &mut TestAppContext) {
        let (_dir, _vault, tree) = setup(cx);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        draw(cx);
        cx.update(|_, cx| {
            assert!(
                tree.read(cx)
                    .rows
                    .iter()
                    .any(|row| matches!(row, Row::Folder { path, .. } if path == "empty")),
                "empty directories must be included in the rendered tree"
            );
        });
        assert!(cx.debug_bounds("file-tree-folder:empty").is_some());
        assert!(cx.debug_bounds("file-tree-folder:source/child").is_some());
    }

    #[gpui_kit::test]
    fn folder_selection_is_the_new_note_directory(cx: &mut TestAppContext) {
        let (_dir, _vault, tree) = setup(cx);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        click(cx, "file-tree-folder:empty");
        assert_eq!(tree.read_with(cx, |t, _| t.selected_dir()), Some("empty".into()));
    }

    #[gpui_kit::test]
    fn tree_drag_moves_files_and_directories_without_opening_them(cx: &mut TestAppContext) {
        use gpui_kit::{MouseButton, point, px};
        use std::{cell::RefCell, rc::Rc};
        let (_dir, _vault, tree) = setup(cx);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let moves = Rc::new(RefCell::new(Vec::new()));
        let opened = Rc::new(RefCell::new(0));
        cx.update(|_, cx| {
            let moves = moves.clone();
            let opened = opened.clone();
            cx.subscribe(&tree, move |_, event, _| match event {
                super::FileTreeEvent::Move { from, to } => {
                    moves.borrow_mut().push((from.to_string(), to.to_string()))
                }
                super::FileTreeEvent::Open(_) => *opened.borrow_mut() += 1,
            })
            .detach();
        });
        for (source, dest, expected) in [
            ("file-tree-note:source/a.md", "file-tree-folder:empty", Some(("source/a.md", "empty/a.md"))),
            ("file-tree-folder:source", "file-tree-folder:empty", Some(("source", "empty/source"))),
            ("file-tree-note:source/a.md", "file-tree-root", Some(("source/a.md", "a.md"))),
            ("file-tree-folder:source", "file-tree-folder:source/child", None),
        ] {
            draw(cx);
            let from = cx.debug_bounds(source).unwrap().center();
            let to = cx.debug_bounds(dest).unwrap().center();
            let count = moves.borrow().len();
            cx.simulate_mouse_move(from, None, Modifiers::default());
            cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
            cx.simulate_mouse_move(from + point(px(10.), px(0.)), MouseButton::Left, Modifiers::default());
            cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::default());
            draw(cx);
            cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
            draw(cx);
            if let Some((from, to)) = expected {
                assert_eq!(moves.borrow().len(), count + 1);
                assert_eq!(moves.borrow().last(), Some(&(from.into(), to.into())));
            } else {
                assert_eq!(moves.borrow().len(), count);
            }
        }
        assert_eq!(*opened.borrow(), 0);
    }

    #[gpui_kit::test]
    fn tree_rename_confirms_and_cancels_without_writing_files(cx: &mut TestAppContext) {
        use std::{cell::RefCell, rc::Rc};
        let (dir, _vault, tree) = setup(cx);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let moves = Rc::new(RefCell::new(Vec::new()));
        cx.update(|_, cx| {
            let sink = moves.clone();
            cx.subscribe(&tree, move |_, event, _| {
                if let super::FileTreeEvent::Move { from, to } = event {
                    sink.borrow_mut().push((from.to_string(), to.to_string()));
                }
            })
            .detach();
        });
        click(cx, "file-tree-note:source/a.md");
        cx.simulate_keystrokes("f2");
        draw(cx);
        cx.simulate_input("renamed");
        cx.simulate_keystrokes("enter");
        draw(cx);
        assert_eq!(&*moves.borrow(), &[("source/a.md".into(), "source/renamed.md".into())]);
        assert!(dir.path().join("source/a.md").is_file());
        cx.simulate_keystrokes("f2");
        draw(cx);
        cx.simulate_input("cancelled");
        cx.simulate_keystrokes("escape");
        draw(cx);
        assert!(cx.debug_bounds("file-tree-rename-input").is_none());
        assert_eq!(moves.borrow().len(), 1);
        click(cx, "file-tree-folder:empty");
        cx.simulate_keystrokes("f2");
        draw(cx);
        cx.simulate_input("renamed-folder");
        cx.simulate_keystrokes("enter");
        draw(cx);
        assert_eq!(moves.borrow().last(), Some(&("empty".into(), "renamed-folder".into())));
    }

    #[test]
    fn tree_drop_rejects_cross_vault_and_noop_targets() {
        let drag = super::TreeDrag {
            vault: "one".into(),
            path: aero_core::VaultPath::new("dir/n.md").unwrap(),
            directory: false,
        };
        assert!(drag.target("two", "target").is_none());
        assert!(drag.target("one", "dir").is_none());
    }

    #[gpui_kit::test]
    fn file_tree_f2_opens_inline_rename(cx: &mut TestAppContext) {
        let (_dir, _vault, _tree) = setup(cx);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        click(cx, "file-tree-note:source/a.md");
        cx.simulate_keystrokes("f2");
        draw(cx);
        assert!(cx.debug_bounds("file-tree-rename-input").is_some());
    }

    /// `<note>.assets/` 默认折叠：笔记行上显示 📎 与附件数，点开才列出来；
    /// 后台算出的引用报告把没人引用的标成"未引用"。
    #[gpui_kit::test]
    fn attachments_fold_under_their_note_and_mark_unreferenced_ones(cx: &mut TestAppContext) {
        let (dir, vault, tree) = setup(cx);
        std::fs::create_dir_all(dir.path().join("source/a.assets")).unwrap();
        std::fs::write(dir.path().join("source/a.assets/used.png"), b"png").unwrap();
        std::fs::write(dir.path().join("source/a.assets/orphan.png"), b"png").unwrap();
        std::fs::write(dir.path().join("source/a.md"), "# A\n\n![[used.png]]\n").unwrap();
        // 附件目录本身不出现在树里
        cx.update(|cx| {
            tree.update(cx, |t, cx| {
                t.rescan();
                cx.notify();
            })
        });
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        draw(cx);
        assert!(cx.debug_bounds("file-tree-folder:source/a.assets").is_none(), "附件目录不当文件夹显示");
        assert!(cx.debug_bounds("file-tree-attachment:source/a.assets/used.png").is_none(), "默认折叠");
        let clip = cx.debug_bounds("file-tree-clip:source/a.md").expect("笔记行应有 📎 徽标");

        // 点 📎 展开：两个附件都列出来，且不会顺带打开笔记
        cx.simulate_click(clip.center(), Modifiers::default());
        draw(cx);
        cx.update(|_, cx| assert_eq!(tree.read(cx).selected(), None, "点徽标不选中 / 不打开笔记"));
        for name in ["used.png", "orphan.png"] {
            let selector = format!("file-tree-attachment:source/a.assets/{name}");
            assert!(
                cx.update(|_, cx| tree.read(cx).rows.iter().any(|row| matches!(
                    row,
                    Row::Attachment { path, .. } if path.file_name() == name
                ))),
                "{selector} 应有附件行"
            );
        }
        // 报告还没来时不标"未引用"
        assert!(cx.debug_bounds("file-tree-orphan:source/a.assets/orphan.png").is_none());

        let report = aero_core::attachments::References::read(&vault).unwrap();
        cx.update(|_, cx| tree.update(cx, |t, cx| t.set_references(report, cx)));
        draw(cx);
        assert!(
            cx.debug_bounds("file-tree-orphan:source/a.assets/orphan.png").is_some(),
            "没人引用的附件要标出来"
        );
        assert!(
            cx.debug_bounds("file-tree-orphan:source/a.assets/used.png").is_none(),
            "被引用的不该标未引用"
        );

        // 再点一次收起
        let clip = cx.debug_bounds("file-tree-clip:source/a.md").unwrap();
        cx.simulate_click(clip.center(), Modifiers::default());
        draw(cx);
        cx.update(|_, cx| {
            assert!(!tree.read(cx).rows.iter().any(|row| matches!(row, Row::Attachment { .. })));
        });
    }
}
