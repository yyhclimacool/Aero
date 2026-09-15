//! 左侧文件树（`ui_design.md` 4.1，M1a 子集：无拖放、无重命名）。
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
    fn build(notes: &[VaultPath]) -> Folder {
        let mut root = Folder::default();
        for note in notes {
            let mut cur = &mut root;
            let dir = note.parent();
            if !dir.is_empty() {
                let mut prefix = String::new();
                for seg in dir.split('/') {
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
            }
            cur.notes.push(note.clone());
        }
        root.sort();
        root
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
    Folder { path: String, name: String, depth: usize, expanded: bool },
    Note { path: VaultPath, depth: usize },
}

pub struct FileTree {
    vault: Arc<Vault>,
    root: Folder,
    expanded: HashSet<String>,
    rows: Vec<Row>,
    row_sizes: Rc<Vec<Size<Pixels>>>,
    selected: Option<VaultPath>,
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
            rows: Vec::new(),
            row_sizes: Rc::new(Vec::new()),
            selected: None,
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

    /// 重新枚举磁盘上的笔记（新建 / 外部变化后调用）。
    pub fn rescan(&mut self) {
        match self.vault.notes() {
            Ok(notes) => {
                self.root = Folder::build(&notes);
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        self.rebuild_rows();
    }

    /// 当前选中的文件夹（选中的是笔记时取其所在目录），用于新建笔记的位置规则（3.5）。
    pub fn selected_dir(&self) -> Option<String> {
        self.selected.as_ref().map(|p| p.parent().to_owned())
    }

    pub fn selected(&self) -> Option<&VaultPath> {
        self.selected.as_ref()
    }

    pub fn select(&mut self, path: Option<VaultPath>, cx: &mut Context<Self>) {
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
        fn walk(f: &Folder, depth: usize, expanded: &HashSet<String>, out: &mut Vec<Row>) {
            for sub in &f.folders {
                let is_open = expanded.contains(&sub.path);
                out.push(Row::Folder {
                    path: sub.path.clone(),
                    name: sub.name.clone(),
                    depth,
                    expanded: is_open,
                });
                if is_open {
                    walk(sub, depth + 1, expanded, out);
                }
            }
            for n in &f.notes {
                out.push(Row::Note { path: n.clone(), depth });
            }
        }
        let mut rows = Vec::new();
        walk(&self.root, 0, &self.expanded, &mut rows);
        self.row_sizes = Rc::new(vec![size(px(200.), px(ROW_HEIGHT)); rows.len()]);
        self.rows = rows;
    }

    fn toggle_folder(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_owned());
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn open(&mut self, path: VaultPath, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(path.clone());
        self.focus_handle.focus(window, cx);
        cx.emit(FileTreeEvent::Open(path));
        cx.notify();
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let hover = theme.list_hover;
        let active = theme.list_active;
        let indent = |depth: usize| px(8. + depth as f32 * 14.);

        match self.rows[ix].clone() {
            Row::Folder { path, name, depth, expanded } => h_flex()
                .id(("folder", ix))
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
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_folder(&path, cx)))
                .into_any_element(),
            Row::Note { path, depth } => {
                let selected = self.selected.as_ref() == Some(&path);
                let rel = path.as_str().to_owned();
                h_flex()
                    .id(("note", ix))
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
                    .on_click(cx.listener(move |this, _, window, cx| this.open(path.clone(), window, cx)))
                    // 右键菜单（4.1 的 M1a 子集；重命名走标题，新建走 ⌘N）
                    .context_menu(move |menu, _, _| {
                        menu.menu("移到废纸篓", Box::new(TrashNote(rel.clone())))
                            .separator()
                            .menu("在 Finder 中显示", Box::new(RevealInFinder(rel.clone())))
                            .menu("复制路径", Box::new(CopyNotePath(rel.clone())))
                    })
                    .into_any_element()
            }
        }
    }
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
            .size_full()
            .when_some(self.error.clone(), |this, err| {
                this.child(div().p_3().text_xs().text_color(cx.theme().danger).child(err))
            })
            .when(self.rows.is_empty() && self.error.is_none(), |this| {
                this.child(
                    div().p_3().text_sm().text_color(muted).child("这个 vault 还没有笔记。按 ⌘N 新建一篇。"),
                )
            })
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
