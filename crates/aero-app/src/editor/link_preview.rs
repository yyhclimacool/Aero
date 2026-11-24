use super::*;
use crate::paths::LinkPreviewMode;
use crate::search::SearchService;
use aero_core::links::{Resolution, Resolver};
use gpui_kit::base::text::{LinkStyle, LinkUnderline};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

/// 当前的链接预览设置。工作区从 `state.toml` 写入；编辑器悬停时读取。
#[derive(Clone, Copy, Debug, Default)]
pub struct PreviewMode(pub LinkPreviewMode);

impl gpui_kit::Global for PreviewMode {}

pub fn preview_mode(cx: &App) -> LinkPreviewMode {
    cx.try_global::<PreviewMode>().map(|mode| mode.0).unwrap_or_default()
}

pub fn set_preview_mode(mode: LinkPreviewMode, cx: &mut App) {
    cx.set_global(PreviewMode(mode));
}

#[derive(Default)]
pub(super) struct LinkPreview {
    pub(super) catalog: Option<Arc<crate::catalog::Catalog>>,
    pub(super) resolver: Option<Resolver>,
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
    /// 最近一次见到的 ⌘ 状态。按住 ⌘ 才预览时，目录刷新不能在没按键时把浮层打开。
    platform_down: bool,
    /// 跨篇片段的已完成结果；刷新期间保留，避免虚线随后台任务闪烁。
    fragments: HashMap<(VaultPath, String), FragmentMark>,
    fragment_request: Option<FragmentRequest>,
    fragment_generation: u64,
    fragment_cancel: Option<CancelFragmentScan>,
    fragment_task: Option<Task<()>>,
    #[cfg(test)]
    fragment_reads: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    fragment_scans: Arc<std::sync::atomic::AtomicUsize>,
}

/// 丢弃 Task 无法打断已开始的同步读取；失效、替换请求和编辑器销毁都要通知后台停止。
#[derive(Default)]
struct CancelFragmentScan(Arc<AtomicBool>);

impl Drop for CancelFragmentScan {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FragmentRequest {
    version: u64,
    resolver_generation: u64,
    /// 重载可把文档版本归零；取消后相同版本也不能接收旧结果。
    generation: u64,
}

struct FragmentMark {
    exists: bool,
}

/// 只在后台运行：同一路径的所有片段共用一次读取和解析。取消时不返回部分结果。
fn load_fragment_marks(
    source: &str,
    resolver: &Resolver,
    current_path: &VaultPath,
    cancelled: &AtomicBool,
    mut read: impl FnMut(&VaultPath) -> Option<Document>,
) -> Option<HashMap<(VaultPath, String), FragmentMark>> {
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    let mut needed = Vec::new();
    for span in aero_core::links::scan(source) {
        if cancelled.load(Ordering::Acquire) {
            return None;
        }
        let Some(fragment) = span.fragment.as_deref().filter(|fragment| !fragment.is_empty()) else {
            continue;
        };
        if span.target.is_empty() {
            continue;
        }
        if let Resolution::Resolved { path, .. } =
            resolver.resolve(&span.target, Some(fragment), current_path.parent())
            && path != *current_path
        {
            needed.push((path, fragment.to_owned()));
        }
    }
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    needed.sort();
    needed.dedup();
    let mut marks = HashMap::new();
    for group in needed.chunk_by(|a, b| a.0 == b.0) {
        // 最多让正在读取的一个目标完成，不再读下一个；主线程不用等旧任务退出。
        if cancelled.load(Ordering::Acquire) {
            return None;
        }
        let doc = read(&group[0].0);
        for key in group {
            if cancelled.load(Ordering::Acquire) {
                return None;
            }
            let exists = doc.as_ref().is_some_and(|doc| fragment_range(doc, Some(&key.1)).is_ok());
            marks.insert(key.clone(), FragmentMark { exists });
        }
    }
    (!cancelled.load(Ordering::Acquire)).then_some(marks)
}

struct Popup {
    title: String,
    /// 笔记片段是 Markdown 源码（浮层按预览渲染）；错误、外部链接、歧义是纯文本。
    body: String,
    rich: bool,
    origin: Option<VaultPath>,
    bounds: Bounds<Pixels>,
}

const EXCERPT_CHARS: usize = 300;
const PREVIEW_GAP: Pixels = px(8.);
const PREVIEW_MAX_H: Pixels = px(320.);
/// 标题、内边距和间距占掉的高度，剩下的给正文滚动区。
const PREVIEW_CHROME: Pixels = px(56.);

struct PreviewPlace {
    origin: Point<Pixels>,
    above: bool,
    max_h: Pixels,
    body_max: Pixels,
}

/// 浮层躲开链接本身，也躲开编辑光标那一行。哪一侧更空就放哪一侧，高度不超过那一侧的空隙，
/// 这样贴边时不会被挤回来盖住正在看的文字。
fn place_preview(
    link: Bounds<Pixels>,
    caret: Option<Bounds<Pixels>>,
    viewport: Bounds<Pixels>,
) -> PreviewPlace {
    let below_limit = caret
        .filter(|caret| caret.top() >= link.bottom())
        .map_or(viewport.bottom(), |caret| viewport.bottom().min(caret.top() - PREVIEW_GAP));
    let below = (below_limit - (link.bottom() + PREVIEW_GAP)).max(px(0.));
    let above_limit = caret
        .filter(|caret| caret.bottom() <= link.top())
        .map_or(viewport.top(), |caret| viewport.top().max(caret.bottom() + PREVIEW_GAP));
    let above = ((link.top() - PREVIEW_GAP) - above_limit).max(px(0.));
    let (origin, above, room) = if below >= above {
        (point(link.left(), link.bottom() + PREVIEW_GAP), false, below)
    } else {
        (point(link.left(), link.top() - PREVIEW_GAP), true, above)
    };
    let max_h = room.min(PREVIEW_MAX_H).max(px(1.));
    let body_max = (max_h - PREVIEW_CHROME).max(px(1.));
    PreviewPlace { origin, above, max_h, body_max }
}

/// 悬停预览要渲染的 Markdown。整篇只取大约前 300 个字符（按纯文本计，不切开一个字符）；
/// 标题 / `^块` 取整个片段。front matter 不在叶子里，不会出现。
fn excerpt(doc: &Document, fragment: Option<&str>) -> Result<String, String> {
    let range = fragment_range(doc, fragment)?;
    let leaves: Vec<_> = aero_core::index::indexable_leaves(doc)
        .into_iter()
        .filter(|b| {
            let r = doc.range_of(b);
            r.start >= range.start && r.start < range.end
        })
        .collect();
    let Some(first) = leaves.first() else { return Ok(String::new()) };
    if fragment.is_some_and(|f| !f.is_empty()) {
        return Ok(doc.slice(range).to_string().trim().to_owned());
    }
    // 从包含第一块的根块开始，这样 callout / 列表的标记还在，预览才能渲染成卡片而不是剥掉的正文。
    let leaf_start = doc.range_of(first).start;
    let start = doc
        .blocks()
        .roots
        .iter()
        .find_map(|root| {
            let root_range = doc.range_of(root);
            (root_range.start <= leaf_start && leaf_start < root_range.end).then_some(root_range.start)
        })
        .unwrap_or(leaf_start);
    let mut chars = 0usize;
    let mut end = start;
    for block in &leaves {
        if chars >= EXCERPT_CHARS {
            break;
        }
        let content = doc.content_range(block);
        let src = doc.slice(content.clone()).to_string();
        let plain = aero_core::index::plain_text(&src, &block.kind);
        let room = EXCERPT_CHARS - chars;
        if plain.chars().count() > room {
            let cut = src.char_indices().nth(room).map(|(i, _)| i).unwrap_or(src.len());
            end = content.start + cut;
            break;
        }
        chars += plain.chars().count();
        end = doc.range_of(block).end;
    }
    Ok(doc.slice(start..end).to_string().trim().to_owned())
}

/// 链接 / 嵌入的片段在目标文档里的源码区间：`^id` 是那一块，`#标题` 是该章节（含子标题、
/// 不越过同级），没有片段就是整篇。悬停预览渲染这段 Markdown，嵌入取它的源码。
pub(super) fn fragment_range(doc: &Document, fragment: Option<&str>) -> Result<Range<usize>, String> {
    Ok(if let Some(id) = fragment.and_then(|f| f.strip_prefix('^')) {
        let id = aero_core::block_refs::resolve(doc, id)?;
        let block = doc.blocks().iter().find(|(_, b)| b.id == id).unwrap().1;
        aero_core::properties::owned_range(doc, block.id).unwrap_or_else(|| doc.range_of(block))
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
        self.link_preview.catalog = Some(service.read(cx).catalog.clone());
        self.link_preview.subscription = Some(cx.observe(&service, |this, service, cx| {
            this.refresh_link_resolver(&service, cx);
        }));
        self.refresh_link_resolver(&service, cx);
    }

    fn refresh_link_resolver(&mut self, service: &Entity<SearchService>, cx: &mut Context<Self>) {
        // 保存 / watch 通知也包括目标正文变化，不能只比较目录中的路径和别名。
        self.invalidate_fragment_marks();
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
                this.link_preview.resolver_task = None;
                this.link_preview.resolver = Some(resolver);
                this.update_local_link_names();
                this.refresh_embeds(cx);
                this.refresh_fragment_marks(cx);
                if this.link_preview.hovered.is_some() {
                    this.schedule_link_preview(cx);
                }
                cx.notify();
            });
        }));
    }

    pub(super) fn update_local_link_names(&mut self) {
        self.invalidate_fragment_marks();
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
        self.embeds.clear_content();
        self.cancel_link_preview(cx);
        self.link_preview.resolver_generation += 1;
        self.link_preview.resolver_task = None;
        if let Some(resolver) = &mut self.link_preview.resolver {
            resolver.remove(old_path);
        }
        self.update_local_link_names();
        self.refresh_embeds(cx);
    }

    fn fragment_request(&self) -> FragmentRequest {
        FragmentRequest {
            version: self.doc.version(),
            resolver_generation: self.link_preview.resolver_generation,
            generation: self.link_preview.fragment_generation,
        }
    }

    fn invalidate_fragment_marks(&mut self) {
        self.link_preview.fragment_generation += 1;
        self.link_preview.fragment_request = None;
        self.link_preview.fragment_cancel = None;
        self.link_preview.fragment_task = None;
    }

    fn finish_fragment_marks(
        &mut self,
        request: FragmentRequest,
        marks: HashMap<(VaultPath, String), FragmentMark>,
    ) -> bool {
        if self.is_deleted()
            || self.fragment_request() != request
            || self.link_preview.fragment_request != Some(request)
        {
            return false;
        }
        self.link_preview.fragments = marks;
        self.link_preview.fragment_cancel = None;
        self.link_preview.fragment_task = None;
        true
    }

    /// 每个文档 / 目录版本只排队一次；渲染不扫正文、不读目标文件，也不等待任务。
    pub(super) fn refresh_fragment_marks(&mut self, cx: &mut Context<Self>) {
        if self.is_deleted() || self.link_preview.resolver_task.is_some() {
            return;
        }
        let request = self.fragment_request();
        if self.link_preview.fragment_request == Some(request) {
            return;
        }
        let Some(resolver) = self.link_preview.resolver.clone() else { return };
        self.link_preview.fragment_request = Some(request);
        let cancel = CancelFragmentScan::default();
        let cancelled = cancel.0.clone();
        // 只保留最新的排队任务；旧任务若已经开始，则在下一个检查点退出。
        self.link_preview.fragment_cancel = Some(cancel);
        let source = self.doc.snapshot();
        let path = self.path.clone();
        let vault = self.vault.clone();
        #[cfg(test)]
        let (reads, scans) =
            (self.link_preview.fragment_reads.clone(), self.link_preview.fragment_scans.clone());
        self.link_preview.fragment_task = Some(cx.spawn(async move |this, cx| {
            let Some(marks) = cx
                .background_spawn(async move {
                    if cancelled.load(Ordering::Acquire) {
                        return None;
                    }
                    #[cfg(test)]
                    scans.fetch_add(1, Ordering::SeqCst);
                    load_fragment_marks(&String::from(source.text()), &resolver, &path, &cancelled, |path| {
                        #[cfg(test)]
                        reads.fetch_add(1, Ordering::SeqCst);
                        vault.read(path).ok().map(|(doc, _)| doc)
                    })
                })
                .await
            else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if this.finish_fragment_marks(request, marks) {
                    cx.notify();
                }
            });
        }));
    }

    pub(super) fn preview_link_style(&self, url: &str, cx: &App) -> LinkStyle {
        if let Some((path, _)) = aero_core::links::decode_vault_url(url) {
            let exists = aero_core::links::resolve_relative(self.path.parent(), &path)
                .and_then(|full| aero_core::VaultPath::new(&full).ok())
                .is_some_and(|path| self.vault.fs_path(&path).is_file());
            return if exists {
                LinkStyle { color: Some(cx.theme().link), underline: LinkUnderline::Solid }
            } else {
                LinkStyle { color: Some(cx.theme().muted_foreground), underline: LinkUnderline::Dashed }
            };
        }
        let Some((target, fragment)) = aero_core::links::decode_note_url(url) else {
            return LinkStyle { color: Some(cx.theme().link), underline: LinkUnderline::Solid };
        };
        let resolution = if target.is_empty() {
            if fragment.as_deref().is_some_and(|fragment| fragment_range(&self.doc, Some(fragment)).is_err())
            {
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
            Resolution::Resolved { path, fragment } => {
                let missing =
                    fragment.as_deref().filter(|fragment| !fragment.is_empty()).is_some_and(|fragment| {
                        if path == self.path {
                            fragment_range(&self.doc, Some(fragment)).is_err()
                        } else {
                            self.link_preview
                                .fragments
                                .get(&(path, fragment.to_owned()))
                                .is_some_and(|mark| !mark.exists)
                        }
                    });
                if missing {
                    LinkStyle { color: Some(cx.theme().muted_foreground), underline: LinkUnderline::Dashed }
                } else {
                    LinkStyle { color: Some(cx.theme().link), underline: LinkUnderline::Solid }
                }
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
        self.link_preview.platform_down = window.modifiers().platform;
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

    pub(super) fn preview_modifiers(&mut self, modifiers: Modifiers, cx: &mut Context<Self>) {
        self.link_preview.platform_down = modifiers.platform;
        if preview_mode(cx) != LinkPreviewMode::Command || self.link_preview.hovered.is_none() {
            return;
        }
        if modifiers.platform {
            if self.link_preview.popup.is_none() && self.link_preview.task.is_none() {
                self.schedule_link_preview(cx);
            }
        } else {
            self.close_link_popup(cx);
        }
    }

    pub(crate) fn preview_mode_changed(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.link_preview.platform_down = window.modifiers().platform;
        match preview_mode(cx) {
            LinkPreviewMode::Off => self.cancel_link_preview(cx),
            LinkPreviewMode::Command if !self.link_preview.platform_down => self.close_link_popup(cx),
            LinkPreviewMode::Command | LinkPreviewMode::Hover => {
                if self.link_preview.hovered.is_some()
                    && self.link_preview.popup.is_none()
                    && self.link_preview.task.is_none()
                {
                    self.schedule_link_preview(cx);
                }
            }
        }
    }

    pub(super) fn validate_link_preview_scroll(&mut self, cx: &mut Context<Self>) {
        if self.link_preview.scroll_at_hover.is_some_and(|offset| offset != self.scroll.offset()) {
            self.cancel_link_preview(cx);
        }
    }

    fn schedule_link_preview(&mut self, cx: &mut Context<Self>) {
        let delay = match preview_mode(cx) {
            LinkPreviewMode::Hover => Duration::from_millis(500),
            LinkPreviewMode::Command if self.link_preview.platform_down => Duration::ZERO,
            LinkPreviewMode::Command | LinkPreviewMode::Off => return,
        };
        self.link_preview.generation += 1;
        let generation = self.link_preview.generation;
        self.link_preview.task = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
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
                if this.link_preview.generation == generation
                    && this.link_preview.hovered.is_some()
                    && preview_mode(cx) != LinkPreviewMode::Off
                {
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
        self.invalidate_fragment_marks();
        self.link_preview.resolver_generation += 1;
        self.link_preview.resolver_task = None;
        self.link_preview.resolver = Some(resolver);
        self.refresh_embeds(cx);
        cx.notify();
    }

    fn load_link_preview(&self, url: SharedString, bounds: Bounds<Pixels>, cx: &App) -> Task<Popup> {
        if let Some((path, fragment)) = aero_core::links::decode_vault_url(&url) {
            let title = match &fragment {
                Some(fragment) => format!("{path}#{fragment}"),
                None => path,
            };
            return cx.background_spawn(async move {
                Popup {
                    title,
                    body: "库内链接。单击打开笔记，或在访达中定位文件。".into(),
                    rich: false,
                    origin: None,
                    bounds,
                }
            });
        }
        let Some((target, fragment)) = aero_core::links::decode_note_url(&url) else {
            return cx.background_spawn(async move {
                Popup {
                    title: "外部链接".into(), body: url.to_string(), rich: false, origin: None, bounds
                }
            });
        };
        let resolution = if target.is_empty() {
            Resolution::Resolved { path: self.path.clone(), fragment }
        } else if let Some(resolver) = &self.link_preview.resolver {
            resolver.resolve(&target, fragment.as_deref(), self.path.parent())
        } else {
            return cx.background_spawn(async move {
                Popup {
                    title: target, body: "正在读取笔记目录…".into(), rich: false, origin: None, bounds
                }
            });
        };
        match resolution {
            Resolution::Resolved { path, fragment } if path == self.path => {
                let title = self.title();
                let (body, rich) = match excerpt(&self.doc, fragment.as_deref()) {
                    Ok(body) => (body, true),
                    Err(error) => (error, false),
                };
                cx.background_spawn(async move { Popup { title, body, rich, origin: Some(path), bounds } })
            }
            Resolution::Resolved { path, fragment } => {
                let vault = self.vault.clone();
                cx.background_spawn(async move {
                    match vault.read(&path) {
                        Ok((doc, _)) => {
                            let title = doc.title_or(path.stem());
                            let (body, rich) = match excerpt(&doc, fragment.as_deref()) {
                                Ok(body) => (body, true),
                                Err(error) => (error, false),
                            };
                            Popup { title, body, rich, origin: Some(path), bounds }
                        }
                        Err(error) => Popup {
                            title: path.as_str().into(),
                            body: format!("无法读取笔记：{error}"),
                            rich: false,
                            origin: None,
                            bounds,
                        },
                    }
                })
            }
            Resolution::Unresolved => cx.background_spawn(async move {
                Popup {
                    title: target,
                    body: "未解析链接：目标笔记不存在".into(),
                    rich: false,
                    origin: None,
                    bounds,
                }
            }),
            Resolution::Ambiguous { candidates } => cx.background_spawn(async move {
                Popup {
                    title: format!("歧义链接：{target}"),
                    body: format!(
                        "请使用完整路径消歧：\n{}",
                        candidates.iter().map(VaultPath::as_str).collect::<Vec<_>>().join("\n")
                    ),
                    rich: false,
                    origin: None,
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

    /// 关掉已经打开的浮层，但留下悬停目标：松开 ⌘ 之后再按还能打开。
    fn close_link_popup(&mut self, cx: &mut Context<Self>) {
        self.link_preview.generation += 1;
        self.link_preview.task = None;
        self.link_preview.close_task = None;
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

    fn render_link_preview_body(
        &self,
        body: &str,
        rich: bool,
        origin: Option<VaultPath>,
        max_h: Pixels,
        cx: &Context<Self>,
    ) -> AnyElement {
        let frame = div()
            .id("link-preview-body")
            .max_h(max_h)
            .w_full()
            .overflow_y_scroll()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation());
        if rich && !body.is_empty() {
            let style = self.text_view_style(cx).with_foreground(cx.theme().popover_foreground);
            frame
                .debug_selector(|| "link-preview-rich".into())
                .child(preview::markdown(
                    format!("link-preview-{}", cx.entity_id()).into(),
                    aero_core::links::to_markdown(body).into(),
                    style,
                    preview::PreviewContext {
                        state: self.preview.clone(),
                        editor: cx.entity(),
                        block: BlockId(u64::MAX),
                        reveal: 0,
                        origin,
                    },
                ))
                .into_any_element()
        } else {
            frame
                .debug_selector(|| "link-preview-plain".into())
                .text_sm()
                .line_height(relative(1.6))
                .child(body.to_owned())
                .into_any_element()
        }
    }

    pub(super) fn render_link_preview(&self, window: &Window, cx: &Context<Self>) -> Option<AnyElement> {
        let (title, body, rich, origin, bounds) = {
            let popup = self.link_preview.popup.as_ref()?;
            (popup.title.clone(), popup.body.clone(), popup.rich, popup.origin.clone(), popup.bounds)
        };
        let caret = self.focused.as_ref().and_then(|focused| {
            let at = focused.textarea.read(cx).selected_range().start;
            focused.textarea.read(cx).range_to_bounds(&(at..at))
        });
        let viewport = Bounds::new(point(px(8.), px(8.)), {
            let window_size = window.viewport_size();
            size((window_size.width - px(16.)).max(px(1.)), (window_size.height - px(16.)).max(px(1.)))
        });
        let place = place_preview(bounds, caret, viewport);
        let theme = cx.theme();
        Some(
            deferred(
                anchored()
                    .anchor(if place.above { Anchor::BottomLeft } else { Anchor::TopLeft })
                    .position(place.origin)
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        v_flex()
                            .id("link-preview")
                            .debug_selector(|| "link-preview".into())
                            .occlude()
                            .w(px(360.))
                            .max_w_full()
                            .max_h(place.max_h)
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
                            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(title))
                            .child(self.render_link_preview_body(&body, rich, origin, place.body_max, cx)),
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
    use gpui_kit::{AppContext as _, Bounds, Entity, TestAppContext, point, px};
    use std::{
        sync::{Arc, atomic::Ordering},
        time::Duration,
    };

    fn fragment_editor(cx: &mut TestAppContext, source: &str) -> (tempfile::TempDir, Entity<Editor>) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), source).unwrap();
        let vault = Arc::new(aero_core::Vault::open(dir.path()).unwrap());
        let path = aero_core::VaultPath::new("a.md").unwrap();
        let (doc, stamp) = vault.read(&path).unwrap();
        let editor = cx.new(|cx| Editor::new(vault, path, doc, stamp, cx));
        editor.update(cx, |e, cx| e.install_test_resolver(&[("a.md", None), ("b.md", None)], cx));
        (dir, editor)
    }

    #[gpui_kit::test]
    fn fragment_cancel_during_read_skips_remaining_targets(cx: &mut TestAppContext) {
        let source = "[[d#Old]] [[b#Old]] [[c#Old]] [[b#Other]]\n";
        let (_dir, editor) = fragment_editor(cx, source);
        let (resolver, path, cancelled) = editor.update(cx, |e, cx| {
            e.install_test_resolver(&[("a.md", None), ("b.md", None), ("c.md", None), ("d.md", None)], cx);
            e.refresh_fragment_marks(cx);
            (
                e.link_preview.resolver.clone().unwrap(),
                e.path.clone(),
                e.link_preview.fragment_cancel.as_ref().unwrap().0.clone(),
            )
        });
        let mut reads = Vec::new();
        let marks = super::load_fragment_marks(source, &resolver, &path, &cancelled, |path| {
            reads.push(path.as_str().to_owned());
            if reads.len() == 1 {
                editor.update(cx, |e, _| e.invalidate_fragment_marks());
            }
            Some(Document::from_text("# Old\n\n# Other\n"))
        });
        assert_eq!(reads, ["b.md"], "cancellation must stop the work, not just its publication");
        assert!(marks.is_none(), "cancelled scans must not publish partial marks");
    }

    #[gpui_kit::test]
    fn fragment_new_requests_cancel_running_reads_and_finish_latest(cx: &mut TestAppContext) {
        for action in ["edit", "source-request", "resolver"] {
            let source = "[[d#Old]] [[b#Old]] [[c#Old]]\n";
            let (dir, editor) = fragment_editor(cx, source);
            std::fs::write(dir.path().join("b.md"), "# New\n").unwrap();
            let (resolver, path, cancelled) = editor.update(cx, |e, cx| {
                e.install_test_resolver(
                    &[("a.md", None), ("b.md", None), ("c.md", None), ("d.md", None)],
                    cx,
                );
                e.refresh_fragment_marks(cx);
                (
                    e.link_preview.resolver.clone().unwrap(),
                    e.path.clone(),
                    e.link_preview.fragment_cancel.as_ref().unwrap().0.clone(),
                )
            });
            let mut reads = Vec::new();
            let marks = super::load_fragment_marks(source, &resolver, &path, &cancelled, |path| {
                reads.push(path.as_str().to_owned());
                if reads.len() == 1 {
                    editor.update(cx, |e, cx| {
                        match action {
                            "edit" | "source-request" => {
                                e.doc.edit(aero_core::Edit::replace(0..e.doc.len(), "[[b#New]]\n")).unwrap();
                                if action == "edit" {
                                    e.after_doc_change(cx);
                                    e.save_task = None;
                                }
                            }
                            "resolver" => e.install_test_resolver(&[("a.md", None), ("b.md", None)], cx),
                            _ => unreachable!(),
                        }
                        if action != "source-request" {
                            assert!(cancelled.load(Ordering::Acquire), "{action} must cancel immediately");
                        }
                        e.refresh_fragment_marks(cx);
                        assert!(cancelled.load(Ordering::Acquire));
                        assert!(!e.link_preview.fragment_cancel.as_ref().unwrap().0.load(Ordering::Acquire));
                    });
                }
                Some(Document::from_text("# Old\n"))
            });
            assert_eq!(reads, ["b.md"], "{action} must stop old target reads");
            assert!(marks.is_none());
            cx.run_until_parked();
            editor.update(cx, |e, _| {
                let fragment = if action == "resolver" { "Old" } else { "New" };
                let key = (aero_core::VaultPath::new("b.md").unwrap(), fragment.into());
                assert_eq!(e.link_preview.fragments.len(), 1, "{action}: only latest marks survive");
                assert_eq!(e.link_preview.fragments[&key].exists, action != "resolver");
                assert!(e.link_preview.fragment_task.is_none(), "{action}: latest request must finish");
                assert_eq!(e.link_preview.fragment_reads.load(Ordering::SeqCst), 1);
                assert_eq!(e.link_preview.fragment_scans.load(Ordering::SeqCst), 1);
            });
        }
    }

    #[test]
    fn fragment_cancelled_before_scan_does_not_read() {
        let cancel = super::CancelFragmentScan::default();
        let cancelled = cancel.0.clone();
        drop(cancel);
        let path = aero_core::VaultPath::new("a.md").unwrap();
        let resolver = aero_core::links::Resolver::from_notes([
            path.clone(),
            aero_core::VaultPath::new("b.md").unwrap(),
        ]);
        let marks = super::load_fragment_marks("[[b#Old]]\n", &resolver, &path, &cancelled, |_| {
            panic!("a cancelled scan must not read any targets")
        });
        assert!(marks.is_none());
    }

    #[gpui_kit::test]
    fn fragment_owner_drop_cancels_running_read(cx: &mut TestAppContext) {
        let source = "[[b#Old]] [[c#Old]]\n";
        let (_dir, editor) = fragment_editor(cx, source);
        let (resolver, path, cancelled, scans) = editor.update(cx, |e, cx| {
            e.install_test_resolver(&[("a.md", None), ("b.md", None), ("c.md", None)], cx);
            e.refresh_fragment_marks(cx);
            (
                e.link_preview.resolver.clone().unwrap(),
                e.path.clone(),
                e.link_preview.fragment_cancel.as_ref().unwrap().0.clone(),
                e.link_preview.fragment_scans.clone(),
            )
        });
        let mut editor = Some(editor);
        let mut reads = Vec::new();
        let marks = super::load_fragment_marks(source, &resolver, &path, &cancelled, |path| {
            reads.push(path.as_str().to_owned());
            drop(editor.take());
            // Release the dropped entity without starting any queued background work.
            cx.update(|_| {});
            Some(Document::from_text("# Old\n"))
        });
        assert!(cancelled.load(Ordering::Acquire), "dropping the owner must signal the worker");
        assert_eq!(reads, ["b.md"]);
        assert!(marks.is_none());
        cx.run_until_parked();
        assert_eq!(scans.load(Ordering::SeqCst), 0, "the queued scan must also be cancelled");
    }

    #[gpui_kit::test]
    fn fragment_pending_requests_coalesce_and_latest_completes(cx: &mut TestAppContext) {
        let (dir, editor) = fragment_editor(cx, "[[b#Old]]\n");
        std::fs::write(dir.path().join("b.md"), "# Latest19\n").unwrap();
        editor.update(cx, |e, cx| {
            e.refresh_fragment_marks(cx);
            for version in 0..20 {
                let cancelled = e.link_preview.fragment_cancel.as_ref().unwrap().0.clone();
                e.doc
                    .edit(aero_core::Edit::replace(0..e.doc.len(), format!("[[b#Latest{version}]]\n")))
                    .unwrap();
                e.after_doc_change(cx);
                e.save_task = None;
                e.refresh_fragment_marks(cx);
                assert!(cancelled.load(Ordering::Acquire));
            }
            assert_eq!(e.link_preview.fragment_reads.load(Ordering::SeqCst), 0);
            assert_eq!(e.link_preview.fragment_scans.load(Ordering::SeqCst), 0);
        });
        cx.run_until_parked();
        editor.update(cx, |e, _| {
            assert_eq!(e.link_preview.fragment_reads.load(Ordering::SeqCst), 1);
            assert_eq!(e.link_preview.fragment_scans.load(Ordering::SeqCst), 1);
            assert_eq!(e.link_preview.fragments.len(), 1);
            let key = (aero_core::VaultPath::new("b.md").unwrap(), "Latest19".into());
            assert!(e.link_preview.fragments[&key].exists);
            assert!(e.link_preview.fragment_task.is_none());
            assert!(e.link_preview.fragment_cancel.is_none());
        });
    }

    #[gpui_kit::test]
    fn fragment_refresh_does_no_io_until_background_runs(cx: &mut TestAppContext) {
        let (dir, editor) = fragment_editor(cx, "[[b#Present]]\n");
        std::fs::write(dir.path().join("b.md"), "# Present\n").unwrap();
        editor.update(cx, |e, cx| {
            e.refresh_fragment_marks(cx);
            assert_eq!(
                e.link_preview.fragment_reads.load(Ordering::SeqCst),
                0,
                "render must not read targets"
            );
        });
        cx.run_until_parked();
        editor.update(cx, |e, _| {
            assert_eq!(e.link_preview.fragment_reads.load(Ordering::SeqCst), 1);
            assert!(e.link_preview.fragments.values().all(|mark| mark.exists));
            assert_eq!(e.link_preview.fragments.len(), 1);
        });
    }

    #[gpui_kit::test]
    fn fragment_cache_hits_do_not_rescan_or_read(cx: &mut TestAppContext) {
        let (dir, editor) = fragment_editor(cx, "[[b#Present]]\n");
        std::fs::write(dir.path().join("b.md"), "# Present\n").unwrap();
        editor.update(cx, |e, cx| e.refresh_fragment_marks(cx));
        cx.run_until_parked();
        for _ in 0..20 {
            editor.update(cx, |e, cx| e.refresh_fragment_marks(cx));
            cx.run_until_parked();
        }
        editor.update(cx, |e, _| {
            assert_eq!(
                e.link_preview.fragment_reads.load(Ordering::SeqCst),
                1,
                "cache hits must not read/parse targets"
            );
            assert_eq!(
                e.link_preview.fragment_scans.load(Ordering::SeqCst),
                1,
                "cache hits must not scan the source"
            );
        });
    }

    #[gpui_kit::test]
    fn fragment_refresh_reads_each_target_once(cx: &mut TestAppContext) {
        let (dir, editor) = fragment_editor(
            cx,
            "[[b#Present]] [[b#Missing]] [[b#^block]] [[b#Present]] [[#Local]] [[a#Local]]\n\n# Local\n",
        );
        std::fs::write(dir.path().join("b.md"), "# Present\n\nbody ^block\n").unwrap();
        editor.update(cx, |e, cx| e.refresh_fragment_marks(cx));
        cx.run_until_parked();
        editor.update(cx, |e, _| {
            assert_eq!(
                e.link_preview.fragment_reads.load(Ordering::SeqCst),
                1,
                "one read/parse per target, not per fragment"
            );
            assert_eq!(e.link_preview.fragments.len(), 3);
            let path = aero_core::VaultPath::new("b.md").unwrap();
            for (fragment, exists) in [("Present", true), ("Missing", false), ("^block", true)] {
                assert_eq!(e.link_preview.fragments[&(path.clone(), fragment.into())].exists, exists);
            }
        });
    }

    #[gpui_kit::test]
    fn fragment_missing_target_recovers_after_resolver_refresh(cx: &mut TestAppContext) {
        let (dir, editor) = fragment_editor(cx, "[[b#Present]]\n");
        let key = (aero_core::VaultPath::new("b.md").unwrap(), "Present".into());
        editor.update(cx, |e, cx| e.refresh_fragment_marks(cx));
        cx.run_until_parked();
        editor.update(cx, |e, _| assert!(!e.link_preview.fragments[&key].exists));
        std::fs::write(dir.path().join("b.md"), "# Present\n").unwrap();
        editor.update(cx, |e, cx| {
            e.install_test_resolver(&[("a.md", None), ("b.md", None)], cx);
            e.refresh_fragment_marks(cx);
            assert!(!e.link_preview.fragments[&key].exists, "keep cached marks until refresh completes");
        });
        cx.run_until_parked();
        editor.update(cx, |e, _| {
            assert!(e.link_preview.fragments[&key].exists);
            assert_eq!(e.link_preview.fragment_reads.load(Ordering::SeqCst), 2);
        });
    }

    #[gpui_kit::test]
    fn fragment_source_edit_replaces_obsolete_marks(cx: &mut TestAppContext) {
        let (dir, editor) = fragment_editor(cx, "[[b#Old]]\n");
        std::fs::write(dir.path().join("b.md"), "# Old\n\n# New\n").unwrap();
        editor.update(cx, |e, cx| e.refresh_fragment_marks(cx));
        cx.run_until_parked();
        let old = (aero_core::VaultPath::new("b.md").unwrap(), "Old".into());
        let new = (aero_core::VaultPath::new("b.md").unwrap(), "New".into());
        editor.update(cx, |e, cx| {
            e.doc.edit(aero_core::Edit::replace(0..e.doc.len(), "[[b#New]]\n")).unwrap();
            e.after_doc_change(cx);
            e.save_task = None;
            assert!(e.link_preview.fragments[&old].exists);
            e.refresh_fragment_marks(cx);
        });
        cx.run_until_parked();
        editor.update(cx, |e, _| {
            assert!(e.link_preview.fragments[&new].exists);
            assert!(
                !e.link_preview.fragments.contains_key(&old),
                "completed refresh must prune obsolete marks"
            );
            assert_eq!(e.link_preview.fragment_reads.load(Ordering::SeqCst), 2);
        });
    }

    #[gpui_kit::test]
    fn fragment_late_completion_is_ignored_after_invalidation(cx: &mut TestAppContext) {
        for action in ["edit", "reload", "rename", "delete", "resolver", "cancel"] {
            let (_dir, editor) = fragment_editor(cx, "[[b#Present]]\n");
            let key = (aero_core::VaultPath::new("b.md").unwrap(), "Present".into());
            let request = editor.update(cx, |e, cx| {
                e.refresh_fragment_marks(cx);
                e.link_preview.fragment_request.unwrap()
            });
            cx.run_until_parked();
            editor.update(cx, |e, cx| {
                assert!(!e.link_preview.fragments[&key].exists);
                match action {
                    // No render or after_doc_change: the completion must check the live version too.
                    "edit" => {
                        e.doc.edit(aero_core::Edit::insert(0, "changed ")).unwrap();
                    }
                    // A replacement Document restarts at version zero.
                    "reload" => e.reload(b"replacement\n", e.stamp, cx),
                    "rename" => e.set_path(aero_core::VaultPath::new("renamed.md").unwrap(), cx),
                    "delete" => e.mark_deleted(cx),
                    "resolver" => e.install_test_resolver(&[("a.md", None), ("b.md", None)], cx),
                    "cancel" => e.invalidate_fragment_marks(),
                    _ => unreachable!(),
                }
                let stale =
                    std::collections::HashMap::from([(key.clone(), super::FragmentMark { exists: true })]);
                assert!(!e.finish_fragment_marks(request, stale), "{action} must reject late results");
                assert!(!e.link_preview.fragments[&key].exists, "{action} must not revive stale marks");
            });
        }
    }

    #[gpui_kit::test]
    fn fragment_old_completion_cannot_overwrite_new_request(cx: &mut TestAppContext) {
        let (_dir, editor) = fragment_editor(cx, "[[b#Present]]\n");
        editor.update(cx, |e, cx| {
            e.refresh_fragment_marks(cx);
            let old = e.link_preview.fragment_request.unwrap();
            e.install_test_resolver(&[("a.md", None), ("b.md", None)], cx);
            e.refresh_fragment_marks(cx);
            let current = e.link_preview.fragment_request.unwrap();
            let key = (aero_core::VaultPath::new("b.md").unwrap(), "Present".into());
            assert_ne!(old, current);
            let result =
                |exists| std::collections::HashMap::from([(key.clone(), super::FragmentMark { exists })]);
            assert!(
                !e.finish_fragment_marks(old, result(true)),
                "reject stale work while the new request is pending"
            );
            assert!(e.link_preview.fragment_task.is_some(), "stale work must not cancel the new task");
            assert!(e.finish_fragment_marks(current, result(false)));
            assert!(
                !e.finish_fragment_marks(old, result(true)),
                "reject stale work after the new request completes"
            );
            assert!(!e.link_preview.fragments[&key].exists);
        });
    }

    #[gpui_kit::test]
    fn fragment_resolver_notifications_refresh_unchanged_source(cx: &mut TestAppContext) {
        let (dir, editor) = fragment_editor(cx, "[[b#Present]]\n");
        std::fs::write(dir.path().join("b.md"), "# Missing\n").unwrap();
        let data = tempfile::tempdir().unwrap();
        let vault = editor.update(cx, |e, _| e.vault.clone());
        let service = cx.new(|cx| super::SearchService::new(vault, data.path().into(), cx));
        // Join the OS startup thread; exercise the real observer/resolver with its disk fallback.
        cx.update(|cx| service.read(cx).close_blocking());
        editor.update(cx, |e, cx| e.connect_link_preview(service.clone(), cx));
        cx.run_until_parked();
        let key = (aero_core::VaultPath::new("b.md").unwrap(), "Present".into());
        editor.update(cx, |e, _| assert!(!e.link_preview.fragments[&key].exists));
        let generation = editor.update(cx, |e, _| e.link_preview.resolver_generation);
        for _ in 0..20 {
            service.update(cx, |_, cx| cx.emit(crate::search::SearchEvent::Progress));
        }
        cx.run_until_parked();
        editor.update(cx, |e, _| {
            assert_eq!(e.link_preview.resolver_generation, generation, "progress must not rebuild catalogs");
        });
        for (text, exists) in [
            (Some("# Present\n"), true),
            (Some("# Missing\n"), false),
            (None, false),
            (Some("# Present\n"), true),
        ] {
            match text {
                Some(text) => std::fs::write(dir.path().join("b.md"), text).unwrap(),
                None => std::fs::remove_file(dir.path().join("b.md")).unwrap(),
            }
            // SearchService's save/watch/index completion calls notify, not a source edit.
            service.update(cx, |s, cx| {
                s.catalog.touch([aero_core::VaultPath::new("b.md").unwrap()]);
                cx.notify();
            });
            cx.run_until_parked();
            editor.update(cx, |e, cx| {
                assert_eq!(e.doc.version(), 0, "the source was not edited");
                let url = aero_core::links::encode_note_url("b", Some("Present"));
                assert_eq!(
                    e.preview_link_style(&url, cx).underline,
                    if exists {
                        gpui_kit::base::text::LinkUnderline::Solid
                    } else {
                        gpui_kit::base::text::LinkUnderline::Dashed
                    }
                );
                if text.is_some() {
                    assert_eq!(e.link_preview.fragments[&key].exists, exists);
                } else {
                    assert!(
                        e.link_preview.fragments.is_empty(),
                        "deleted targets are pruned from the catalog"
                    );
                }
            });
        }
    }

    #[gpui_kit::test]
    fn fragment_resolver_refresh_tracks_target_rename(cx: &mut TestAppContext) {
        let (dir, editor) = fragment_editor(cx, "[[b#Present]]\n");
        std::fs::write(dir.path().join("b.md"), "# Present\n").unwrap();
        editor.update(cx, |e, cx| e.refresh_fragment_marks(cx));
        cx.run_until_parked();
        std::fs::rename(dir.path().join("b.md"), dir.path().join("c.md")).unwrap();
        editor.update(cx, |e, cx| {
            e.install_test_resolver(&[("a.md", None), ("c.md", Some("b"))], cx);
            e.refresh_fragment_marks(cx);
        });
        cx.run_until_parked();
        editor.update(cx, |e, _| {
            assert_eq!(e.doc.version(), 0);
            assert_eq!(e.link_preview.fragments.len(), 1);
            let key = (aero_core::VaultPath::new("c.md").unwrap(), "Present".into());
            assert!(e.link_preview.fragments[&key].exists);
            assert_eq!(e.link_preview.fragment_reads.load(Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn fragment_existence_matches_excerpt_validation() {
        for source in [
            "# **Heading**\n\nbody ^block\n",
            "> ## Heading\n> nested\n\nbody ^block\n",
            "# Heading\n",
            "first ^block\n\nsecond ^block\n",
            "---\ntitle: Heading\n---\n",
        ] {
            let doc = Document::from_text(source);
            for fragment in
                [None, Some(""), Some("Heading"), Some("missing"), Some("^block"), Some("^missing")]
            {
                assert_eq!(super::fragment_range(&doc, fragment).is_ok(), excerpt(&doc, fragment).is_ok());
            }
        }
    }

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
    fn missing_cross_note_fragments_are_dashed_before_hover(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.md"), "# 存在\n\n正文\n").unwrap();
        let vault = Arc::new(aero_core::Vault::open(dir.path()).unwrap());
        let dashed = |cx: &mut TestAppContext, source: &str| {
            std::fs::write(dir.path().join("a.md"), source).unwrap();
            let path = aero_core::VaultPath::new("a.md").unwrap();
            let (doc, stamp) = vault.read(&path).unwrap();
            let (editor, visual) =
                cx.add_window_view(|_, cx| Editor::new(vault.clone(), path, doc, stamp, cx));
            visual.update(|_, cx| {
                editor.update(cx, |e, cx| {
                    e.link_preview.resolver = Some(aero_core::links::Resolver::from_notes([
                        aero_core::VaultPath::new("a.md").unwrap(),
                        aero_core::VaultPath::new("b.md").unwrap(),
                    ]));
                    cx.notify();
                })
            });
            // The first render queues fragment checks; settle them without hovering.
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
            visual.update(|window, cx| {
                use gpui_kit::component::ActiveTheme as _;
                window.draw(cx).clear(cx);
                let scale = window.scale_factor();
                let muted = cx.theme().muted_foreground;
                window
                    .painted_quads()
                    .into_iter()
                    .filter(|quad| {
                        quad.background.as_solid() == Some(muted) && quad.bounds.size.height.0 <= scale * 1.1
                    })
                    .count()
            })
        };
        assert_eq!(dashed(cx, "[[b#存在]]\n"), 0, "还在的跨篇标题不是虚线");
        assert!(dashed(cx, "[[b#没有]]\n") > 0, "缺失的跨篇标题不用悬停也是虚线");
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
        visual.update(|_, cx| editor.update(cx, |e, cx| e.check_external(cx)));
        visual.run_until_parked();
        visual.update(|_, cx| {
            editor.update(cx, |e, _| {
                assert!(e.is_deleted());
                assert!(matches!(
                    e.link_preview.resolver.as_ref().unwrap().resolve("local-name", None, ""),
                    Resolution::Unresolved
                ));
            })
        });
        std::fs::rename(&parked, &original).unwrap();
        visual.update(|_, cx| editor.update(cx, |e, cx| e.check_external(cx)));
        visual.run_until_parked();
        visual.update(|_, cx| editor.update(cx, |e, _| {
            assert!(!e.is_deleted());
            assert!(matches!(e.link_preview.resolver.as_ref().unwrap().resolve("local-name", None, ""), Resolution::Resolved { path, .. } if path.as_str() == "a.md"));
        }));
        std::fs::rename(&original, &parked).unwrap();
        visual.update(|_, cx| editor.update(cx, |e, cx| e.check_external(cx)));
        visual.run_until_parked();
        visual.update(|_, cx| editor.update(cx, |e, cx| {
            assert!(e.is_deleted());
            e.save_as(aero_core::VaultPath::new("recovered.md").unwrap(), cx).unwrap();
            let resolver = e.link_preview.resolver.as_ref().unwrap();
            assert!(matches!(resolver.resolve("a", None, ""), Resolution::Unresolved));
            assert!(matches!(resolver.resolve("local-name", None, ""), Resolution::Resolved { path, .. } if path.as_str() == "recovered.md"));
        }));
        std::fs::rename(dir.path().join("recovered.md"), dir.path().join("parked-recovered")).unwrap();
        visual.update(|_, cx| editor.update(cx, |e, cx| e.check_external(cx)));
        visual.run_until_parked();
        visual.update(|_, cx| {
            editor.update(cx, |e, cx| {
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

    #[gpui_kit::test]
    fn hover_preview_renders_the_target_as_markdown(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "见 [[other]]\n").unwrap();
        std::fs::write(dir.path().join("other.md"), "> [!note] Hi\n> **inside**\n").unwrap();
        let vault = Arc::new(aero_core::Vault::open(dir.path()).unwrap());
        let path = aero_core::VaultPath::new("a.md").unwrap();
        let (doc, stamp) = vault.read(&path).unwrap();
        let (editor, visual) = cx.add_window_view(|_, cx| Editor::new(vault, path, doc, stamp, cx));
        let bounds = Bounds::from_corners(point(px(20.), px(20.)), point(px(100.), px(45.)));
        let url = aero_core::links::encode_note_url("other", None).into();
        visual.update(|window, cx| {
            editor.update(cx, |e, cx| {
                e.install_test_resolver(&[("other.md", Some("Other"))], cx);
                e.hover_link(&url, true, bounds, window, cx);
            })
        });
        visual.run_until_parked();
        visual.executor().advance_clock(Duration::from_millis(500));
        visual.run_until_parked();
        visual.update(|window, cx| {
            let (body, rich) = {
                let popup = editor.read(cx).link_preview.popup.as_ref().unwrap();
                (popup.body.clone(), popup.rich)
            };
            assert!(body.contains("**inside**"), "{body}");
            assert!(rich);
            window.draw(cx).clear(cx);
        });
        assert!(visual.debug_bounds("link-preview-rich").is_some());
        assert!(visual.debug_bounds("callout-title-Hi").is_some(), "浮层要按预览渲染 callout，而不是纯文本");
    }

    #[gpui_kit::test]
    fn preview_off_stays_closed_and_command_waits_for_the_key(cx: &mut TestAppContext) {
        use crate::paths::LinkPreviewMode;
        use gpui_kit::Modifiers;
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "body\n").unwrap();
        let vault = Arc::new(aero_core::Vault::open(dir.path()).unwrap());
        let path = aero_core::VaultPath::new("a.md").unwrap();
        let (doc, stamp) = vault.read(&path).unwrap();
        let (editor, visual) = cx.add_window_view(|_, cx| Editor::new(vault, path, doc, stamp, cx));
        let bounds = Bounds::from_corners(point(px(20.), px(20.)), point(px(100.), px(45.)));
        let url = aero_core::links::encode_note_url("", None).into();
        let held = Modifiers { platform: true, control: false, alt: false, shift: false, function: false };
        let released =
            Modifiers { platform: false, control: false, alt: false, shift: false, function: false };
        visual.update(|_, cx| super::set_preview_mode(LinkPreviewMode::Off, cx));
        visual.update(|window, cx| editor.update(cx, |e, cx| e.hover_link(&url, true, bounds, window, cx)));
        visual.run_until_parked();
        visual.executor().advance_clock(Duration::from_millis(500));
        visual.run_until_parked();
        visual.update(|_, cx| assert!(editor.read(cx).link_preview.popup.is_none(), "关闭时不出现浮层"));
        visual.update(|window, cx| {
            super::set_preview_mode(LinkPreviewMode::Command, cx);
            editor.update(cx, |e, cx| {
                e.hover_link(&url, false, bounds, window, cx);
                e.hover_link(&url, true, bounds, window, cx);
            });
        });
        visual.run_until_parked();
        visual.executor().advance_clock(Duration::from_millis(500));
        visual.run_until_parked();
        visual.update(|window, cx| {
            let focus = editor.read(cx).focus_handle.clone();
            focus.focus(window, cx);
            window.draw(cx).clear(cx);
            assert!(editor.read(cx).link_preview.popup.is_none(), "没按 ⌘ 时不出现浮层");
        });
        visual.simulate_modifiers_change(held);
        visual.run_until_parked();
        visual.update(|_, cx| assert_eq!(editor.read(cx).link_preview.popup.as_ref().unwrap().body, "body"));
        visual.simulate_modifiers_change(released);
        visual.run_until_parked();
        visual.update(|_, cx| assert!(editor.read(cx).link_preview.popup.is_none(), "松开 ⌘ 浮层消失"));
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

    #[test]
    fn preview_stays_off_the_link_and_the_caret_line() {
        let viewport = Bounds::from_corners(point(px(0.), px(0.)), point(px(800.), px(600.)));
        let low = Bounds::from_corners(point(px(40.), px(500.)), point(px(120.), px(520.)));
        let flipped = super::place_preview(low, None, viewport);
        assert!(flipped.above, "靠近窗口底部时应翻到链接上方");
        assert!(flipped.origin.y <= low.top());
        let link = Bounds::from_corners(point(px(40.), px(80.)), point(px(120.), px(100.)));
        let caret = Bounds::from_corners(point(px(200.), px(500.)), point(px(202.), px(524.)));
        let placed = super::place_preview(link, Some(caret), viewport);
        assert!(!placed.above, "光标离链接还有空隙时，浮层留在链接下方");
        assert!(placed.origin.y >= link.bottom(), "不能盖住链接那一行");
        assert!(placed.origin.y + placed.max_h <= caret.top(), "不能盖住光标那一行");
        let tight = Bounds::from_corners(point(px(40.), px(108.)), point(px(42.), px(132.)));
        let flipped_off_caret = super::place_preview(link, Some(tight), viewport);
        assert!(flipped_off_caret.above, "光标紧挨着链接时翻到上方，而不是盖住光标");
        assert!(flipped_off_caret.origin.y <= link.top());
    }

    #[test]
    fn excerpt_keeps_inline_markdown_for_the_preview_renderer() {
        let doc = Document::from_text("see **粗** and ==亮==\n");
        let text = excerpt(&doc, None).unwrap();
        assert!(text.contains("**粗**"), "{text}");
        assert!(text.contains("==亮=="), "{text}");
    }
}
