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
    resolved: HashMap<(VaultPath, Option<String>), Result<(String, String), String>>,
    pub(super) resolver: Option<aero_core::links::Resolver>,
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
        title: String,
        source: String,
    },
    Failed {
        path: VaultPath,
        error: SharedString,
    },
}

/// 块元素拿到的嵌入状态。
pub(super) enum EmbedView {
    Loading,
    Ready { path: VaultPath, fragment: Option<String>, title: String, source: String },
    Failed(SharedString),
}

impl super::Editor {
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
                return match slice(&self.doc, fragment.as_deref()) {
                    Ok(source) => EmbedView::Ready {
                        path: path.clone(),
                        fragment: fragment.clone(),
                        title: self.title(),
                        source,
                    },
                    Err(error) => EmbedView::Failed(error.into()),
                };
            }
            if let Some(result) = self.embeds.resolved.get(&(path.clone(), fragment.clone())) {
                return match result {
                    Ok((title, source)) => EmbedView::Ready {
                        path: path.clone(),
                        fragment: fragment.clone(),
                        title: title.clone(),
                        source: source.clone(),
                    },
                    Err(error) => EmbedView::Failed(error.clone().into()),
                };
            }
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
            return match slice(&self.doc, fragment.as_deref()) {
                Ok(source) => EmbedView::Ready { path, fragment, title: self.title(), source },
                Err(error) => EmbedView::Failed(error.into()),
            };
        }
        match self.embeds.entries.get(&(target.to_owned(), fragment.clone())) {
            Some(Entry::Ready { path, title, source }) => EmbedView::Ready {
                path: path.clone(),
                fragment,
                title: title.clone(),
                source: source.clone(),
            },
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
                        (!editor.is_deleted()).then(|| (editor.path.clone(), editor.doc.clone()))
                    })
                    .collect();
                if !this.is_deleted() {
                    overlays.insert(this.path.clone(), this.doc.clone());
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
                    let targets = aero_core::links::scan_embeds(&source)
                        .into_iter()
                        .filter(|embed| aero_core::attachments::Kind::for_path(&embed.target).is_none())
                        .map(|embed| (embed.target, embed.fragment))
                        .collect();
                    let mut resolver =
                        directory.and_then(|d| d.snapshot().ok()).map(|s| s.resolver.clone()).unwrap_or_else(
                            || aero_core::links::Resolver::from_notes(vault.notes().unwrap_or_default()),
                        );
                    for (path, doc) in &overlays {
                        resolver.insert(
                            path.clone(),
                            Some(&doc.title_or(path.stem())),
                            &doc.front_matter_aliases(),
                        );
                        resolver.register_identity(path, doc);
                    }
                    let mut resolved = HashMap::new();
                    let mut queue = std::collections::VecDeque::from([(origin, source, 0u8)]);
                    while let Some((origin, source, depth)) = queue.pop_front() {
                        if depth == 3 {
                            continue;
                        }
                        for embed in aero_core::links::scan_embeds(&source) {
                            if aero_core::attachments::Kind::for_path(&embed.target).is_some() {
                                continue;
                            }
                            let resolution = if embed.target.is_empty() {
                                Resolution::Resolved { path: origin.clone(), fragment: embed.fragment }
                            } else {
                                resolver.resolve(&embed.target, embed.fragment.as_deref(), origin.parent())
                            };
                            let Resolution::Resolved { path, fragment } = resolution else {
                                continue;
                            };
                            let key = (path.clone(), fragment.clone());
                            if resolved.contains_key(&key) {
                                continue;
                            }
                            let doc = overlays
                                .get(&path)
                                .cloned()
                                .map(std::sync::Arc::new)
                                .map(Ok)
                                .unwrap_or_else(|| super::source_cache::read(&vault, &path));
                            let result = doc.and_then(|doc| {
                                slice(&doc, fragment.as_deref())
                                    .map(|source| (doc.title_or(path.stem()), source))
                            });
                            if let Ok((_, source)) = &result {
                                queue.push_back((path, source.clone(), depth + 1));
                            }
                            resolved.insert(key, result);
                        }
                    }
                    (targets, resolver, resolved)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.embed_revision(cx) == revision {
                    this.embeds.resolver = Some(resolver);
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
        cx.notify();
    }

    fn prefetch_embeds(&mut self, cx: &mut Context<Self>) {
        let wanted = &self.embeds.targets;
        // 不再被引用的嵌入丢掉，免得改来改去越攒越多
        self.embeds.entries.retain(|key, _| wanted.contains(key));
        for (target, fragment) in self.embeds.targets.clone() {
            let key = (target.clone(), fragment.clone());
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
                    Ok((title, source)) => {
                        Entry::Ready { path, title: title.clone(), source: source.clone() }
                    }
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
                    let source = slice(&doc, fragment.as_deref())?;
                    let title = doc.title_or(read_path.stem());
                    Ok::<_, String>((title, source))
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
                        Ok((title, source)) => Entry::Ready { path, title, source },
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
        // An external source event can arrive while the old source is being
        // read. Dropping the owner prevents that result from being applied.
        self.embeds.scan = None;
        self.embeds.revision = None;
        if self.embeds.resolved.keys().any(|(path, _)| paths.contains(path)) {
            self.embeds.resolved.retain(|(path, _), _| !paths.contains(path));
            self.embeds.revision = None;
            self.refresh_embeds(cx);
        }
        let before = self.embeds.entries.len();
        self.embeds.entries.retain(|_, entry| match entry {
            Entry::Ready { path, .. } => !paths.contains(path),
            Entry::Failed { .. } => false,
            Entry::Loading { path, .. } => !paths.contains(path),
        });
        if self.embeds.entries.len() != before {
            self.refresh_embeds(cx);
            cx.notify();
        }
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
