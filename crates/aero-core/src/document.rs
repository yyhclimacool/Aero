//! Document：一份 Rope（唯一文本真相）+ marker 表 + Block 树（方案 7.2 / 7.4，ADR 0004）。
//!
//! 编辑管线分两段：
//! - 同步段：[`Document::edit`] 改 Rope、平移 anchor、记 undo。块树保持旧结构，但区间已跟着漂移，
//!   UI 可以立刻按新区间重绘聚焦块。
//! - 异步段：[`Document::snapshot`] 拿一个 O(1) 快照，在任意线程 [`Snapshot::parse`]，
//!   再用 [`Document::apply_parse`] 把结果装回；若期间又有编辑（version 不符）则结果作废，
//!   由调用方重新快照。小文档直接 [`Document::reparse`] 同步完成。

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::ops::Range;

use crate::anchors::MarkerTable;
use crate::block::{Block, BlockId, BlockKind, BlockTree};
use crate::parse::{self, ParseOutput, ParseReport, ParsedBlock};
use crate::text::{self, Edit, EditError, LineEnding, Rope, RopeSlice, SourceText};

/// 文本快照，可跨线程解析。
#[derive(Clone, Debug)]
pub struct Snapshot {
    text: Rope,
    version: u64,
}

impl Snapshot {
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn text(&self) -> &Rope {
        &self.text
    }

    /// 物化 + 解析。10 MB 文档物化 ≈ 0.7 ms，解析按 ~18 ms/MB 估（ADR 0002 / 0004）。
    pub fn parse(&self) -> ParseOutput {
        parse::parse(&String::from(&self.text))
    }
}

#[derive(Clone, Debug)]
pub struct Document {
    text: Rope,
    anchors: MarkerTable,
    blocks: BlockTree,
    /// 每次编辑 +1
    version: u64,
    /// 当前块树对应的文本版本
    parsed_version: u64,
    next_id: u64,
    /// 逆编辑组；每组按顺序应用即可撤销一次操作
    undo: Vec<Vec<Edit>>,
    redo: Vec<Vec<Edit>>,
    pub bom: bool,
    pub line_ending: LineEnding,
    /// 最近一次解析的后处理统计
    pub report: ParseReport,
}

impl Default for Document {
    fn default() -> Self {
        Self::from_source(SourceText::fresh(""))
    }
}

impl Document {
    pub fn from_text(text: &str) -> Self {
        Self::from_source(SourceText::fresh(text))
    }

    /// 从文件字节构造：去 BOM、CRLF → LF，写回时还原。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, std::str::Utf8Error> {
        SourceText::decode(bytes).map(Self::from_source)
    }

    pub fn from_source(src: SourceText) -> Self {
        let mut doc = Document {
            text: Rope::from_str(&src.text),
            anchors: MarkerTable::new(),
            blocks: BlockTree::default(),
            version: 0,
            parsed_version: u64::MAX,
            next_id: 1,
            undo: Vec::new(),
            redo: Vec::new(),
            bom: src.bom,
            line_ending: src.line_ending,
            report: ParseReport::default(),
        };
        doc.reparse();
        doc
    }

    // ---- 读 ----

    pub fn text(&self) -> &Rope {
        &self.text
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.len() == 0
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn blocks(&self) -> &BlockTree {
        &self.blocks
    }

    /// 块树是否落后于文本（有编辑尚未解析）。
    pub fn is_parse_stale(&self) -> bool {
        self.parsed_version != self.version
    }

    pub fn slice(&self, r: Range<usize>) -> RopeSlice<'_> {
        self.text.slice(r)
    }

    /// 块的当前字节区间（随编辑漂移后）。
    pub fn range_of(&self, b: &Block) -> Range<usize> {
        self.anchors.resolve_range(&b.range)
    }

    /// 去掉尾部 `^id` / 属性标记后的正文区间。
    pub fn content_range(&self, b: &Block) -> Range<usize> {
        let r = self.range_of(b);
        r.start..r.end.saturating_sub(b.suffix_len as usize).max(r.start)
    }

    pub fn block_text(&self, b: &Block) -> String {
        self.text.slice(self.range_of(b)).to_string()
    }

    /// front matter `title:` 的源码字节区间（不含引号）及解码后的单行文本。
    /// 只认最常见的写法（顶层 `title: xxx`，可带单 / 双引号）；别的形式当作没有 title。
    pub fn front_matter_title(&self) -> Option<(Range<usize>, String)> {
        let fm = self.blocks.roots.first().filter(|b| b.kind == BlockKind::FrontMatter)?;
        let range = self.content_range(fm);
        let src = self.slice(range.clone()).to_string();
        let (r, text) = front_matter_title(&src)?;
        Some((range.start + r.start..range.start + r.end, text))
    }

    /// Actual front matter only; string scalar or list, preserving multiword names.
    /// Invalid YAML yields no aliases; invalid list items are ignored individually.
    pub fn front_matter_aliases(&self) -> Vec<String> {
        #[derive(serde::Deserialize)]
        struct FrontMatter {
            #[serde(default)]
            aliases: AliasValues,
        }
        let Some(fm) = self.blocks.roots.first().filter(|b| b.kind == BlockKind::FrontMatter) else {
            return Vec::new();
        };
        let source = self.block_text(fm);
        let yaml = source.lines().skip(1).take_while(|line| *line != "---").collect::<Vec<_>>().join("\n");
        let Ok(front) = serde_saphyr::from_str::<FrontMatter>(&yaml) else { return Vec::new() };
        let mut seen = std::collections::HashSet::new();
        front
            .aliases
            .0
            .into_iter()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty() && seen.insert(s.to_lowercase()))
            .collect()
    }

    /// 笔记标题：front matter `title`，没有则 `fallback`（通常是文件名）。
    pub fn title_or(&self, fallback: &str) -> String {
        self.front_matter_title().map(|(_, t)| t).unwrap_or_else(|| fallback.to_owned())
    }

    /// 包含 `offset` 的最深块（区间按闭区间判断，光标停在块尾也算在块内）。
    pub fn block_at(&self, offset: usize) -> Option<&Block> {
        let mut level = &self.blocks.roots;
        let mut found = None;
        loop {
            let hit = level.iter().find(|b| {
                let r = self.range_of(b);
                r.start <= offset && offset <= r.end
            });
            match hit {
                Some(b) => {
                    found = Some(b);
                    if b.children.is_empty() {
                        return found;
                    }
                    level = &b.children;
                }
                None => return found,
            }
        }
    }

    /// 写回文件用的字节（还原 BOM 与行尾）。
    pub fn to_bytes(&self) -> Vec<u8> {
        SourceText::encode(&self.to_string(), self.bom, self.line_ending)
    }

    // ---- 写 ----

    /// 应用一次编辑，作为独立的 undo 组。
    pub fn edit(&mut self, edit: Edit) -> Result<(), EditError> {
        self.edit_group(vec![edit])
    }

    /// 顺序应用一组编辑（后一个的坐标基于前一个应用之后的文本），作为一个 undo 组。
    /// 任一编辑非法时整组回滚，文档不变。
    pub fn edit_group(&mut self, edits: Vec<Edit>) -> Result<(), EditError> {
        let mut inverses = Vec::with_capacity(edits.len());
        for e in &edits {
            if let Err(err) = text::check_edit(&self.text, e) {
                for inv in inverses.iter().rev() {
                    self.apply_raw(inv);
                }
                return Err(err);
            }
            inverses.push(self.apply_raw(e));
        }
        if !inverses.is_empty() {
            inverses.reverse();
            self.undo.push(inverses);
            self.redo.clear();
        }
        Ok(())
    }

    /// 应用一次编辑并并入上一个 undo 组（9.5：相邻字符输入合并为一组），没有上一组时新开一组。
    /// 由调用方决定何时合并（例如 300 ms 内的连续输入）。
    pub fn edit_coalescing(&mut self, edit: Edit) -> Result<(), EditError> {
        text::check_edit(&self.text, &edit)?;
        let inverse = self.apply_raw(&edit);
        match self.undo.last_mut() {
            // 组内逆编辑按顺序应用，新编辑的逆必须最先应用
            Some(group) => group.insert(0, inverse),
            None => self.undo.push(vec![inverse]),
        }
        self.redo.clear();
        Ok(())
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// 撤销一组编辑。返回新文本中受影响的区间（编辑器据此放光标）；没有可撤销的内容时为 `None`。
    pub fn undo(&mut self) -> Option<Range<usize>> {
        let group = self.undo.pop()?;
        let (redo_group, affected) = self.apply_group(&group);
        self.redo.push(redo_group);
        Some(affected)
    }

    pub fn redo(&mut self) -> Option<Range<usize>> {
        let group = self.redo.pop()?;
        let (undo_group, affected) = self.apply_group(&group);
        self.undo.push(undo_group);
        Some(affected)
    }

    /// 顺序应用一组编辑，返回逆组与新文本中受影响区间的并集。
    fn apply_group(&mut self, group: &[Edit]) -> (Vec<Edit>, Range<usize>) {
        let mut inverses: Vec<Edit> = Vec::with_capacity(group.len());
        let mut affected: Option<Range<usize>> = None;
        for e in group {
            let new_range = e.new_range();
            // 前面已应用的编辑区间要按本次编辑的位移平移
            if let Some(a) = affected.as_mut() {
                let delta = e.delta();
                if a.start >= e.range.end {
                    a.start = (a.start as isize + delta) as usize;
                }
                if a.end >= e.range.end {
                    a.end = (a.end as isize + delta) as usize;
                }
                a.start = a.start.min(new_range.start);
                a.end = a.end.max(new_range.end);
            } else {
                affected = Some(new_range);
            }
            inverses.push(self.apply_raw(e));
        }
        inverses.reverse();
        (inverses, affected.unwrap_or(0..0))
    }

    /// 改 Rope + 平移 anchor + 版本号。调用方保证编辑合法。
    fn apply_raw(&mut self, edit: &Edit) -> Edit {
        let inverse = text::apply_edit(&mut self.text, edit);
        self.anchors.apply(edit);
        self.version += 1;
        inverse
    }

    // ---- 解析 ----

    pub fn snapshot(&self) -> Snapshot {
        Snapshot { text: self.text.clone(), version: self.version }
    }

    /// 同步重解析。
    pub fn reparse(&mut self) {
        let snap = self.snapshot();
        let out = snap.parse();
        self.apply_parse(snap.version(), out);
    }

    /// 装回一次解析结果。`version` 是解析所用快照的版本；与当前不符则丢弃并返回 `false`。
    pub fn apply_parse(&mut self, version: u64, out: ParseOutput) -> bool {
        if version != self.version {
            return false;
        }
        self.report = out.report;
        self.rebuild(out.blocks);
        self.parsed_version = version;
        true
    }

    /// 用新的解析结果重建 anchor 表与块树，按 7.3 沿用旧 BlockId：
    /// 先按 (kind, hash) 精确匹配（同内容的多个候选取位置最近的），再对剩余块按同类型位置就近匹配。
    /// 旧块的位置取已漂移的 anchor，与新区间在同一坐标系里。
    fn rebuild(&mut self, parsed: Vec<ParsedBlock>) {
        let mut exact: HashMap<(&'static str, u64), Candidates> = HashMap::new();
        for (_, b) in self.blocks.iter() {
            let start = self.anchors.resolve(b.range.start);
            exact.entry((b.kind.tag(), b.hash)).or_default().insert(start, b.id);
        }

        let flat: Vec<(&'static str, u64, usize)> =
            ParsedBlock::iter(&parsed).map(|(_, b)| (b.kind.tag(), b.hash, b.range.start)).collect();
        let mut ids: Vec<Option<BlockId>> = vec![None; flat.len()];

        // 第一遍：精确匹配。先取起点完全重合的，再取最近的——否则内容相同的"孪生块"里，
        // 一个被改动后其余的会在先序贪心里被错位认领，撤销后 id 整体串位。
        for max_dist in [0, usize::MAX] {
            for (i, (tag, hash, start)) in flat.iter().enumerate() {
                if ids[i].is_none() {
                    ids[i] = exact.get_mut(&(*tag, *hash)).and_then(|c| c.take_nearest(*start, max_dist));
                }
            }
        }

        // 第二遍：同类型按位置就近（内容被改过的块）
        let mut by_tag: HashMap<&'static str, Candidates> = HashMap::new();
        for ((tag, _), c) in exact {
            by_tag.entry(tag).or_default().extend(c);
        }
        for (i, (tag, _, start)) in flat.iter().enumerate() {
            if ids[i].is_none() {
                ids[i] = by_tag.get_mut(tag).and_then(|c| c.take_nearest(*start, usize::MAX));
            }
        }

        // 第三遍：不限类型、但起点必须几乎重合（段落 ↔ 标题 ↔ 列表项这类改前缀的类型转换
        // 要保住身份，否则 `/` 菜单切换块类型会丢焦点与折叠状态；远处消失的块不能把 id 传给无关的新块）
        let mut any = Candidates::default();
        for (_, c) in by_tag {
            any.extend(c);
        }
        for (i, (_, _, start)) in flat.iter().enumerate() {
            if ids[i].is_none() {
                ids[i] = any.take_nearest(*start, 2);
            }
        }

        let mut anchors = MarkerTable::successor(&self.anchors);
        let mut ids = ids.into_iter();
        let roots = self.build_blocks(parsed, &mut anchors, &mut ids);
        self.anchors = anchors;
        self.blocks = BlockTree { roots };
    }

    fn build_blocks(
        &mut self,
        parsed: Vec<ParsedBlock>,
        anchors: &mut MarkerTable,
        ids: &mut impl Iterator<Item = Option<BlockId>>,
    ) -> Vec<Block> {
        parsed
            .into_iter()
            .map(|p| {
                let id = ids.next().flatten().unwrap_or_else(|| {
                    let id = BlockId(self.next_id);
                    self.next_id += 1;
                    id
                });
                let range = anchors.anchor_range(p.range);
                let children = self.build_blocks(p.children, anchors, ids);
                Block {
                    id,
                    kind: p.kind,
                    range,
                    suffix_len: p.suffix_len,
                    persisted_id: p.persisted_id,
                    attrs: p.attrs,
                    hash: p.hash,
                    children,
                }
            })
            .collect()
    }
}

/// Lossless cache encoding shared by metadata and full-text storage.
/// Only serializes string lists; never used to rewrite Markdown front matter.
pub fn encode_aliases(aliases: &[String]) -> String {
    serde_saphyr::to_string(&aliases).expect("string lists are YAML serializable")
}

pub fn decode_aliases(yaml: &str) -> Result<Vec<String>, serde_saphyr::Error> {
    serde_saphyr::from_str(yaml)
}

#[derive(Default)]
struct AliasValue<const LIST: bool>(Vec<String>);
type AliasValues = AliasValue<true>;

impl<'de, const LIST: bool> serde::Deserialize<'de> for AliasValue<LIST> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<const LIST: bool>;
        impl<'de, const LIST: bool> serde::de::Visitor<'de> for V<LIST> {
            type Value = AliasValue<LIST>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an alias string or list of alias strings")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(AliasValue(vec![v.to_owned()]))
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(AliasValue::default())
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(AliasValue::default())
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(AliasValue::default())
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(AliasValue::default())
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(AliasValue::default())
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?.is_some() {}
                Ok(AliasValue::default())
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                if LIST {
                    while let Some(AliasValue(v)) = seq.next_element::<AliasValue<false>>()? {
                        out.extend(v);
                    }
                } else {
                    while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                }
                Ok(AliasValue(out))
            }
        }
        d.deserialize_any(V::<LIST>)
    }
}

/// 按起点排好序的候选旧块，支持取出离某位置最近的一个。
#[derive(Default)]
struct Candidates {
    by_start: BTreeMap<usize, Vec<BlockId>>,
    len: usize,
}

impl Candidates {
    fn insert(&mut self, start: usize, id: BlockId) {
        self.by_start.entry(start).or_default().push(id);
        self.len += 1;
    }

    fn extend(&mut self, other: Candidates) {
        for (start, ids) in other.by_start {
            for id in ids {
                self.insert(start, id);
            }
        }
    }

    /// 取出起点离 `start` 最近、且距离不超过 `max_dist` 的候选。
    fn take_nearest(&mut self, start: usize, max_dist: usize) -> Option<BlockId> {
        if self.len == 0 {
            return None;
        }
        let below = self.by_start.range(..=start).next_back().map(|(k, _)| *k);
        let above = self.by_start.range(start..).next().map(|(k, _)| *k);
        let key = match (below, above) {
            (Some(b), Some(a)) => {
                if start - b <= a - start {
                    b
                } else {
                    a
                }
            }
            (Some(b), None) => b,
            (None, Some(a)) => a,
            (None, None) => return None,
        };
        if key.abs_diff(start) > max_dist {
            return None;
        }
        let ids = self.by_start.get_mut(&key)?;
        let id = ids.pop();
        if ids.is_empty() {
            self.by_start.remove(&key);
        }
        self.len -= 1;
        id
    }
}

impl fmt::Display for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.text.chunks() {
            f.write_str(chunk)?;
        }
        Ok(())
    }
}

/// front matter 源码里 `title:` 一行的值：值在 `src` 内的字节区间（不含引号）与去引号后的文本。
pub fn front_matter_title(src: &str) -> Option<(Range<usize>, String)> {
    let mut at = 0;
    for line in src.split_inclusive('\n') {
        let start = at;
        at += line.len();
        let body = line.trim_end_matches(['\n', '\r']);
        let Some(rest) = body.strip_prefix("title:") else { continue };
        let value_start = start + "title:".len() + (rest.len() - rest.trim_start().len());
        let value = rest.trim();
        if value.is_empty() || value.starts_with('#') {
            return Some((value_start..value_start, String::new()));
        }
        let quoted = matches!(value.as_bytes()[0], b'"' | b'\'');
        let end = if quoted {
            let quote = value.as_bytes()[0];
            let mut i = 1;
            loop {
                let ch = *value.as_bytes().get(i)?;
                if ch == b'\\' && quote == b'"' {
                    i += 2;
                } else if ch == quote {
                    if quote == b'\'' && value.as_bytes().get(i + 1) == Some(&quote) {
                        i += 2;
                    } else {
                        break i + 1;
                    }
                } else {
                    i += 1;
                }
            }
        } else {
            value
                .char_indices()
                .find(|(i, c)| *c == '#' && value[..*i].ends_with(char::is_whitespace))
                .map_or(value.len(), |(i, _)| value[..i].trim_end().len())
        };
        let trailing = value[end..].trim();
        if !trailing.is_empty() && !trailing.starts_with('#') {
            return None;
        }
        let text = serde_saphyr::from_str::<String>(&value[..end]).ok()?;
        let range =
            if quoted { value_start + 1..value_start + end - 1 } else { value_start..value_start + end };
        return Some((range, text));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_parse_only_actual_front_matter_without_rewriting() {
        for (yaml, expected) in [
            ("aliases: '  Multi  word  '", vec!["Multi  word"]),
            (
                "aliases: [ÉCOLE, école, '雪 山', '', ' ', 42, true, null, [nested], {x: y}, Last]",
                vec!["ÉCOLE", "雪 山", "Last"],
            ),
            (
                "aliases:\n  - First\n  - 2.5\n  - false\n  - [nested]\n  - {x: y}\n  - ' First '\n  - 'Second name'",
                vec!["First", "Second name"],
            ),
            ("aliases: [broken", vec![]),
            ("aliases: {nested: name}", vec![]),
            ("aliases: 123", vec![]),
            ("aliases: null", vec![]),
        ] {
            let source =
                format!("\u{feff}---\r\n{}\r\n---\r\n\r\naliases: [body]\r\n", yaml.replace('\n', "\r\n"));
            let doc = Document::from_bytes(source.as_bytes()).unwrap();
            assert_eq!(doc.front_matter_aliases(), expected, "{yaml}");
            assert_eq!(doc.to_bytes(), source.as_bytes());
        }
        for source in
            ["aliases: [body]\n", "body\n\n---\naliases: [body]\n---\n", "---\naliases: [unclosed]\n"]
        {
            assert!(Document::from_text(source).front_matter_aliases().is_empty(), "{source}");
        }
    }

    #[test]
    fn front_matter_title_decodes_yaml_and_excludes_trailing_comments() {
        for (src, raw, expected) in [
            ("---\ntitle: 'Sam''s note' # keep\n---", "Sam''s note", "Sam's note"),
            ("---\ntitle: \"a\\\"b\\\\c\" # keep\n---", "a\\\"b\\\\c", "a\"b\\c"),
            ("---\ntitle: New title # keep\n---", "New title", "New title"),
        ] {
            let (range, title) = front_matter_title(src).unwrap();
            assert_eq!(&src[range], raw);
            assert_eq!(title, expected);
        }
    }

    #[test]
    fn front_matter_title_value_range() {
        let src = "---\ntags: [a]\ntitle: 我的标题\n---";
        let (r, t) = front_matter_title(src).unwrap();
        assert_eq!(t, "我的标题");
        assert_eq!(&src[r], "我的标题");
        let src = "---\ntitle:   \"quoted: yes\"  \n---";
        let (r, t) = front_matter_title(src).unwrap();
        assert_eq!(t, "quoted: yes");
        assert_eq!(&src[r], "quoted: yes");
        assert!(front_matter_title("---\nsubtitle: x\n---").is_none());
        let (r, t) = front_matter_title("---\ntitle:\n---").unwrap();
        assert_eq!(t, "");
        assert!(r.is_empty());

        let doc = Document::from_text("---\ntitle: T\n---\n\nbody\n");
        let (r, t) = doc.front_matter_title().unwrap();
        assert_eq!(t, "T");
        assert_eq!(doc.slice(r).to_string(), "T");
        assert_eq!(doc.title_or("file"), "T");
        assert_eq!(Document::from_text("plain\n").title_or("file"), "file");
    }

    const DOC: &str = "# 标题\n\n第一段。\n\n- a\n- b\n\n第三段。\n";

    #[test]
    fn parse_on_construct_and_ranges_follow_edits() {
        let mut d = Document::from_text(DOC);
        assert_eq!(d.blocks.roots.len(), 4);
        let para = d.blocks.roots[1].clone();
        assert_eq!(d.block_text(&para), "第一段。");

        // 在标题里插字：段落区间整体后移，块树未重解析但区间已正确
        d.edit(Edit::insert("# 标题".len(), "！")).unwrap();
        assert!(d.is_parse_stale());
        assert_eq!(d.block_text(&para), "第一段。");
        assert_eq!(d.to_string(), "# 标题！\n\n第一段。\n\n- a\n- b\n\n第三段。\n");
    }

    #[test]
    fn block_ids_survive_reparse() {
        let mut d = Document::from_text(DOC);
        let before: Vec<(BlockId, BlockKind)> =
            d.blocks.iter().map(|(_, b)| (b.id, b.kind.clone())).collect();
        // 改第一段的内容
        let r = d.range_of(&d.blocks.roots[1]);
        d.edit(Edit::insert(r.end, "更多。")).unwrap();
        d.reparse();
        let after: Vec<(BlockId, BlockKind)> = d.blocks.iter().map(|(_, b)| (b.id, b.kind.clone())).collect();
        assert_eq!(before.len(), after.len());
        // 未改动的块 id 不变（精确匹配）；改动的段落靠位置匹配也沿用了 id
        assert_eq!(before, after);
    }

    #[test]
    fn new_block_gets_new_id_and_others_keep_theirs() {
        let mut d = Document::from_text(DOC);
        let ids_before: Vec<BlockId> = d.blocks.roots.iter().map(|b| b.id).collect();
        d.edit(Edit::insert(d.len(), "\n新段落。\n")).unwrap();
        d.reparse();
        let ids_after: Vec<BlockId> = d.blocks.roots.iter().map(|b| b.id).collect();
        assert_eq!(ids_after.len(), 5);
        assert_eq!(&ids_after[..4], &ids_before[..]);
        assert!(!ids_before.contains(&ids_after[4]));
    }

    #[test]
    fn undo_redo_roundtrip() {
        let mut d = Document::from_text("abc");
        d.edit(Edit::insert(3, "d")).unwrap();
        d.edit_group(vec![Edit::delete(0..1), Edit::insert(0, "X")]).unwrap();
        assert_eq!(d.to_string(), "Xbcd");
        assert!(d.undo().is_some());
        assert_eq!(d.to_string(), "abcd");
        assert!(d.undo().is_some());
        assert_eq!(d.to_string(), "abc");
        assert!(d.undo().is_none());
        assert!(d.redo().is_some());
        assert!(d.redo().is_some());
        assert_eq!(d.to_string(), "Xbcd");
        assert!(d.redo().is_none());
    }

    #[test]
    fn undo_redo_report_affected_range() {
        let mut d = Document::from_text("abc");
        d.edit(Edit::insert(3, "def")).unwrap();
        // 撤销插入 → 删除，受影响区间收缩为插入点
        assert_eq!(d.undo(), Some(3..3));
        // 重做 → 插入的新文本区间
        assert_eq!(d.redo(), Some(3..6));
        // 合并组：三个字符依次输入，撤销后回到起点
        d.edit(Edit::insert(0, "1")).unwrap();
        d.edit_coalescing(Edit::insert(1, "2")).unwrap();
        d.edit_coalescing(Edit::insert(2, "3")).unwrap();
        assert_eq!(d.to_string(), "123abcdef");
        assert_eq!(d.undo(), Some(0..0));
        assert_eq!(d.redo(), Some(0..3));
        // 替换：撤销后区间是原文本的位置
        d.edit(Edit::replace(0..3, "X")).unwrap();
        assert_eq!(d.to_string(), "Xabcdef");
        assert_eq!(d.undo(), Some(0..3));
    }

    #[test]
    fn coalesced_edits_undo_together() {
        let mut d = Document::from_text("");
        d.edit(Edit::insert(0, "a")).unwrap();
        d.edit_coalescing(Edit::insert(1, "b")).unwrap();
        d.edit_coalescing(Edit::insert(2, "c")).unwrap();
        d.edit(Edit::insert(3, "!")).unwrap();
        assert_eq!(d.to_string(), "abc!");
        assert!(d.undo().is_some());
        assert_eq!(d.to_string(), "abc");
        assert!(d.undo().is_some());
        assert_eq!(d.to_string(), "");
        assert!(d.redo().is_some());
        assert_eq!(d.to_string(), "abc");
        // 没有上一组时自成一组
        let mut e = Document::from_text("x");
        e.edit_coalescing(Edit::insert(1, "y")).unwrap();
        assert!(e.undo().is_some());
        assert_eq!(e.to_string(), "x");
    }

    #[test]
    fn invalid_group_is_rolled_back() {
        let mut d = Document::from_text("abc");
        let err = d.edit_group(vec![Edit::insert(0, "x"), Edit::delete(0..99)]).unwrap_err();
        assert!(matches!(err, EditError::OutOfBounds { .. }));
        assert_eq!(d.to_string(), "abc");
        assert!(!d.can_undo());
    }

    #[test]
    fn stale_parse_is_rejected() {
        let mut d = Document::from_text(DOC);
        let snap = d.snapshot();
        d.edit(Edit::insert(0, "x")).unwrap();
        let out = snap.parse();
        assert!(!d.apply_parse(snap.version(), out));
        assert!(d.is_parse_stale());
        d.reparse();
        assert!(!d.is_parse_stale());
    }

    #[test]
    fn block_at_finds_deepest() {
        let d = Document::from_text(DOC);
        let off = DOC.find("- b").unwrap() + 2;
        let b = d.block_at(off).unwrap();
        assert_eq!(b.kind, BlockKind::Paragraph);
        assert_eq!(d.block_text(b), "b");
        // 块尾（闭区间）
        let heading = &d.blocks.roots[0];
        assert_eq!(d.block_at(d.range_of(heading).end).unwrap().id, heading.id);
    }

    #[test]
    fn bytes_roundtrip_preserves_crlf_and_bom() {
        let bytes = b"\xEF\xBB\xBF# a\r\n\r\nb\r\n";
        let d = Document::from_bytes(bytes).unwrap();
        assert_eq!(d.to_bytes(), bytes);
        assert_eq!(d.blocks.roots.len(), 2);
    }
}
