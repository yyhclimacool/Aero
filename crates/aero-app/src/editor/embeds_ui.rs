//! 笔记 / 章节 / 块嵌入的只读渲染（`ui_design.md` 5.6：左侧竖线标识，点击跳到来源笔记）。
//!
//! 嵌入的内容是**源码**，交给同一套预览渲染器画，所以嵌入里的标题、表格、callout、图片都照常渲染。
//! 同篇嵌入直接用未保存的文档；跨篇在后台读盘并缓存，笔记变动时丢缓存重读。
//! 解析与跳转复用 `link_preview` 的解析器，写法上和 `[[链接]]` 完全一致。

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use aero_core::links::Resolution;
use aero_core::{Document, VaultPath};
use gpui_kit::*;

/// 嵌入内容缓存。键是源码里的写法（目标 + 片段），因为解析本身也可能还没就绪。
#[derive(Default)]
pub(super) struct Embeds {
    resolved: HashMap<(VaultPath, Option<String>), Result<SourceSnapshot, String>>,
    pub(super) resolver: Option<aero_core::links::Resolver>,
    pub(super) resolver_sources: u64,
    entries: HashMap<(String, Option<String>), Entry>,
    targets: HashSet<(String, Option<String>)>,
    pub(super) revision: Option<Revision>,
    scan: Option<Task<()>>,
    next_request: u64,
    #[cfg(test)]
    scan_count: usize,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Revision {
    sources: u64,
    epoch: u64,
    version: u64,
    path: VaultPath,
}

impl Embeds {
    pub(super) fn clear_content(&mut self) {
        self.entries.clear();
        self.resolved.clear();
        self.targets.clear();
        self.revision = None;
        self.scan = None;
    }
}

enum Entry {
    /// 持有任务句柄：编辑器关掉时后台读盘随之取消
    Loading {
        path: VaultPath,
        request: u64,
        _task: Task<()>,
    },
    Ready {
        path: VaultPath,
        snapshot: SourceSnapshot,
    },
    Failed {
        path: VaultPath,
        error: SharedString,
    },
}

/// 块元素拿到的嵌入状态。
pub(super) enum EmbedView {
    Loading,
    Ready {
        path: VaultPath,
        fragment: Option<String>,
        title: SharedString,
        source: SharedString,
        excerpt: SharedString,
        conflict: Option<VaultPath>,
    },
    Failed(SharedString),
}

#[cfg(test)]
thread_local! {
    pub(super) static SNAPSHOT_ANALYSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone)]
pub(super) struct SourceSnapshot {
    title: SharedString,
    source: SharedString,
    excerpt: SharedString,
    fragment: Option<String>,
    conflict: Option<VaultPath>,
}

impl SourceSnapshot {
    pub(super) fn new(
        doc: &Document,
        path: &VaultPath,
        fragment: Option<&str>,
        conflict: Option<VaultPath>,
    ) -> Result<Self, String> {
        #[cfg(test)]
        SNAPSHOT_ANALYSES.with(|count| count.set(count.get() + 1));
        let source = slice(doc, fragment)?;
        let fragment = match fragment.and_then(|f| f.strip_prefix('^')) {
            Some(id) => {
                let block = aero_core::block_refs::resolve(doc, id)?;
                doc.blocks().iter().find(|(_, b)| b.id == block)
                    .and_then(|(_, b)| b.persisted_id.as_ref()).map(|id| format!("^{id}"))
                    .or_else(|| fragment.map(str::to_owned))
            }
            None => fragment.filter(|f| !f.is_empty()).map(str::to_owned),
        };
        let plain = aero_core::index::plain_text(&source, &aero_core::BlockKind::Paragraph);
        let flat = plain.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut excerpt: String = flat.chars().take(180).collect();
        if flat.chars().count() > 180 { excerpt.push('…'); }
        Ok(Self { title: doc.title_or(path.stem()).into(), source: source.into(), excerpt: excerpt.into(), fragment, conflict })
    }

    pub(super) fn view(&self, path: VaultPath) -> EmbedView {
        EmbedView::Ready {
            path,
            fragment: self.fragment.clone(),
            title: self.title.clone(),
            source: self.source.clone(),
            excerpt: self.excerpt.clone(),
            conflict: self.conflict.clone(),
        }
    }
}

fn source_targets(source: &str) -> Vec<(String, Option<String>, bool)> {
    let mut targets: Vec<_> = aero_core::links::scan_embeds(source).into_iter()
        .filter(|span| aero_core::attachments::Kind::for_path(&span.target).is_none())
        .map(|span| (span.target, span.fragment, true)).collect();
    targets.extend(aero_core::links::scan(source).into_iter()
        .filter(|span| span.fragment.as_deref().is_some_and(|f| f.starts_with('^')))
        .map(|span| (span.target, span.fragment, false)));
    targets
}

impl super::Editor {
    pub(super) fn cached_source(&self, path: &VaultPath, fragment: Option<&str>) -> EmbedView {
        if !self.embeds.revision.as_ref().is_some_and(|revision|
            revision.epoch == self.io.epoch && revision.version == self.doc.version() && revision.path == self.path)
        {
            return EmbedView::Loading;
        }
        match self.embeds.resolved.get(&(path.clone(), fragment.map(str::to_owned))) {
            Some(Ok(snapshot)) => snapshot.view(path.clone()),
            Some(Err(error)) => EmbedView::Failed(error.clone().into()),
            None if self.embeds.resolved.len() >= 128 => EmbedView::Failed("来源预览超过缓存上限，请前往原文".into()),
            None => EmbedView::Loading,
        }
    }

    pub(super) fn embeds_current(&self, cx: &App) -> bool {
        self.embeds.resolver_sources == super::live_sources::generation(self.vault.id(), cx)
            && self.embeds.revision.as_ref() == Some(&self.embed_revision(cx))
    }

    pub(super) fn embed_view_from(
        &self,
        target: &str,
        fragment: Option<&str>,
        origin: Option<&VaultPath>,
    ) -> EmbedView {
        let origin = origin.unwrap_or(&self.path);
        let resolver = self.embeds.resolver.as_ref().or(self.link_preview.resolver.as_ref());
        let resolution = if target.is_empty() {
            Some(Resolution::Resolved { path: origin.clone(), fragment: fragment.map(str::to_owned) })
        } else {
            resolver.map(|r| r.resolve(target, fragment, origin.parent()))
        };
        if let Some(Resolution::Resolved { path, fragment }) = &resolution {
            if path == &self.path {
                return self.local_source_view(fragment.as_deref());
            }
            return self.cached_source(path, fragment.as_deref());
        }
        match resolution {
            Some(Resolution::Unresolved) => EmbedView::Failed(format!("找不到同步来源：{target}").into()),
            Some(Resolution::Ambiguous { .. }) => EmbedView::Failed("同步来源身份有歧义".into()),
            _ => self.embed_view(target, fragment),
        }
    }
    /// 渲染期只读缓存（渲染中不能改自己）：内容由 [`Self::refresh_embeds`] 预取。
    pub(super) fn embed_view(&self, target: &str, fragment: Option<&str>) -> EmbedView {
        let Some(resolution) = self.resolve_link_target(target, fragment) else {
            return EmbedView::Loading;
        };
        let (path, fragment) = match resolution {
            Resolution::Resolved { path, fragment } => (path, fragment),
            Resolution::Unresolved => {
                return EmbedView::Failed(format!("未解析的嵌入：{target}").into());
            }
            Resolution::Ambiguous { candidates } => {
                let names: Vec<_> = candidates.iter().map(|p| p.as_str()).collect();
                return EmbedView::Failed(format!("{target} 有多个同名笔记：{}", names.join("、")).into());
            }
        };
        // 同篇嵌入用未保存的文档，改一处两处一起变
        if path == self.path {
            return self.local_source_view(fragment.as_deref());
        }
        match self.embeds.entries.get(&(target.to_owned(), fragment.clone())) {
            Some(Entry::Ready { path, snapshot }) => snapshot.view(path.clone()),
            Some(Entry::Failed { error, .. }) => EmbedView::Failed(error.clone()),
            // 还没预取到（刚打开 / 刚改完 / 解析器刚就绪）：占位，不是错误
            None | Some(Entry::Loading { .. }) => EmbedView::Loading,
        }
    }

    /// 预取本篇里所有跨篇嵌入的内容。打开笔记、正文变化、笔记目录就绪、缓存失效后各调一次；
    /// 渲染本身只读缓存。
    pub(super) fn refresh_embeds(&mut self, cx: &mut Context<Self>) {
        if self.embeds.revision.as_ref() == Some(&self.embed_revision(cx)) {
            self.prefetch_embeds(cx);
            return;
        }
        self.refresh_reference_hover(false, cx);
        if self.embeds.scan.is_some() {
            return;
        }
        let debounce = self.embeds.revision.is_some();
        self.embeds.scan = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor().timer(Duration::from_millis(100)).await;
            }
            let Ok((revision, snapshot, vault, directory, overlays)) = this.update(cx, |this, cx| {
                #[cfg(test)]
                {
                    this.embeds.scan_count += 1;
                }
                let mut overlays: HashMap<_, _> = super::Editor::live_editors(this.vault.id(), cx)
                    .into_iter()
                    .filter(|editor| editor.entity_id() != cx.entity_id())
                    .filter_map(|editor| {
                        let editor = editor.read(cx);
                        (!editor.is_deleted()).then(|| (editor.path.clone(), (editor.doc.clone(), editor.source_conflict())))
                    })
                    .collect();
                if !this.is_deleted() {
                    overlays.insert(this.path.clone(), (this.doc.clone(), this.source_conflict()));
                }
                (
                    this.embed_revision(cx),
                    this.doc.snapshot(),
                    this.vault.clone(),
                    this.link_preview.catalog.clone(),
                    overlays,
                )
            }) else {
                return;
            };
            let origin = revision.path.clone();
            let (targets, resolver, resolved) = cx
                .background_spawn(async move {
                    let source = snapshot.text().to_string();
                    let targets = source_targets(&source).into_iter().map(|(target, fragment, _)| (target, fragment)).collect();
                    let mut resolver =
                        directory.and_then(|d| d.snapshot().ok()).map(|s| s.resolver.clone()).unwrap_or_else(
                            || aero_core::links::Resolver::from_notes(vault.notes().unwrap_or_default()),
                        );
                    for (path, (doc, _)) in &overlays {
                        resolver.insert(
                            path.clone(),
                            Some(&doc.title_or(path.stem())),
                            &doc.front_matter_aliases(),
                        );
                        resolver.register_identity(path, doc);
                    }
                    let mut resolved = HashMap::new();
                    let mut expanded = HashSet::new();
                    let mut bytes = 0usize;
                    let mut queue = std::collections::VecDeque::from([(origin, SharedString::from(source), 0u8)]);
                    while let Some((origin, source, depth)) = queue.pop_front() {
                        for (target, fragment, expand) in source_targets(&source) {
                            let resolution = if target.is_empty() {
                                Resolution::Resolved { path: origin.clone(), fragment }
                            } else {
                                resolver.resolve(&target, fragment.as_deref(), origin.parent())
                            };
                            let Resolution::Resolved { path, fragment } = resolution else { continue };
                            let key = (path.clone(), fragment.clone());
                            if !resolved.contains_key(&key) {
                                if resolved.len() >= 128 { continue; }
                                let result = if bytes >= 32 * 1024 * 1024 {
                                    Err("来源预览超过缓存上限，请前往原文".into())
                                } else {
                                    let doc = overlays.get(&path).cloned()
                                        .map(|(doc, conflict)| (std::sync::Arc::new(doc), conflict))
                                        .map(Ok)
                                        .unwrap_or_else(|| super::source_cache::read(&vault, &path)
                                            .map(|doc| (doc, None))
                                            .map_err(|error| format!("无法读取笔记：{error}")));
                                    doc.and_then(|(doc, conflict)| SourceSnapshot::new(&doc, &path, fragment.as_deref(), conflict))
                                        .and_then(|snapshot| {
                                            if snapshot.source.len() > 32 * 1024 * 1024 - bytes {
                                                Err("来源预览超过缓存上限，请前往原文".into())
                                            } else { Ok(snapshot) }
                                        })
                                };
                                if let Ok(snapshot) = &result { bytes += snapshot.source.len(); }
                                resolved.insert(key.clone(), result);
                            }
                            if expand && depth < 3 && expanded.insert(key.clone()) {
                                if let Some(Ok(snapshot)) = resolved.get(&key) {
                                    queue.push_back((path, snapshot.source.clone(), depth + 1));
                                }
                            }
                        }
                    }
                    (targets, resolver, resolved)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.embed_revision(cx) == revision {
                    this.embeds.resolver = Some(resolver);
                    this.embeds.resolver_sources = revision.sources;
                    this.embeds.resolved = resolved;
                }
                this.receive_embed_scan(revision, targets, cx);
            });
        }));
    }

    fn embed_revision(&self, cx: &App) -> Revision {
        Revision {
            sources: super::live_sources::generation(self.vault.id(), cx),
            epoch: self.io.epoch,
            version: self.doc.version(),
            path: self.path.clone(),
        }
    }

    fn receive_embed_scan(
        &mut self,
        revision: Revision,
        targets: HashSet<(String, Option<String>)>,
        cx: &mut Context<Self>,
    ) {
        self.embeds.scan = None;
        if self.embed_revision(cx) != revision {
            self.refresh_embeds(cx);
            return;
        }
        self.embeds.revision = Some(revision);
        self.embeds.targets = targets;
        self.prefetch_embeds(cx);
        self.refresh_reference_hover(true, cx);
        cx.notify();
    }

    fn prefetch_embeds(&mut self, cx: &mut Context<Self>) {
        let wanted = &self.embeds.targets;
        // 不再被引用的嵌入丢掉，免得改来改去越攒越多
        self.embeds.entries.retain(|key, _| wanted.contains(key));
        for (target, fragment) in self.embeds.targets.clone() {
            let key = (target.clone(), fragment.clone());
            if self.embeds.entries.len() >= 128 && !self.embeds.entries.contains_key(&key) { continue; }
            // 同篇嵌入与解析不出的目标都在渲染时即时算，不进缓存
            let Some(Resolution::Resolved { path, fragment }) =
                self.resolve_link_target(&target, fragment.as_deref())
            else {
                self.embeds.entries.remove(&key);
                continue;
            };
            if path == self.path {
                self.embeds.entries.remove(&key);
                continue;
            }
            if let Some(result) = self.embeds.resolved.get(&(path.clone(), fragment.clone())) {
                let entry = match result {
                    Ok(snapshot) => Entry::Ready { path, snapshot: snapshot.clone() },
                    Err(error) => Entry::Failed { path, error: error.clone().into() },
                };
                self.embeds.entries.insert(key, entry);
                continue;
            }
            let cached_path = self.embeds.entries.get(&key).map(|entry| match entry {
                Entry::Loading { path, .. } | Entry::Ready { path, .. } | Entry::Failed { path, .. } => path,
            });
            if cached_path == Some(&path) {
                continue;
            }
            self.embeds.next_request += 1;
            let request = self.embeds.next_request;
            let task = self.load_embed(key.clone(), path.clone(), fragment, request, cx);
            self.embeds.entries.insert(key, Entry::Loading { path, request, _task: task });
        }
    }

    fn load_embed(
        &self,
        key: (String, Option<String>),
        path: VaultPath,
        fragment: Option<String>,
        request: u64,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let vault = self.vault.clone();
        let epoch = self.io.epoch;
        let origin = self.path.clone();
        cx.spawn(async move |this, cx| {
            let read_path = path.clone();
            let read = cx
                .background_spawn(async move {
                    let doc = super::source_cache::read(&vault, &read_path).map_err(|e| format!("无法读取笔记：{e}"))?;
                    SourceSnapshot::new(&doc, &read_path, fragment.as_deref(), None)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.io.epoch != epoch
                    || this.path != origin
                    || !matches!(this.embeds.entries.get(&key), Some(Entry::Loading { request: pending, .. }) if *pending == request)
                {
                    return;
                }
                this.embeds.entries.insert(
                    key,
                    match read {
                        Ok(snapshot) => Entry::Ready { path, snapshot },
                        Err(error) => Entry::Failed { path, error: error.into() },
                    },
                );
                cx.notify();
            });
        })
    }

    /// 别的笔记动了（保存、外部改动、改名、删除）：丢掉它的嵌入缓存与所有失败项，下一帧重读。
    pub fn invalidate_embeds(&mut self, paths: &[VaultPath], cx: &mut Context<Self>) {
        super::source_cache::invalidate(self.vault.id(), paths);
        if let Some(catalog) = &self.link_preview.catalog {
            catalog.touch(paths.iter().cloned());
        }
        // An identity change can invalidate references to other paths too.
        self.embeds.clear_content();
        self.refresh_embeds(cx);
        cx.notify();
    }
}

/// 片段对应的源码；`^id` 是那一块，`#标题` 是那一节，没有片段就是整篇（去掉 front matter）。
pub(super) fn slice(doc: &Document, fragment: Option<&str>) -> Result<String, String> {
    let range = super::link_preview::fragment_range(doc, fragment)?;
    let text = doc.slice(range).to_string();
    let text = match fragment {
        // 整篇嵌入不带 front matter：属性是那篇笔记的，不是这篇的
        None | Some("") => strip_front_matter(&text),
        _ => text.as_str(),
    };
    Ok(text.trim().to_owned())
}

fn strip_front_matter(text: &str) -> &str {
    let rest = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n"));
    match rest.and_then(|rest| rest.split_once("\n---")) {
        Some((_, after)) => after.trim_start_matches(['-', '\r', '\n']),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::{Entry, slice, strip_front_matter};
    use aero_core::{Document, Edit, VaultPath};
    use gpui_kit::{TestAppContext, VisualTestContext};
    use std::time::Duration;

    #[test]
    fn g08_source_snapshot_views_share_prepared_text() {
        let path = VaultPath::new("source.md").unwrap();
        let snapshot = super::SourceSnapshot::new(&Document::from_text("---\ntitle: A source title longer than inline string storage\n---\n\nA source body longer than inline string storage ^p\n"), &path, Some("^p"), None).unwrap();
        let super::EmbedView::Ready { source: first, title: first_title, .. } = snapshot.view(path.clone()) else { panic!() };
        let super::EmbedView::Ready { source: second, title: second_title, .. } = snapshot.view(path) else { panic!() };
        assert_eq!(first.as_ptr(), second.as_ptr(), "views must share prepared source rather than copy it per cell");
        assert_eq!(first_title.as_ptr(), second_title.as_ptr(), "views must share prepared titles");
    }

    #[gpui_kit::test]
    fn g08_reference_refreshes_closed_title_excerpt_without_writing_alias(cx: &mut TestAppContext) {
        let original = "[[source#^p|copied title]]\n";
        let (_dir, vault, destination, editor) = super::super::tests::open(cx, original);
        let path = VaultPath::new("source.md").unwrap();
        for (title, body) in [("First", "old excerpt"), ("Second", "new excerpt")] {
            vault.write_bytes(&path, format!("---\ntitle: {title}\n---\n\n{body} ^p\n").as_bytes()).unwrap();
            editor.update(cx, |e, cx| e.invalidate_embeds(std::slice::from_ref(&path), cx));
            cx.run_until_parked();
            editor.read_with(cx, |e, _| {
                let super::EmbedView::Ready { title: actual, source, .. } = e.embed_view_from("source", Some("^p"), None) else {
                    panic!("ordinary block references must prefetch their source");
                };
                assert_eq!(actual, title);
                assert_eq!(source, format!("{body} ^p"));
                assert_eq!(e.doc.to_string(), original);
                assert_eq!(e.doc.version(), 0);
            });
            assert_eq!(vault.read_bytes(&destination).unwrap().0, original.as_bytes());
        }
    }

    #[gpui_kit::test]
    fn g08_reference_failures_evict_success_and_distinguish_missing_duplicate_read(cx: &mut TestAppContext) {
        let (_dir, vault, _, editor) = super::super::tests::open(cx, "[[source#^p|hint]]\n");
        let path = VaultPath::new("source.md").unwrap();
        let mut errors = Vec::new();
        for bytes in [b"body ^p\n".as_slice(), b"no anchor\n", b"one ^p\n\ntwo ^p\n", &[0xff]] {
            vault.write_bytes(&path, bytes).unwrap();
            editor.update(cx, |e, cx| e.invalidate_embeds(std::slice::from_ref(&path), cx));
            cx.run_until_parked();
            editor.read_with(cx, |e, _| match e.embed_view_from("source", Some("^p"), None) {
                super::EmbedView::Ready { source, .. } if bytes == b"body ^p\n" => assert_eq!(source, "body ^p"),
                super::EmbedView::Failed(error) if bytes != b"body ^p\n" => errors.push(error.to_string()),
                _ => panic!("must resolve to current content or its current error"),
            });
        }
        assert_eq!(errors.len(), 3);
        assert_ne!(errors[0], errors[1]);
        assert_ne!(errors[1], errors[2]);
    }

    #[gpui_kit::test]
    fn g08_full_path_candidate_beats_ambiguous_live_aliases(cx: &mut TestAppContext) {
        use super::super::Editor;
        use gpui_kit::AppContext as _;
        let (_dir, vault, _, editor) = super::super::tests::open(cx, "[[Source#^p]]\n");
        let path = VaultPath::new("Source.md").unwrap();
        vault.write_bytes(&path, b"correct unopened body ^p\n").unwrap();
        let mut aliases = Vec::new();
        for name in ["alias-one.md", "alias-two.md"] {
            let path = VaultPath::new(name).unwrap();
            vault.write_bytes(&path, b"---\naliases: [Source]\n---\n\nwrong body ^p\n").unwrap();
            let (doc, stamp) = vault.read(&path).unwrap();
            aliases.push(cx.new(|cx| Editor::new(vault.clone(), path, doc, stamp, cx)));
        }
        editor.update(cx, |e, cx| e.refresh_embeds(cx));
        cx.run_until_parked();
        editor.read_with(cx, |e, cx| {
            let view = e.live_embed("Source", Some("^p"), None, cx)
                .unwrap_or_else(|| e.embed_view_from("Source", Some("^p"), None));
            assert!(matches!(view, super::EmbedView::Ready { path: actual, source, .. }
                if actual == path && source == "correct unopened body ^p"), "full catalog paths outrank partial live aliases");
        });
    }

    #[gpui_kit::test]
    fn g08_live_source_redraw_reuses_text_and_refreshes_once_per_version(cx: &mut TestAppContext) {
        use super::super::Editor;
        use gpui_kit::AppContext as _;
        let (_dir, vault, _, editor) = super::super::tests::open(cx, "[[source#^p]]\n\n[[source#^p]]\n");
        let path = VaultPath::new("source.md").unwrap();
        vault.write_bytes(&path, b"A source body longer than inline string storage ^p\n").unwrap();
        let (doc, stamp) = vault.read(&path).unwrap();
        let source = cx.new(|cx| Editor::new(vault.clone(), path.clone(), doc, stamp, cx));
        cx.run_until_parked();
        let view_text = |cx: &mut TestAppContext| editor.read_with(cx, |e, cx| {
            let super::EmbedView::Ready { source, .. } = e.live_embed("source", Some("^p"), None, cx).unwrap() else { panic!("prepared live source") };
            source
        });
        let first = view_text(cx);
        let analyses = super::SNAPSHOT_ANALYSES.get();
        for _ in 0..4 {
            let again = view_text(cx);
            assert_eq!(first.as_ptr(), again.as_ptr(), "live views must share the prepared version");
        }
        assert_eq!(super::SNAPSHOT_ANALYSES.get(), analyses, "live lookups must not analyze on paint");
        let scans = editor.read_with(cx, |e, _| e.embeds.scan_count);
        source.update(cx, |e, cx| {
            e.doc.edit(Edit::replace(0..e.doc.len(), "A refreshed body longer than inline string storage ^p\n")).unwrap();
            e.doc.reparse();
            e.publish_live(cx);
        });
        cx.run_until_parked();
        let second = view_text(cx);
        assert_eq!(second, "A refreshed body longer than inline string storage ^p");
        assert_ne!(first.as_ptr(), second.as_ptr());
        let again = view_text(cx);
        assert_eq!(second.as_ptr(), again.as_ptr());
        assert_eq!(editor.read_with(cx, |e, _| e.embeds.scan_count), scans + 1);
    }

    #[gpui_kit::test]
    fn g08_stale_complete_cache_waits_even_when_live_resolution_agrees(cx: &mut TestAppContext) {
        use super::super::Editor;
        use gpui_kit::AppContext as _;
        let (_dir, vault, _, editor) = super::super::tests::open(cx, "[[source#^p]]\n");
        let path = VaultPath::new("source.md").unwrap();
        vault.write_bytes(&path, b"old body ^p\n").unwrap();
        let (doc, stamp) = vault.read(&path).unwrap();
        let source = cx.new(|cx| Editor::new(vault.clone(), path.clone(), doc, stamp, cx));
        cx.run_until_parked();
        source.update(cx, |e, cx| {
            e.doc.edit(Edit::replace(0..e.doc.len(), "one ^p\n\ntwo ^p\n")).unwrap();
            e.doc.reparse();
            e.publish_live(cx);
        });
        editor.read_with(cx, |e, cx| assert!(matches!(e.live_embed("source", Some("^p"), None, cx), Some(super::EmbedView::Loading)), "unvalidated complete snapshots must wait for the latest generation"));
    }

    #[gpui_kit::test]
    fn g08_live_duplicate_identity_never_reuses_success_while_scan_is_pending(cx: &mut TestAppContext) {
        use super::super::Editor;
        use gpui_kit::AppContext as _;
        let document = aero_core::identity::new_document_id();
        let block = aero_core::identity::new_block_id();
        let address = format!("id:{document}");
        let fragment = format!("^{block}");
        let (_dir, vault, _, editor) = super::super::tests::open(cx, &format!("[[{address}#{fragment}|hint]]\n"));
        let path = VaultPath::new("source.md").unwrap();
        let text = format!("---\naero:\n  id: {document}\n---\n\nbody ^{block}\n");
        vault.write_bytes(&path, text.as_bytes()).unwrap();
        let (doc, stamp) = vault.read(&path).unwrap();
        let _source = cx.new(|cx| Editor::new(vault.clone(), path.clone(), doc, stamp, cx));
        cx.run_until_parked();
        editor.read_with(cx, |e, cx| assert!(matches!(e.live_embed(&address, Some(&fragment), None, cx), Some(super::EmbedView::Ready { .. }))));
        let duplicate_path = VaultPath::new("duplicate.md").unwrap();
        vault.write_bytes(&duplicate_path, text.as_bytes()).unwrap();
        let (doc, stamp) = vault.read(&duplicate_path).unwrap();
        cx.update(|cx| {
            let _duplicate = cx.new(|cx| Editor::new(vault.clone(), duplicate_path.clone(), doc, stamp, cx));
            let view = editor.read(cx).live_embed(&address, Some(&fragment), None, cx);
            assert!(!matches!(view, Some(super::EmbedView::Ready { .. })), "identity changes must not display a previously unique body");
        });
        cx.run_until_parked();
        std::fs::remove_file(vault.fs_path(&duplicate_path)).unwrap();
        editor.update(cx, |e, cx| e.invalidate_embeds(std::slice::from_ref(&duplicate_path), cx));
        cx.run_until_parked();
        editor.read_with(cx, |e, cx| assert!(matches!(e.live_embed(&address, Some(&fragment), None, cx), Some(super::EmbedView::Ready { .. })), "released live identities must not cause phantom conflicts"));
    }

    #[gpui_kit::test]
    fn g08_closed_identity_invalidation_rebuilds_catalog_before_reusing_body(cx: &mut TestAppContext) {
        let document = aero_core::identity::new_document_id();
        let block = aero_core::identity::new_block_id();
        let address = format!("id:{document}");
        let fragment = format!("^{block}");
        let (_dir, vault, _, editor) = super::super::tests::open(cx, &format!("[[{address}#{fragment}|hint]]\n"));
        let path = VaultPath::new("source.md").unwrap();
        let duplicate = VaultPath::new("duplicate.md").unwrap();
        let text = format!("---\naero:\n  id: {document}\n---\n\nbody ^{block}\n");
        vault.write_bytes(&path, text.as_bytes()).unwrap();
        let catalog = crate::catalog::Catalog::new(vault.clone());
        editor.update(cx, |e, cx| {
            e.link_preview.catalog = Some(catalog);
            e.invalidate_embeds(std::slice::from_ref(&path), cx);
        });
        cx.run_until_parked();
        editor.read_with(cx, |e, _| assert!(matches!(e.embed_view_from(&address, Some(&fragment), None), super::EmbedView::Ready { .. })));
        vault.write_bytes(&duplicate, text.as_bytes()).unwrap();
        editor.update(cx, |e, cx| {
            e.invalidate_embeds(std::slice::from_ref(&duplicate), cx);
            assert!(!matches!(e.embed_view_from(&address, Some(&fragment), None), super::EmbedView::Ready { .. }));
        });
        cx.run_until_parked();
        editor.read_with(cx, |e, _| assert!(matches!(e.embed_view_from(&address, Some(&fragment), None), super::EmbedView::Failed(error) if error.contains("歧义"))));
        std::fs::remove_file(vault.fs_path(&duplicate)).unwrap();
        editor.update(cx, |e, cx| e.invalidate_embeds(std::slice::from_ref(&duplicate), cx));
        cx.run_until_parked();
        editor.read_with(cx, |e, _| assert!(matches!(e.embed_view_from(&address, Some(&fragment), None), super::EmbedView::Ready { .. })));
    }

    #[gpui_kit::test]
    fn deleted_live_source_invalidates_nested_references_and_recovers(cx: &mut TestAppContext) {
        use super::super::Editor;
        use super::EmbedView;
        let (_dir, vault, _path, target) = super::super::tests::open(cx, "![[middle]]\n");
        let path = VaultPath::new("source.md").unwrap();
        let middle_path = VaultPath::new("middle.md").unwrap();
        vault.create_bytes(&path, b"original source\n").unwrap();
        vault.create_bytes(&middle_path, b"![[source]]\n").unwrap();
        let (doc, stamp) = Editor::load(vault.clone(), path.clone()).unwrap();
        let (source, _) =
            cx.add_window_view(|_, cx| Editor::new(vault.clone(), path.clone(), doc, stamp, cx));
        target.update(cx, |e, cx| e.refresh_embeds(cx));
        cx.run_until_parked();
        for restored in ["original source\n", "new source\n"] {
            target.read_with(cx, |e, cx| {
                assert!(matches!(
                    e.live_embed("source", None, Some(&middle_path), cx),
                    Some(EmbedView::Ready { .. })
                ));
                assert!(matches!(
                    e.embed_view_from("source", None, Some(&middle_path)),
                    EmbedView::Ready { .. }
                ));
            });
            std::fs::remove_file(vault.fs_path(&path)).unwrap();
            source.update(cx, |e, cx| e.mark_deleted(cx));
            target.read_with(cx, |e, cx| {
                assert!(
                    matches!(
                        e.live_embed("source", None, Some(&middle_path), cx),
                        Some(EmbedView::Failed(_))
                    ),
                    "deleted source cannot render retained editor text"
                );
            });
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            target.read_with(cx, |e, _| {
                assert!(
                    matches!(e.embed_view_from("source", None, Some(&middle_path)), EmbedView::Failed(_)),
                    "nested cached source must also become unavailable"
                );
            });
            assert_eq!(
                source.read_with(cx, |e, _| e.doc.to_string()),
                "original source\n",
                "recovery buffer remains available to the user"
            );
            vault.create_bytes(&path, restored.as_bytes()).unwrap();
            source.update(cx, |e, cx| e.check_external(cx));
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            assert!(!source.read_with(cx, |e, _| e.is_deleted()));
            target.read_with(cx, |e, cx| {
                assert!(matches!(e.live_embed("source", None, Some(&middle_path), cx), Some(EmbedView::Ready { source, .. }) if source == restored.trim_end()));
                assert!(matches!(e.embed_view_from("source", None, Some(&middle_path)), EmbedView::Ready { source, .. } if source == restored.trim_end()), "restoring identical bytes must notify subscribers too");
            });
        }
    }

    #[gpui_kit::test]
    fn embed_scans_coalesce_edits_and_reuse_a_current_snapshot(cx: &mut TestAppContext) {
        let (_dir, _vault, _path, editor) = super::super::tests::open(cx, "![[first]]");
        editor.update(cx, |editor, cx| editor.refresh_embeds(cx));
        cx.run_until_parked();
        editor.update(cx, |editor, cx| {
            assert!(editor.embeds.targets.contains(&("first".into(), None)));
            assert_eq!(editor.embeds.scan_count, 1);
            for _ in 0..10 {
                editor.refresh_embeds(cx);
            }
            assert!(editor.embeds.scan.is_none());
            for target in ["middle", "latest"] {
                editor.doc.edit(Edit::replace(0..editor.doc.len(), format!("![[{target}]]"))).unwrap();
                editor.refresh_embeds(cx);
            }
            assert_eq!(editor.embeds.scan_count, 1, "scan must not run in the editing callback");
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        editor.read_with(cx, |editor, _| {
            assert_eq!(editor.embeds.scan_count, 2);
            assert_eq!(editor.embeds.targets.len(), 1);
            assert!(editor.embeds.targets.contains(&("latest".into(), None)));
            assert!(editor.embeds.scan.is_none());
        });
    }

    #[gpui_kit::test]
    fn embed_scan_rejects_a_reply_from_before_reload_even_at_the_same_version(cx: &mut TestAppContext) {
        let (_dir, _vault, _path, editor) = super::super::tests::open(cx, "![[old]]");
        let visual = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        editor.update(visual, |editor, cx| {
            let old = editor.embed_revision(cx);
            let stamp = editor.stamp;
            editor.install_document(Document::from_text("![[new]]"), stamp, cx);
            assert_eq!(old.version, editor.doc.version());
            editor.receive_embed_scan(old, [("old".into(), None)].into_iter().collect(), cx);
            assert!(!editor.embeds.targets.contains(&("old".into(), None)));
        });
        visual.run_until_parked();
        editor.read_with(visual, |editor, _| {
            assert_eq!(editor.embeds.targets.len(), 1);
            assert!(editor.embeds.targets.contains(&("new".into(), None)));
        });
    }

    #[gpui_kit::test]
    fn completed_embed_load_cannot_restore_an_evicted_entry(cx: &mut TestAppContext) {
        let (dir, _vault, _path, editor) = super::super::tests::open(cx, "![[b]]");
        std::fs::write(dir.path().join("b.md"), "target body").unwrap();
        editor.update(cx, |editor, cx| {
            let key = ("b".into(), None);
            let path = VaultPath::new("b.md").unwrap();
            let task = editor.load_embed(key.clone(), path.clone(), None, 1, cx);
            editor.embeds.entries.insert(key.clone(), Entry::Loading { path, request: 1, _task: task });
            let Entry::Loading { _task, .. } = editor.embeds.entries.remove(&key).unwrap() else {
                unreachable!()
            };
            // Let an already accepted load finish after eviction to exercise
            // the callback guard rather than relying on task cancellation.
            _task.detach();
        });
        cx.run_until_parked();
        editor.read_with(cx, |editor, _| assert!(editor.embeds.entries.is_empty()));
    }

    #[test]
    fn embedded_source_keeps_markdown_and_drops_front_matter() {
        let doc = Document::from_text(
            "---\ntitle: T\n---\n\n# 一\n\n正文 **粗**\n\n## 二\n\n表格前 ^mark\n\n| A |\n| --- |\n| x |\n",
        );
        // 整篇：去掉 front matter，其余源码原样（渲染器负责画）
        let whole = slice(&doc, None).unwrap();
        assert!(whole.starts_with("# 一"), "{whole:?}");
        assert!(whole.contains("**粗**") && whole.contains("| --- |"));
        assert!(!whole.contains("title: T"));
        // 章节：含子标题，到下一个同级标题为止
        let first = slice(&doc, Some("一")).unwrap();
        assert!(first.starts_with("# 一") && first.contains("## 二"), "子标题要留在章节里: {first:?}");
        let second = slice(&doc, Some("二")).unwrap();
        assert!(second.starts_with("## 二") && second.contains("| x |"), "{second:?}");
        // 块：正好那一块，`^id` 一起带着（渲染时不显示）
        assert_eq!(slice(&doc, Some("^mark")).unwrap(), "表格前 ^mark");
        // 找不到的片段是错误，不是空内容
        assert!(slice(&doc, Some("没有这节")).is_err());
        assert!(slice(&doc, Some("^nope")).is_err());
        // 同级标题处截断
        let peers = Document::from_text("## 甲\n\n甲正文\n\n## 乙\n\n乙正文\n");
        assert_eq!(slice(&peers, Some("甲")).unwrap(), "## 甲\n\n甲正文");
    }

    #[test]
    fn front_matter_stripping_leaves_documents_without_it_alone() {
        assert_eq!(strip_front_matter("# T\n"), "# T\n");
        assert_eq!(strip_front_matter("---\na: 1\n---\n# T\n"), "# T\n");
        assert_eq!(strip_front_matter("---\r\na: 1\r\n---\r\n# T\r\n"), "# T\r\n");
        // 只有开头的 `---`（没有闭合）不算 front matter，原样保留
        assert_eq!(strip_front_matter("---\nnot closed\n"), "---\nnot closed\n");
    }
}
