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
}

/// 用临时闭合的链接交给现有解析器判定上下文，避免在代码、数学、转义、URL 中弹菜单。
pub(super) fn trigger(text: &str, cursor: usize) -> Option<Trigger> {
    let before = text.get(..cursor)?;
    let start = before.rfind("[[")?;
    let query = &before[start + 2..];
    if query.contains(['[', ']', '\n', '\r', '#', '|']) {
        return None;
    }
    let tail = &text[cursor..];
    if let Some(end) = tail.find("]]")
        && !tail[..end].contains(['[', '\n', '\r'])
        && tail[..end].contains(['#', '|'])
    {
        return None; // 现有链接带片段 / 别名时暂不补全，保留完整源码。
    }
    // 已经有闭括号（含在现有目标中间编辑）时一并替换，避免留下双重 ]]。
    let end = tail
        .find("]]")
        .filter(|&i| !tail[..i].contains(['[', '\n', '\r', '#', '|']))
        .map_or(cursor, |i| cursor + i + 2);
    let mut probe = text.to_owned();
    probe.replace_range(start..end, "[[aero-completion]]");
    if !links::scan(&probe).iter().any(|s| s.range == (start..start + "[[aero-completion]]".len())) {
        return None;
    }
    Some(Trigger { range: start..end, cursor, query: query.to_owned() })
}

pub(super) struct Catalog {
    pub notes: Vec<(VaultPath, String)>,
    pub resolver: Resolver,
    recent: Vec<VaultPath>,
}

impl Catalog {
    pub fn new(notes: Vec<(VaultPath, String)>, recent: Vec<VaultPath>) -> Self {
        let mut resolver = Resolver::new();
        for (path, title) in &notes {
            resolver.insert(path.clone(), Some(title));
        }
        Self { notes, resolver, recent }
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
        scored.sort_by(|(a, sa), (b, sb)| sb.cmp(sa).then_with(|| self.notes[*a].0.cmp(&self.notes[*b].0)));
        scored.into_iter().take(8).map(|(i, _)| i).collect()
    }

    /// 先尝试短文件名；有重名时写路径，并用同一解析器验证，不猜目标。
    pub fn target(&self, index: usize, dir: &str) -> Option<String> {
        let (path, title) = self.notes.get(index)?;
        for target in
            [path.stem(), path.as_str().strip_suffix(".md").unwrap_or(path.as_str()), path.as_str(), title]
        {
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
pub(super) fn insertion(target: &str, alias: Option<&str>, in_table: bool) -> (String, Option<Range<usize>>) {
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
    pub error: Option<String>,
    pub matches: Vec<usize>,
    pub selected: usize,
}

impl Menu {
    pub fn refilter(&mut self) {
        self.matches = self.catalog.as_ref().map_or_else(Vec::new, |c| c.filter(&self.trigger.query));
        self.selected = 0;
        if self.catalog.is_some() {
            self.error = None;
        }
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
            "![[abc",
            "[[a|abc",
            "[[a#abc",
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
        let (src, selection) = insertion("b/n", Some("中文😀"), false);
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
}
