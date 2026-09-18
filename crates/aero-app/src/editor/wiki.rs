//! 笔记补全的触发、排序与源码生成；编辑器只负责菜单和一次文档编辑。

use std::ops::Range;

use aero_core::VaultPath;
use aero_core::links::{self, Resolution, Resolver};

use crate::quick_open::fuzzy_score;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Trigger {
    pub range: Range<usize>,
    pub cursor: usize,
    pub query: String,
    /// Some 表示片段补全；空目标表示当前笔记。已还原 Markdown 转义。
    pub fragment_target: Option<String>,
    pub block_ref: bool,
    pub embed: bool,
}

/// 用临时闭合的链接交给现有解析器判定上下文，避免在代码、数学、转义、URL 中弹菜单。
pub(super) fn trigger(text: &str, cursor: usize) -> Option<Trigger> {
    let before = text.get(..cursor)?;
    let start = before.rfind("[[")?;
    let embed = start > 0 && before.as_bytes()[start - 1] == b'!';
    let query = &before[start + 2..];
    if query.contains(['[', ']', '\n', '\r', '|']) {
        return None;
    }
    let (target, fragment) = links::split_target(query);
    let block_ref = fragment.as_ref().is_some_and(|f| f.starts_with('^'));
    let fragment_target = fragment.as_ref().map(|_| target.clone());
    let query = fragment.map_or(target, |f| if block_ref { f[1..].into() } else { f });
    let tail = &text[cursor..];
    if let Some(end) = tail.find("]]")
        && !tail[..end].contains(['[', '\n', '\r'])
        && (tail[..end].contains('|')
            || (fragment_target.is_none() && links::split_target(&tail[..end]).1.is_some()))
    {
        return None; // 现有链接带片段 / 别名时暂不补全，保留完整源码。
    }
    // 已经有闭括号（含在现有目标中间编辑）时一并替换，避免留下双重 ]]。
    let end = tail
        .find("]]")
        .filter(|&i| !tail[..i].contains(['[', '\n', '\r', '|']))
        .map_or(cursor, |i| cursor + i + 2);
    let start = if embed { start - 1 } else { start };
    let placeholder = if embed { "![[aero-completion]]" } else { "[[aero-completion]]" };
    let mut probe = text.to_owned();
    probe.replace_range(start..end, placeholder);
    let expected = start..start + placeholder.len();
    let valid = if embed {
        links::scan_embeds(&probe).iter().any(|s| s.range == expected)
    } else {
        links::scan(&probe).iter().any(|s| s.range == expected)
    };
    if !valid {
        return None;
    }
    Some(Trigger { range: start..end, cursor, query, fragment_target, block_ref, embed })
}

pub(super) struct Catalog {
    pub notes: Vec<(VaultPath, String)>,
    pub resolver: Resolver,
    pub attachments: aero_core::attachments::Catalog,
    pub embed_source: Option<VaultPath>,
    recent: Vec<VaultPath>,
}

impl Catalog {
    pub fn new(notes: Vec<(VaultPath, String)>, recent: Vec<VaultPath>) -> Self {
        let mut resolver = Resolver::new();
        for (path, title) in &notes {
            resolver.insert(path.clone(), Some(title));
        }
        Self { notes, resolver, recent, attachments: Default::default(), embed_source: None }
    }

    pub fn filter(&self, query: &str) -> Vec<usize> {
        let query = query.trim();
        let mut scored: Vec<_> = self
            .notes
            .iter()
            .enumerate()
            .filter_map(|(i, (path, title))| {
                let score = fuzzy_score(query, title).max(fuzzy_score(query, path.as_str()))?;
                let recent = self.recent.iter().position(|p| p == path);
                let bonus = recent.map_or(0, |r| 20 - r.min(19) as i32);
                Some((i, score + bonus))
            })
            .collect();
        if let Some(source) = &self.embed_source {
            let assets = source.assets_dir();
            scored.extend(self.attachments.entries.iter().enumerate().filter_map(|(i, entry)| {
                let score = fuzzy_score(query, entry.path.file_name())
                    .max(fuzzy_score(query, entry.path.as_str()))?;
                let bonus = if assets.as_ref().is_some_and(|dir| entry.path.parent() == dir.as_str()) {
                    20
                } else if entry.path.parent() == source.parent() {
                    10
                } else {
                    0
                };
                Some((self.notes.len() + i, score + bonus))
            }));
        }
        let path = |i: usize| {
            if i < self.notes.len() {
                &self.notes[i].0
            } else {
                &self.attachments.entries[i - self.notes.len()].path
            }
        };
        scored.sort_by(|(a, sa), (b, sb)| sb.cmp(sa).then_with(|| path(*a).cmp(path(*b))));
        scored.into_iter().take(8).map(|(i, _)| i).collect()
    }

    /// 先尝试短文件名；有重名时写路径，并用同一解析器验证，不猜目标。
    pub fn target(&self, index: usize, dir: &str) -> Option<String> {
        let (path, title) = self.notes.get(index)?;
        for target in
            [path.stem(), path.as_str().strip_suffix(".md").unwrap_or(path.as_str()), path.as_str(), title]
        {
            // 图片 / PDF 后缀属于附件命名空间；同名笔记使用带 .md 的目标。
            if self.embed_source.is_some() && aero_core::attachments::Kind::for_path(target).is_some() {
                continue;
            }
            if matches!(self.resolver.resolve(target, None, dir), Resolution::Resolved { path: p, .. } if p == *path)
                && let Some(escaped) = links::escape_target(target)
                && links::scan(&format!("[[{escaped}]]")).first().is_some_and(|s| s.target == target)
            {
                return Some(escaped);
            }
        }
        None
    }
}

/// 别名选区是 UTF-8 字节范围，与 Textarea / Document 一致。
pub(super) fn insertion(
    target: &str,
    alias: Option<&str>,
    in_table: bool,
    embed: bool,
) -> (String, Option<Range<usize>>) {
    // 嵌入的 | 后是尺寸等参数，不能写入普通链接的显示别名。
    if embed {
        return (format!("![[{target}]]"), None);
    }
    match alias {
        Some(alias) => {
            let alias = alias
                .replace('\\', "\\\\")
                .replace('[', "\\[")
                .replace(']', "\\]")
                .replace(['\n', '\r', '|'], " ");
            let separator = if in_table { "\\|" } else { "|" };
            let start = 2 + target.len() + separator.len();
            let end = start + alias.len();
            (format!("[[{target}{separator}{alias}]]"), Some(start..end))
        }
        None => (format!("[[{target}]]"), None),
    }
}

pub(super) struct Menu {
    pub trigger: Trigger,
    pub generation: u64,
    pub catalog: Option<Catalog>,
    pub headings: Option<HeadingCatalog>,
    pub blocks: Option<BlockCatalog>,
    pub error: Option<String>,
    pub matches: Vec<usize>,
    pub selected: usize,
}

impl Menu {
    pub fn refilter(&mut self) {
        self.matches = if self.trigger.block_ref {
            self.blocks.as_ref().map_or_else(Vec::new, |b| b.filter(&self.trigger.query))
        } else if self.trigger.fragment_target.is_some() {
            self.headings.as_ref().map_or_else(Vec::new, |h| h.filter(&self.trigger.query))
        } else {
            self.catalog.as_ref().map_or_else(Vec::new, |c| c.filter(&self.trigger.query))
        };
        // 本篇排除正在插链接的块及其容器，避免半成品查询成为自己的候选。
        if let Some(blocks) = &self.blocks
            && blocks.hash.is_none()
        {
            self.matches.retain(|&i| {
                let range = &blocks.entries[i].range;
                !(range.start <= self.trigger.cursor && self.trigger.range.start <= range.end)
            });
        }
        self.matches.truncate(8);
        self.selected = 0;
        if self.catalog.is_some() || self.headings.is_some() || self.blocks.is_some() {
            self.error = None;
        }
    }
}

pub(super) struct BlockCatalog {
    pub path: VaultPath,
    pub entries: Vec<aero_core::block_refs::Candidate>,
    pub hash: Option<u64>,
}

impl BlockCatalog {
    pub fn new(path: VaultPath, doc: &aero_core::Document, hash: Option<u64>) -> Self {
        Self { path, entries: aero_core::block_refs::candidates(doc), hash }
    }

    pub fn filter(&self, query: &str) -> Vec<usize> {
        let mut scored: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, b)| {
                fuzzy_score(query.trim(), &b.excerpt)
                    .max(b.persisted_id.as_deref().and_then(|id| fuzzy_score(query.trim(), id)))
                    .map(|score| (i, score))
            })
            .collect();
        scored.sort_by(|(a, sa), (b, sb)| sb.cmp(sa).then(a.cmp(b)));
        scored.into_iter().map(|(i, _)| i).collect()
    }
}

pub(super) struct HeadingCatalog {
    pub path: VaultPath,
    pub entries: Vec<aero_core::headings::Heading>,
}

impl HeadingCatalog {
    pub fn new(path: VaultPath, doc: &aero_core::Document) -> Self {
        let mut seen = std::collections::HashSet::new();
        let entries = aero_core::headings::list(doc)
            .into_iter()
            // 同文本标题只提供首处，片段链接本身不能区分重复标题。
            .filter(|h| !h.text.is_empty() && seen.insert(h.text.clone()))
            .collect();
        Self { path, entries }
    }

    pub fn filter(&self, query: &str) -> Vec<usize> {
        let mut scored: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, h)| fuzzy_score(query.trim(), &h.text).map(|s| (i, s)))
            .collect();
        scored.sort_by(|(a, sa), (b, sb)| sb.cmp(sa).then(a.cmp(b)));
        scored.into_iter().take(8).map(|(i, _)| i).collect()
    }

    /// 用链接解析器验证片段原样往返，不把无法表示的标题插成另一条链接。
    pub fn target(&self, index: usize, target: &str) -> Option<String> {
        let heading = &self.entries.get(index)?.text;
        let escaped_target = links::escape_target(target)?;
        let fragment = links::escape_target(heading)?;
        let target_source = format!("{escaped_target}#{fragment}");
        let source = format!("[[{target_source}]]");
        links::scan(&source)
            .first()
            .filter(|s| {
                s.range == (0..source.len()) && s.target == target && s.fragment.as_ref() == Some(heading)
            })
            .map(|_| target_source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wiki_trigger_respects_markdown_context_and_utf8() {
        for (text, cursor, expected) in [
            ("中文 [[note", 13, "note"),
            ("[[", 2, ""),
            ("[[My note]]", 6, "My n"),
            ("[[路径/笔记", "[[路径/笔记".len(), "路径/笔记"),
        ] {
            let t = trigger(text, cursor).unwrap();
            assert_eq!(t.query, expected);
            assert_eq!(t.range.end, text.len());
        }
        for text in [
            "`[[abc`",
            "```\n[[abc\n```",
            "$[[abc$",
            "\\[[abc",
            "\\![[abc",
            "[[a|abc",
            "[[a\nabc",
            "[x](https://[[abc)",
            "[[a]]abc",
        ] {
            let cursor = text.find("abc").unwrap() + 3;
            assert!(trigger(text, cursor).is_none(), "{text}");
        }
        assert!(trigger("[[中", 3).is_none());
    }

    #[test]
    fn wiki_candidates_rank_recent_and_match_title_or_path() {
        let p = |s| VaultPath::new(s).unwrap();
        let c = Catalog::new(
            vec![
                (p("a.md"), "Alpha".into()),
                (p("中文/目标.md"), "知识库".into()),
                (p("z.md"), "Zulu".into()),
            ],
            vec![p("z.md")],
        );
        assert_eq!(c.filter("")[0], 2);
        assert_eq!(c.filter("知识"), vec![1]);
        assert_eq!(c.filter("中文/目"), vec![1]);
        assert_eq!(c.filter("al"), vec![0]);
        assert!(c.filter("不存在").is_empty());
    }

    #[test]
    fn wiki_targets_disambiguate_and_escape_fragment_characters() {
        let p = |s| VaultPath::new(s).unwrap();
        let c = Catalog::new(
            vec![(p("a/n.md"), "N".into()), (p("b/n.md"), "N".into()), (p("a/C#.md"), "C#".into())],
            vec![],
        );
        assert_eq!(c.target(0, "a"), Some("n".into()));
        assert_eq!(c.target(1, "a"), Some("b/n".into()));
        assert_eq!(c.target(2, ""), Some("C\\#".into()));
        let (src, selection) = insertion("b/n", Some("中文😀"), false, false);
        assert_eq!(&src[selection.unwrap()], "中文😀");
        assert_eq!(links::scan(&src)[0].alias.as_deref(), Some("中文😀"));
    }

    #[test]
    fn wiki_existing_aliases_and_shadowed_paths_are_not_retargeted() {
        assert!(trigger("[[note#heading]]", 4).is_none());
        assert!(trigger("[[note|alias]]", 4).is_none());
        let p = |s| VaultPath::new(s).unwrap();
        let c = Catalog::new(
            vec![(p("note.md"), "Root title".into()), (p("sub/note.md"), "Local title".into())],
            vec![],
        );
        assert_eq!(c.target(0, "sub"), Some("Root title".into()));
        let c = Catalog::new(vec![(p("note.md"), "note".into()), (p("sub/note.md"), "note".into())], vec![]);
        assert_eq!(c.target(0, "sub"), None, "cannot silently choose the local note");
    }

    #[test]
    fn heading_trigger_decodes_targets_and_preserves_existing_link_parts() {
        for (source, target, query) in
            [("[[#", "", ""), ("[[目录/笔记#小节", "目录/笔记", "小节"), (r"[[C\##语言", "C#", "语言")]
        {
            let t = trigger(source, source.len()).unwrap();
            assert_eq!(t.fragment_target.as_deref(), Some(target));
            assert_eq!(t.query, query);
        }
        let source = "[[note#标题]] 后";
        let t = trigger(source, "[[note#标".len()).unwrap();
        assert_eq!(&source[t.range], "[[note#标题]]");
        assert!(trigger("[[note#标题|别名]]", "[[note#标".len()).is_none());
        for source in ["`[[#h`", "\\![[#h", "\\[[#h", "[url](https://[[#h)"] {
            let cursor = source.rfind('h').unwrap() + 1;
            assert!(trigger(source, cursor).is_none(), "{source}");
        }
        let t = trigger(r"[[C\#", 5).unwrap();
        assert!(t.fragment_target.is_none());
        assert_eq!(t.query, "C#");
    }

    #[test]
    fn heading_catalog_orders_filters_deduplicates_and_validates_fragments() {
        let doc = aero_core::Document::from_text("# Start\n\n## 中文 C#\n\n## Start\n\n### A | B\n");
        let c = HeadingCatalog::new(VaultPath::new("note.md").unwrap(), &doc);
        assert_eq!(c.entries.len(), 3);
        assert_eq!(c.filter(""), vec![0, 1, 2]);
        assert_eq!(c.filter("文c"), vec![1]);
        assert_eq!(c.target(1, "C#"), Some(r"C\##中文 C\#".into()));
        assert_eq!(c.target(0, ""), Some("#Start".into()));
        assert!(c.target(2, "note").is_none());
        assert!(c.filter("不存在").is_empty());
    }

    #[test]
    fn block_ref_trigger_filters_excerpt_and_existing_id() {
        for (src, target, query) in [("[[#^", "", ""), (r"[[C\##^正文", "C#", "正文")] {
            let t = trigger(src, src.len()).unwrap();
            assert!(t.block_ref);
            assert_eq!(t.fragment_target.as_deref(), Some(target));
            assert_eq!(t.query, query);
        }
        let source = "[[note#^old-id]] after";
        let t = trigger(source, "[[note#^ol".len()).unwrap();
        assert_eq!(&source[t.range], "[[note#^old-id]]");
        for source in ["`[[#^x`", "\\![[#^x", "\\[[#^x", "[[n#^x|alias]]"] {
            assert!(trigger(source, source.find('x').unwrap() + 1).is_none());
        }
        let doc = aero_core::Document::from_text("正文一 ^first\n\n正文二 ^second\n");
        let c = BlockCatalog::new(VaultPath::new("note.md").unwrap(), &doc, None);
        assert_eq!(c.filter("二"), vec![1]);
        assert_eq!(c.filter("FIR"), vec![0]);
        assert_eq!(c.filter(""), vec![0, 1]);
    }

    #[test]
    fn embed_trigger_includes_bang_and_preserves_parameters_and_context() {
        for src in ["中文 ![[图", "![[note#标题", "![[#^段落"] {
            let t = trigger(src, src.len()).unwrap();
            assert!(t.embed);
            assert!(src[t.range].starts_with("![["));
        }
        let src = "正文 ![[picture.png]] 后";
        let t = trigger(src, "正文 ![[pic".len()).unwrap();
        assert_eq!(&src[t.range], "![[picture.png]]");
        for src in [
            "\\![[pic",
            "`![[pic`",
            "$![[pic$",
            "[x](https://![[pic)",
            "![[pic.png|400]]",
            "![[pic.pdf#page=3]]",
        ] {
            assert!(trigger(src, src.find("pic").unwrap() + 3).is_none(), "{src}");
        }
        assert_eq!(insertion("photo.png", Some("不是宽度"), true, true), ("![[photo.png]]".into(), None));
    }

    #[test]
    fn embed_catalog_mixes_notes_and_attachments_without_target_collisions() {
        let p = |s| VaultPath::new(s).unwrap();
        let mut c = Catalog::new(
            vec![(p("photo.png.md"), "照片说明".into()), (p("z.md"), "Z".into())],
            vec![p("z.md")],
        );
        c.embed_source = Some(p("a.md"));
        c.attachments =
            aero_core::attachments::Catalog::new(vec![p("a.assets/photo.png"), p("other/paper.pdf")]);
        let matches = c.filter("photo");
        assert_eq!(matches, vec![2, 0]);
        assert_eq!(c.target(0, "").as_deref(), Some("photo.png.md"));
        assert_eq!(c.filter("paper"), vec![3]);
        assert_eq!(c.filter("照片"), vec![0]);
    }
}
