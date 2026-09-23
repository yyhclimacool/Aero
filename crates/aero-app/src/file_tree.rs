//! 左侧文件视图（资源管理器）：收藏、vault 名与路径、笔记与目录树；拖放移动、行内重命名。
//!
//! 只显示文件夹与 `.md`（扩展名隐藏）；`*.assets/` 与其他文件由 `Vault::notes()` 过滤掉。
//! 文件夹按名称排序，`daily/` 例外按日期倒序。收藏、vault 标题与树行在同一个 VirtualList 里一起滚动。

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

use crate::actions::{CopyNotePath, OpenInNewTab, RevealInFinder, ToggleFavoriteNote, TrashNote};

pub const ROW_HEIGHT: f32 = 28.;
/// vault 标题行：名字一行、路径一行
const VAULT_ROW_HEIGHT: f32 = 44.;
/// 每深一层缩进的宽度
const INDENT: f32 = 14.;

pub enum FileTreeEvent {
    Open(VaultPath),
    /// ⌘ 点击 / 中键 / 右键「在新标签页打开」
    OpenInNewTab(VaultPath),
    /// 行上的 ☆ 或右键「收藏」
    ToggleFavorite(VaultPath),
    Move {
        from: VaultPath,
        to: VaultPath,
    },
    /// 附件改名改写了这些笔记，或附件被删除。
    Touched(Vec<VaultPath>),
}

actions!(file_tree, [RenameSelection]);

#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = file_tree, no_json)]
pub struct RenameEntry(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = file_tree, no_json)]
pub struct TrashAttachment(pub String);

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

/// 一次磁盘枚举的结果：目录树与每篇笔记的附件。只读磁盘，可以在后台线程做。
struct Scan {
    tree: Result<Folder, String>,
    attachments: BTreeMap<VaultPath, Vec<VaultPath>>,
}

impl Scan {
    fn read(vault: &Vault) -> Scan {
        let tree = vault
            .notes()
            .and_then(|notes| vault.folders().map(|folders| Folder::build(&notes, &folders)))
            .map_err(|e| e.to_string());
        // 附件只按后缀枚举、不读内容；枚举失败不影响笔记树
        let mut attachments: BTreeMap<VaultPath, Vec<VaultPath>> = BTreeMap::new();
        if let Ok(catalog) = aero_core::attachments::Catalog::read(vault) {
            for entry in catalog.entries {
                if let Some(note) = owner_note(&entry.path) {
                    attachments.entry(note).or_default().push(entry.path);
                }
            }
        }
        Scan { tree, attachments }
    }
}

#[derive(Clone, Debug)]
enum Row {
    /// 「收藏」分区标题，可折叠
    FavoritesHeader {
        expanded: bool,
    },
    Favorite {
        path: VaultPath,
    },
    /// 还没有收藏时的提示
    FavoritesEmpty,
    /// vault 名与路径，可折叠；也是「移到 vault 根目录」的落点
    VaultHeader {
        expanded: bool,
    },
    /// vault 里一篇笔记、一个文件夹都没有
    VaultEmpty,
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
    /// 收藏的笔记（工作区持有并持久化，这里只显示）
    favorites: Vec<VaultPath>,
    /// 在某个标签页里打开着的笔记
    opened: HashSet<VaultPath>,
    favorites_collapsed: bool,
    vault_collapsed: bool,
    rename_input: Option<RenameInput>,
    /// 后台重扫（`rescan_in_background`）进行中；期间又来请求就置 `scan_again`
    scanning: Option<Task<()>>,
    scan_again: bool,
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
            favorites: Vec::new(),
            opened: HashSet::new(),
            favorites_collapsed: false,
            vault_collapsed: false,
            rename_input: None,
            scanning: None,
            scan_again: false,
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

    /// Finder 拖进文件树：`.md` 复制进落点目录；其他文件拖到笔记上时进入该笔记的 `.assets/`。
    fn import_external(&mut self, paths: &[std::path::PathBuf], directory: &str, note: Option<&VaultPath>) {
        let mut errors = Vec::new();
        for src in paths {
            if src.is_dir() {
                continue;
            }
            let Some(name) = src.file_name().map(|name| name.to_string_lossy().into_owned()) else {
                continue;
            };
            let bytes = match std::fs::read(src) {
                Ok(bytes) => bytes,
                Err(error) => {
                    errors.push(format!("{name}：{error}"));
                    continue;
                }
            };
            let imported = if name.to_ascii_lowercase().ends_with(".md") {
                let stem = &name[..name.len() - 3];
                let existing = self.vault.notes().unwrap_or_default();
                let dest = unique_note_name(&existing, directory, stem);
                self.vault.create_bytes(&dest, &bytes).map(|_| ()).map_err(|e| e.to_string())
            } else if let Some(note) = note {
                let extension = std::path::Path::new(&name)
                    .extension()
                    .map(|ext| ext.to_string_lossy().into_owned())
                    .unwrap_or_default();
                match aero_core::attachments::import_path(&self.vault, note, Some(&name), &extension) {
                    Some(dest) => {
                        self.vault.create_bytes(&dest, &bytes).map(|_| ()).map_err(|e| e.to_string())
                    }
                    None => Err(format!("无法导入 {name}")),
                }
            } else {
                Err(format!("{name} 不是笔记。请拖到某一篇笔记上，它会进入这篇笔记的附件目录。"))
            };
            if let Err(error) = imported {
                errors.push(error);
            }
        }
        self.rescan();
        self.error = errors.into_iter().next();
    }

    /// 重新枚举磁盘上的笔记和目录（包括空目录）与每篇笔记的附件。用户操作之后调用，调用方紧接着要用新树。
    pub fn rescan(&mut self) {
        let scan = Scan::read(&self.vault);
        self.apply(scan);
    }

    /// 文件监听触发的重扫放到后台：目录遍历在有安全软件逐个审计文件访问的机器上可能要几秒，
    /// 不能卡住界面。扫描中又来请求就在这次结束后再扫一次，同时最多一个在跑。
    pub fn rescan_in_background(&mut self, cx: &mut Context<Self>) {
        if self.scanning.is_some() {
            self.scan_again = true;
            return;
        }
        let vault = self.vault.clone();
        let scan = cx.background_spawn(async move { Scan::read(&vault) });
        self.scanning = Some(cx.spawn(async move |this, cx| {
            let scan = scan.await;
            let _ = this.update(cx, |this, cx| {
                this.scanning = None;
                this.apply(scan);
                cx.notify();
                if std::mem::take(&mut this.scan_again) {
                    this.rescan_in_background(cx);
                }
            });
        }));
    }

    fn apply(&mut self, scan: Scan) {
        match scan.tree {
            Ok(root) => {
                self.root = root;
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
        self.attachments = scan.attachments;
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

    /// 工作区送来的收藏列表与打开中的笔记（标签页）。
    pub fn set_favorites(&mut self, favorites: Vec<VaultPath>, cx: &mut Context<Self>) {
        if self.favorites != favorites {
            self.favorites = favorites;
            self.rebuild_rows();
            cx.notify();
        }
    }

    pub fn set_opened(&mut self, opened: HashSet<VaultPath>, cx: &mut Context<Self>) {
        if self.opened != opened {
            self.opened = opened;
            cx.notify();
        }
    }

    /// 收起所有文件夹（收藏与 vault 分区保持原样）。
    pub fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.expanded.clear();
        self.rebuild_rows();
        cx.notify();
    }

    /// 面包屑点文件夹：展开到它、选中并滚到它那一行。
    pub fn reveal_folder(&mut self, dir: &str, cx: &mut Context<Self>) {
        self.vault_collapsed = false;
        let mut prefix = String::new();
        for seg in dir.split('/').filter(|s| !s.is_empty()) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(seg);
            self.expanded.insert(prefix.clone());
        }
        self.selected = None;
        self.selected_folder = VaultPath::new(dir).ok();
        self.rebuild_rows();
        if let Some(ix) = self.rows.iter().position(|r| matches!(r, Row::Folder { path, .. } if path == dir))
        {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
        }
        cx.notify();
    }

    #[cfg(test)]
    pub fn is_scanning(&self) -> bool {
        self.scanning.is_some()
    }

    /// 树里是否有这篇笔记（上次 `rescan` 时磁盘上的、且没被忽略规则过滤掉）。
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
            self.vault_collapsed = false;
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
        let mut rows = vec![Row::FavoritesHeader { expanded: !self.favorites_collapsed }];
        if !self.favorites_collapsed {
            if self.favorites.is_empty() {
                rows.push(Row::FavoritesEmpty);
            }
            rows.extend(self.favorites.iter().map(|path| Row::Favorite { path: path.clone() }));
        }
        rows.push(Row::VaultHeader { expanded: !self.vault_collapsed });
        if !self.vault_collapsed {
            let before = rows.len();
            walk(&self.root, 0, &self.expanded, &self.attachments, &mut rows);
            if rows.len() == before {
                rows.push(Row::VaultEmpty);
            }
        }
        self.row_sizes = Rc::new(
            rows.iter()
                .map(|row| {
                    let height =
                        if matches!(row, Row::VaultHeader { .. }) { VAULT_ROW_HEIGHT } else { ROW_HEIGHT };
                    size(px(200.), px(height))
                })
                .collect(),
        );
        self.rows = rows;
    }

    fn toggle_section(&mut self, favorites: bool, cx: &mut Context<Self>) {
        if favorites {
            self.favorites_collapsed = !self.favorites_collapsed;
        } else {
            self.vault_collapsed = !self.vault_collapsed;
        }
        self.rebuild_rows();
        cx.notify();
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

    fn open(&mut self, path: VaultPath, new_tab: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(path.clone());
        self.selected_folder = None;
        self.focus_handle.focus(window, cx);
        cx.emit(if new_tab { FileTreeEvent::OpenInNewTab(path) } else { FileTreeEvent::Open(path) });
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
        let attachment = edit.path.as_str().contains(".assets/");
        let name = if edit.directory {
            name.to_owned()
        } else if attachment {
            if name.contains('.') {
                name.to_owned()
            } else {
                match edit.path.as_str().rsplit_once('.') {
                    Some((_, ext)) => format!("{name}.{ext}"),
                    None => name.to_owned(),
                }
            }
        } else if name.ends_with(".md") {
            name.to_owned()
        } else {
            format!("{name}.md")
        };
        let target =
            if edit.path.parent().is_empty() { name } else { format!("{}/{name}", edit.path.parent()) };
        let to = VaultPath::new(&target).expect("validated entry name");
        let from = edit.path.clone();
        self.rename_input = None;
        self.error = None;
        self.focus_handle.focus(window, cx);
        if from != to && attachment {
            match aero_core::attachments::rewrite_references(&self.vault, &from, &to) {
                Ok(notes) => {
                    if let Err(err) = self.vault.rename(&from, &to) {
                        self.error = Some(err.to_string());
                    } else {
                        self.rescan();
                        cx.emit(FileTreeEvent::Touched(notes));
                    }
                }
                Err(err) => self.error = Some(err.to_string()),
            }
        } else if from != to {
            cx.emit(FileTreeEvent::Move { from, to });
        }
        cx.notify();
    }

    fn trash_attachment(&mut self, path: VaultPath, cx: &mut Context<Self>) {
        let file = self.vault.fs_path(&path);
        if let Err(err) = crate::trash::move_to_trash(&file) {
            self.error = Some(err);
        } else {
            self.rescan();
            cx.emit(FileTreeEvent::Touched(Vec::new()));
        }
        cx.notify();
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let hover = theme.list_hover;
        let active = theme.list_active;
        let warning = theme.warning;
        // 树行比 vault 标题深一层
        let indent = |depth: usize| px(8. + (depth + 1) as f32 * INDENT);
        let row_path = match &self.rows[ix] {
            Row::Folder { path, .. } => Some(path.as_str()),
            // 附件不参与行内重命名（改名要连着引用一起改，见 11 节）
            Row::Note { path, .. } | Row::Attachment { path, .. } => Some(path.as_str()),
            _ => None,
        };
        if let Some(edit) = self.rename_input.as_ref().filter(|edit| Some(edit.path.as_str()) == row_path) {
            return div()
                .id("file-tree-rename-input")
                .debug_selector(|| "file-tree-rename-input".into())
                .w_full()
                .h(px(ROW_HEIGHT))
                .child(gpui_kit::component::input::Input::new(&edit.input))
                .into_any_element();
        }
        let chevron = |expanded: bool| {
            Icon::new(if expanded { IconName::ChevronDown } else { IconName::ChevronRight })
                .size(px(14.))
                .text_color(muted)
        };

        match self.rows[ix].clone() {
            Row::FavoritesHeader { expanded } => h_flex()
                .id("favorites-header")
                .debug_selector(|| "file-tree-favorites".into())
                .w_full()
                .h(px(ROW_HEIGHT))
                .pl(px(8.))
                .pr_2()
                .gap_1()
                .items_center()
                .rounded_md()
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(muted)
                .child(chevron(expanded))
                .child(Icon::new(IconName::Star).size(px(13.)).text_color(muted))
                .child(div().flex_1().child("收藏"))
                .when(!self.favorites.is_empty(), |el| el.child(self.favorites.len().to_string()))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_section(true, cx)))
                .into_any_element(),
            Row::FavoritesEmpty => div()
                .w_full()
                .h(px(ROW_HEIGHT))
                .pl(indent(0))
                .pr_2()
                .flex()
                .items_center()
                .text_xs()
                .text_color(muted)
                .truncate()
                .child("还没有收藏。在笔记上点 ☆ 收藏")
                .into_any_element(),
            Row::Favorite { path } => {
                let exists = self.contains(&path);
                let selected = self.selected.as_ref() == Some(&path);
                let (open_path, star_path) = (path.clone(), path.clone());
                let dir = path.parent().to_owned();
                h_flex()
                    .id(("favorite", ix))
                    .debug_selector({
                        let rel = path.as_str().to_owned();
                        move || format!("file-tree-favorite:{rel}")
                    })
                    .group("tree-row")
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .pl(indent(0))
                    .pr_2()
                    .gap_1()
                    .items_center()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |s| s.bg(active))
                    .when(!selected, |s| s.hover(move |s| s.bg(hover)))
                    .text_sm()
                    .child(Icon::new(IconName::StarFill).size(px(13.)).text_color(warning))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .when(!exists, |s| s.text_color(muted).line_through())
                            .child(path.stem().to_owned()),
                    )
                    .child(div().flex_1().min_w_0().truncate().text_xs().text_color(muted).child(dir))
                    .child(self.star_button(ix, &star_path, true, cx))
                    .when(!exists, |el| {
                        el.tooltip(|window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new("这篇笔记已经不在了").build(window, cx)
                        })
                    })
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        let new_tab = event.modifiers().platform || event.is_middle_click();
                        this.open(open_path.clone(), new_tab, window, cx)
                    }))
                    .context_menu({
                        let rel = path.as_str().to_owned();
                        move |menu, _, _| {
                            menu.menu("在新标签页打开", Box::new(OpenInNewTab(rel.clone())))
                                .menu("取消收藏", Box::new(ToggleFavoriteNote(rel.clone())))
                        }
                    })
                    .into_any_element()
            }
            Row::VaultHeader { expanded } => {
                let name = self
                    .vault
                    .root()
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let location = crate::paths::display_path(self.vault.root());
                v_flex()
                    .id("file-tree-root")
                    .debug_selector(|| "file-tree-root".into())
                    .w_full()
                    .h(px(VAULT_ROW_HEIGHT))
                    .pl(px(8.))
                    .pr_2()
                    .justify_center()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(chevron(expanded))
                            .child(div().flex_1().min_w_0().truncate().child(name)),
                    )
                    .child(div().pl(px(18.)).text_xs().text_color(muted).truncate().child(location))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_section(false, cx)))
                    .can_drop({
                        let vault = self.vault.id().to_owned();
                        move |value, _, _| {
                            value.downcast_ref::<gpui_kit::ExternalPaths>().is_some()
                                || value
                                    .downcast_ref::<TreeDrag>()
                                    .is_some_and(|d| d.target(&vault, "").is_some())
                        }
                    })
                    .drag_over::<TreeDrag>({
                        let vault = self.vault.id().to_owned();
                        move |style, drag, _, _| {
                            if drag.target(&vault, "").is_some() { style.bg(active) } else { style }
                        }
                    })
                    .on_drop(cx.listener(|this, drag: &TreeDrag, _, cx| {
                        if let Some(to) = drag.target(this.vault.id(), "") {
                            cx.emit(FileTreeEvent::Move { from: drag.path.clone(), to });
                        }
                    }))
                    .on_drop(cx.listener(|this, paths: &gpui_kit::ExternalPaths, _, cx| {
                        this.import_external(paths.paths(), "", None);
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Row::VaultEmpty => div()
                .w_full()
                .h(px(ROW_HEIGHT))
                .pl(indent(0))
                .pr_2()
                .flex()
                .items_center()
                .text_xs()
                .text_color(muted)
                .truncate()
                .child("这个 vault 还没有笔记。按 ⌘N 新建一篇。")
                .into_any_element(),
            Row::Folder { path, name, depth, expanded } => h_flex()
                .id(("folder", ix))
                .debug_selector({
                    let path = path.clone();
                    move || format!("file-tree-folder:{path}")
                })
                .w_full()
                .h(px(ROW_HEIGHT))
                .pl(indent(depth))
                .pr_2()
                .gap_1()
                .items_center()
                .rounded_md()
                .cursor_pointer()
                .when(self.selected_folder.as_ref().is_some_and(|f| f.as_str() == path), |s| s.bg(active))
                .hover(move |s| s.bg(hover))
                .text_sm()
                .child(chevron(expanded))
                .child(
                    Icon::new(if expanded { IconName::FolderOpen } else { IconName::Folder })
                        .size(px(14.))
                        .text_color(muted),
                )
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
                        value.downcast_ref::<gpui_kit::ExternalPaths>().is_some()
                            || value
                                .downcast_ref::<TreeDrag>()
                                .is_some_and(|d| d.target(&vault, &path).is_some())
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
                .on_drop(cx.listener({
                    let path = path.clone();
                    move |this, paths: &gpui_kit::ExternalPaths, _, cx| {
                        this.import_external(paths.paths(), &path, None);
                        cx.notify();
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
                let opened = self.opened.contains(&path);
                let favorite = self.favorites.contains(&path);
                let rel = path.as_str().to_owned();
                h_flex()
                    .id(("note", ix))
                    .debug_selector({
                        let rel = rel.clone();
                        move || format!("file-tree-note:{rel}")
                    })
                    .group("tree-row")
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .pl(indent(depth))
                    .pr_2()
                    .gap_1()
                    .items_center()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |s| s.bg(active))
                    .when(!selected, |s| s.hover(move |s| s.bg(hover)))
                    .text_sm()
                    // 与文件夹的箭头同宽的一格：打开在某个标签页里的笔记画一个点（4.1 的 ●）
                    .child(div().w(px(14.)).flex_none().flex().justify_center().when(opened, |el| {
                        el.child(
                            div()
                                .debug_selector({
                                    let rel = rel.clone();
                                    move || format!("file-tree-opened:{rel}")
                                })
                                .size(px(5.))
                                .rounded_full()
                                .bg(if selected { theme.foreground } else { muted }),
                        )
                    }))
                    .child(Icon::new(IconName::FileText).size(px(14.)).text_color(if opened {
                        theme.foreground
                    } else {
                        muted
                    }))
                    .child(div().flex_1().truncate().child(path.stem().to_owned()))
                    .child(self.star_button(ix, &path, favorite, cx))
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
                    .can_drop(|value, _, _| value.downcast_ref::<gpui_kit::ExternalPaths>().is_some())
                    .drag_over::<gpui_kit::ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().list_active))
                    .on_drop(cx.listener({
                        let path = path.clone();
                        move |this, paths: &gpui_kit::ExternalPaths, _, cx| {
                            this.import_external(paths.paths(), path.parent(), Some(&path));
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        let new_tab = event.modifiers().platform || event.is_middle_click();
                        this.open(path.clone(), new_tab, window, cx)
                    }))
                    // 右键菜单（4.1 的子集；新建走 ⌘N）
                    .context_menu(move |menu, _, _| {
                        menu.menu("在新标签页打开", Box::new(OpenInNewTab(rel.clone())))
                            .menu(
                                if favorite { "取消收藏" } else { "收藏" },
                                Box::new(ToggleFavoriteNote(rel.clone())),
                            )
                            .separator()
                            .menu("重命名", Box::new(RenameEntry(rel.clone())))
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
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .pl(indent(depth) + px(18.))
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
                    .context_menu({
                        let rel = path.as_str().to_owned();
                        move |menu, _, _| {
                            menu.menu("重命名", Box::new(RenameEntry(rel.clone())))
                                .menu("删除", Box::new(TrashAttachment(rel.clone())))
                        }
                    })
                    .into_any_element()
            }
        }
    }
}

impl FileTree {
    /// 行尾的 ☆：悬停这一行才出现，点它收藏 / 取消收藏，不打开笔记。
    fn star_button(&self, ix: usize, path: &VaultPath, favorite: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (pressed, color) =
            (theme.list_active, if favorite { theme.warning } else { theme.muted_foreground });
        let path = path.clone();
        div()
            .id(("star", ix))
            .debug_selector({
                let rel = path.as_str().to_owned();
                move || format!("file-tree-star:{rel}")
            })
            .flex_none()
            .p(px(2.))
            .rounded_sm()
            .invisible()
            .group_hover("tree-row", |s| s.visible())
            .hover(move |s| s.bg(pressed))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.stop_propagation();
                cx.emit(FileTreeEvent::ToggleFavorite(path.clone()));
            }))
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(if favorite { "取消收藏" } else { "收藏" })
                    .build(window, cx)
            })
            .child(
                Icon::new(if favorite { IconName::StarFill } else { IconName::Star })
                    .size(px(13.))
                    .text_color(color),
            )
            .into_any_element()
    }
}

/// `notes/a.assets/x.png` → `notes/a.md`；不在某篇笔记附件目录里的附件返回 `None`。
fn owner_note(path: &VaultPath) -> Option<VaultPath> {
    let at = path.as_str().find(".assets/")?;
    VaultPath::new(&format!("{}.md", &path.as_str()[..at])).ok()
}

impl Render for FileTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            .on_action(cx.listener(|this, action: &TrashAttachment, _, cx| {
                if let Ok(path) = VaultPath::new(&action.0) {
                    this.trash_attachment(path, cx);
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
    fn dropping_finder_files_imports_notes_and_assets(cx: &mut TestAppContext) {
        let (dir, _vault, tree) = setup(cx);
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("新笔记.md"), "# 新\n").unwrap();
        std::fs::write(outside.path().join("图.png"), b"png").unwrap();
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        draw(cx);
        let drop = |cx: &mut VisualTestContext, selector: &'static str, file: &std::path::Path| {
            draw(cx);
            let at = cx.debug_bounds(selector).expect(selector).center();
            let paths = gpui_kit::ExternalPaths([file.to_path_buf()].into_iter().collect());
            cx.simulate_event(gpui_kit::FileDropEvent::Entered { position: at, paths });
            cx.simulate_event(gpui_kit::FileDropEvent::Submit { position: at });
            draw(cx);
        };
        drop(cx, "file-tree-folder:empty", &outside.path().join("新笔记.md"));
        let err = cx.update(|_, cx| tree.read(cx).error.clone());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("empty/新笔记.md")).map_err(|e| e.to_string()),
            Ok("# 新\n".into()),
            "tree error: {err:?}"
        );
        drop(cx, "file-tree-note:source/a.md", &outside.path().join("图.png"));
        assert_eq!(std::fs::read(dir.path().join("source/a.assets/图.png")).unwrap(), b"png");
        drop(cx, "file-tree-folder:empty", &outside.path().join("图.png"));
        assert!(!dir.path().join("empty/图.png").exists(), "文件夹上不接收非笔记文件");
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
                super::FileTreeEvent::Open(_) | super::FileTreeEvent::OpenInNewTab(_) => {
                    *opened.borrow_mut() += 1
                }
                super::FileTreeEvent::Touched(_) | super::FileTreeEvent::ToggleFavorite(_) => {}
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

    /// 资源管理器（4.1）：收藏分区、打开中的笔记带点、⌘ 点击发「新标签页打开」、行尾 ☆ 只切换收藏。
    #[gpui_kit::test]
    fn favorites_opened_markers_and_star_buttons(cx: &mut TestAppContext) {
        use std::{cell::RefCell, collections::HashSet, rc::Rc};
        let (_dir, _vault, tree) = setup(cx);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let events = Rc::new(RefCell::new(Vec::<String>::new()));
        cx.update(|_, cx| {
            let sink = events.clone();
            cx.subscribe(&tree, move |_, event, _| {
                sink.borrow_mut().push(match event {
                    super::FileTreeEvent::Open(p) => format!("open:{p}"),
                    super::FileTreeEvent::OpenInNewTab(p) => format!("new-tab:{p}"),
                    super::FileTreeEvent::ToggleFavorite(p) => format!("star:{p}"),
                    _ => "other".into(),
                })
            })
            .detach();
        });
        draw(cx);
        assert!(cx.update(|_, cx| tree.read(cx).rows.iter().any(|r| matches!(r, Row::FavoritesEmpty))));
        let a = aero_core::VaultPath::new("source/a.md").unwrap();
        let root = aero_core::VaultPath::new("root.md").unwrap();
        cx.update(|_, cx| {
            tree.update(cx, |t, cx| {
                t.set_favorites(vec![a.clone()], cx);
                t.set_opened(HashSet::from([root.clone()]), cx);
            })
        });
        draw(cx);
        assert!(cx.debug_bounds("file-tree-opened:root.md").is_some(), "打开中的笔记带点");
        assert!(cx.debug_bounds("file-tree-opened:source/a.md").is_none());

        click(cx, "file-tree-favorite:source/a.md");
        let row = cx.debug_bounds("file-tree-note:root.md").unwrap().center();
        cx.simulate_click(row, Modifiers::command());
        draw(cx);
        assert_eq!(&*events.borrow(), &["open:source/a.md", "new-tab:root.md"]);

        // ☆ 在悬停的行上出现；点它不打开笔记
        cx.simulate_mouse_move(row, None, Modifiers::default());
        draw(cx);
        click(cx, "file-tree-star:root.md");
        assert_eq!(events.borrow().last().map(String::as_str), Some("star:root.md"));
        assert_eq!(events.borrow().len(), 3);

        click(cx, "file-tree-favorites");
        assert!(cx.debug_bounds("file-tree-favorite:source/a.md").is_none(), "收藏分区可以收起");
        click(cx, "file-tree-root");
        assert!(cx.debug_bounds("file-tree-note:root.md").is_none(), "vault 分区可以收起");
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
