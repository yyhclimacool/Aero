//! 解析：整篇文本 → 带字节区间的块树（方案 8.2 的管线，ADR 0002）。
//!
//! ```text
//! 原文
//!  → comrak（table / tasklist / footnotes / math / wikilinks / highlight /
//!            block_directive / attributes / cjk_friendly_emphasis / front matter）
//!  → 转换：sourcepos → 字节区间，越界 / 非 char 边界的区间夹回父区间
//!  → 后处理：兄弟块按区间排序（FootnoteDefinition 归位）
//!           → 仅含 display Math 的段落 → MathBlock
//!           → 段尾 / 独占行的 ^id 与 {属性列表}；独占行成为 Marker 并挂到前一个兄弟
//!           → 引用块首行 [!type] → Callout
//!           → 顶层块之间有非空白内容的间隙 → Raw（链接引用定义等 comrak 不产节点的内容）
//!  → Vec<ParsedBlock>
//! ```
//!
//! 本模块只产出字节区间，不接触 Rope / Anchor；`Document` 负责把区间变成 anchor。

pub(crate) mod markers;

pub use markers::callout_header;

use std::ops::Range;
use std::sync::Arc;

use comrak::nodes::{AstNode, ListType, NodeValue, Sourcepos};
use comrak::{Arena, Options, parse_document};

use crate::block::{Attrs, BlockKind};

/// 解析结果中的一个块，区间为字节偏移。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedBlock {
    pub kind: BlockKind,
    pub range: Range<usize>,
    pub suffix_len: u32,
    pub persisted_id: Option<Arc<str>>,
    pub attrs: Attrs,
    pub hash: u64,
    pub children: Vec<ParsedBlock>,
}

impl ParsedBlock {
    /// 先序遍历。
    pub fn iter(blocks: &[ParsedBlock]) -> impl Iterator<Item = (usize, &ParsedBlock)> {
        let mut stack: Vec<(usize, &ParsedBlock)> = blocks.iter().rev().map(|b| (0, b)).collect();
        std::iter::from_fn(move || {
            let (depth, b) = stack.pop()?;
            stack.extend(b.children.iter().rev().map(|c| (depth + 1, c)));
            Some((depth, b))
        })
    }
}

/// 后处理层对 comrak 输出做的修正统计，测试用来断言语料上的异常为零。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParseReport {
    /// 区间越出父区间或不在 char 边界上、被夹回的节点数
    pub clamped: usize,
    /// 被夹回的节点：(节点描述, comrak 给的区间, 修正后的区间)，供诊断
    pub clamped_nodes: Vec<(String, Range<usize>, Range<usize>)>,
    /// 没有源位置、被丢弃的节点数
    pub dropped: usize,
    /// 顶层间隙里补出的 Raw 块数
    pub raw_gaps: usize,
}

#[derive(Clone, Debug, Default)]
pub struct ParseOutput {
    pub blocks: Vec<ParsedBlock>,
    pub report: ParseReport,
}

/// comrak 选项：与 ADR 0002 一致。`alerts` 关闭——callout 由 [`markers::callout_header`]
/// 统一识别，这样 Aero 的 8 种类型与 GitHub 的 5 种走同一条路。
pub fn comrak_options() -> Options<'static> {
    let mut o = Options::default();
    let e = &mut o.extension;
    e.strikethrough = true;
    e.table = true;
    e.autolink = true;
    e.tasklist = true;
    e.footnotes = true;
    e.front_matter_delimiter = Some("---".to_owned());
    e.math_dollars = true;
    e.wikilinks_title_after_pipe = true;
    e.highlight = true;
    e.block_directive = true;
    e.cjk_friendly_emphasis = true;
    e.header_attributes = true;
    e.inline_code_attributes = true;
    e.link_attributes = true;
    e.fenced_code_attributes = true;
    o.render.sourcepos = true;
    o
}

/// 解析整篇文本。
pub fn parse(text: &str) -> ParseOutput {
    let opts = comrak_options();
    let arena = Arena::new();
    let root = parse_document(&arena, text, &opts);
    let mut cx = Cx { text, line_starts: line_starts(text), report: ParseReport::default() };
    let mut roots = cx.convert_children(root, 0..text.len());
    cx.fill_gaps(&mut roots);
    ParseOutput { blocks: roots, report: cx.report }
}

/// 一次 comrak 解析同时得到：可能含 `#tag` 的文本片段（标题、图片 alt、链接 URL、wikilink 目标、
/// 代码、`{...}` 属性都排除）与全部 wikilink 区间。索引每篇笔记时两者都要，合在一起省一次解析。
pub(crate) fn inline_scan(text: &str) -> (Vec<&str>, Vec<crate::links::LinkSpan>) {
    let arena = Arena::new();
    let root = parse_document(&arena, text, &comrak_options());
    let links = crate::links::scan_tree(root, text);
    let cx = Cx { text, line_starts: line_starts(text), report: ParseReport::default() };
    // 属性值中的 Markdown 会把一段属性拆成多个 Text 节点，须按原文区间排除。
    let attributes: Vec<Range<usize>> = text
        .match_indices('{')
        .filter_map(|(start, _)| {
            if text[..start].chars().rev().take_while(|&c| c == '\\').count() % 2 == 1 {
                return None;
            }
            let end = start + text[start..].find('}')? + 1;
            Attrs::parse_inner(&text[start + 1..end - 1]).map(|_| start..end)
        })
        .collect();
    let mut sources = Vec::new();
    for node in root.descendants().filter(|node| matches!(node.data().value, NodeValue::Text(_))) {
        if node.ancestors().any(|parent| match &parent.data().value {
            // 标题不算标签；图片 alt、`[[#heading]]` / `[[note#^id]]` 的链接目标里的 `#` 是锚点不是标签
            NodeValue::Heading(_) | NodeValue::Image(_) | NodeValue::WikiLink(_) => true,
            NodeValue::Link(link) => {
                matches!(&node.data().value, NodeValue::Text(value) if value.as_ref() == link.url
                    || (value.starts_with("www.") && link.url.strip_prefix("http://") == Some(value.as_ref())))
            }
            _ => false,
        }) {
            continue;
        }
        let Some(range) = cx.sourcepos_range(node.data().sourcepos) else { continue };
        let mut start = range.start;
        for attr in attributes.iter().filter(|attr| attr.end > range.start && attr.start < range.end) {
            if start < attr.start
                && let Some(source) = text.get(start..attr.start)
            {
                sources.push(source);
            }
            start = start.max(attr.end).min(range.end);
        }
        if start < range.end
            && let Some(source) = text.get(start..range.end)
        {
            sources.push(source);
        }
    }
    (sources, links)
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut v = vec![0];
    v.extend(text.bytes().enumerate().filter(|&(_, b)| b == b'\n').map(|(i, _)| i + 1));
    v
}

/// 内容指纹（xxh3-64），与 [`crate::FileStamp::hash`] 同一个函数。
/// 跨进程、跨版本稳定，可以用作磁盘缓存键。
pub fn hash_bytes(b: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(b)
}

struct Cx<'t> {
    text: &'t str,
    line_starts: Vec<usize>,
    report: ParseReport,
}

impl<'t> Cx<'t> {
    fn sourcepos_range(&self, sp: Sourcepos) -> Option<Range<usize>> {
        if sp.start.line == 0 || sp.end.line == 0 {
            return None;
        }
        let last = self.line_starts.len() - 1;
        let sl = (sp.start.line - 1).min(last);
        let el = (sp.end.line - 1).min(last);
        let s = self.line_starts[sl] + sp.start.column.saturating_sub(1);
        let len = self.text.len();
        let s = s.min(len);
        // end.column 为闭区间。列表最后一项 comrak 会给 `N:0`，N 是列表后被吞掉的最后一个空行，
        // 意思是"到前面最后一个非空行的行尾"：回退到 N 行之前，再去掉尾部空白。
        let e = if sp.end.column == 0 && el > 0 {
            let mut e = (self.line_starts[el] - 1).min(len);
            let bytes = self.text.as_bytes();
            while e > s && bytes[e - 1].is_ascii_whitespace() {
                e -= 1;
            }
            e
        } else {
            (self.line_starts[el] + sp.end.column).min(len)
        };
        Some(s..e.max(s))
    }

    fn floor_boundary(&self, mut i: usize) -> usize {
        while i > 0 && !self.text.is_char_boundary(i) {
            i -= 1;
        }
        i
    }

    fn ceil_boundary(&self, mut i: usize) -> usize {
        while i < self.text.len() && !self.text.is_char_boundary(i) {
            i += 1;
        }
        i
    }

    /// 夹回父区间并落到 char 边界；有改动则计入报告。
    fn sanitize(&mut self, r: Range<usize>, parent: &Range<usize>, what: &NodeValue) -> Range<usize> {
        let s = self.ceil_boundary(r.start.clamp(parent.start, parent.end));
        let e = self.floor_boundary(r.end.clamp(parent.start, parent.end)).max(s);
        if s != r.start || e != r.end {
            self.report.clamped += 1;
            let desc: String = format!("{what:?}").chars().take(40).collect();
            self.report.clamped_nodes.push((desc, r, s..e));
        }
        s..e
    }

    fn hash(&self, r: &Range<usize>) -> u64 {
        hash_bytes(self.text[r.clone()].as_bytes())
    }

    fn leaf(&self, kind: BlockKind, range: Range<usize>, attrs: Attrs) -> ParsedBlock {
        ParsedBlock {
            hash: self.hash(&range),
            kind,
            range,
            suffix_len: 0,
            persisted_id: None,
            attrs,
            children: Vec::new(),
        }
    }

    fn convert_children<'a>(&mut self, node: &'a AstNode<'a>, parent: Range<usize>) -> Vec<ParsedBlock> {
        let mut out: Vec<ParsedBlock> = Vec::new();
        for child in node.children() {
            // comrak 把 FrontMatter 归为非块节点，这里要单独放行
            let is_block = {
                let v = &child.data().value;
                v.block() || matches!(v, NodeValue::FrontMatter(_))
            };
            if !is_block {
                continue;
            }
            if let Some(b) = self.convert_node(child, &parent) {
                out.push(b);
            }
        }
        // FootnoteDefinition 会被 comrak 移到末尾，按源位置归位
        out.sort_by_key(|b| b.range.start);
        // 兄弟区间不得重叠。comrak 偶尔把容器的终点算过头（如列表项里的 `:::` 与后面的 directive），
        // 块的起点远比终点可靠，所以把前一个兄弟截断到当前块的起点。
        for i in 1..out.len() {
            let limit = out[i].range.start;
            if out[i - 1].range.end > limit {
                let old = out[i - 1].range.clone();
                self.truncate(&mut out[i - 1], limit);
                self.report.clamped += 1;
                let desc = format!("{:?} (overlaps next sibling)", out[i - 1].kind);
                self.report.clamped_nodes.push((desc, old, out[i - 1].range.clone()));
            }
        }
        attach_markers(&mut out);
        out
    }

    /// 把块及其后代的终点截到 `limit`，并重算哈希。
    fn truncate(&self, block: &mut ParsedBlock, limit: usize) {
        if block.range.end <= limit {
            return;
        }
        block.range.end = limit.max(block.range.start);
        block.suffix_len = block.suffix_len.min(block.range.len() as u32);
        block.hash = self.hash(&block.range);
        block.children.retain(|c| c.range.start < limit);
        for c in &mut block.children {
            self.truncate(c, limit);
        }
    }

    fn convert_node<'a>(&mut self, node: &'a AstNode<'a>, parent: &Range<usize>) -> Option<ParsedBlock> {
        let ast = node.data();
        let Some(mut raw_range) = self.sourcepos_range(ast.sourcepos) else {
            self.report.dropped += 1;
            return None;
        };
        // Directive closure can leave parent end positions before their actual children.
        for child in node.descendants().skip(1) {
            if let Some(range) = self.sourcepos_range(child.data().sourcepos) {
                raw_range.end = raw_range.end.max(range.end);
            }
        }
        let range = self.sanitize(raw_range, parent, &ast.value);
        let attrs = ast
            .attrs
            .as_ref()
            .map(|a| Attrs { id: a.id.clone(), classes: a.classes.clone(), pairs: a.pairs.clone() })
            .unwrap_or_default();

        let block = match &ast.value {
            NodeValue::Document => unreachable!("Document 不会作为子节点出现"),
            NodeValue::FrontMatter(_) => self.leaf(BlockKind::FrontMatter, range, attrs),
            NodeValue::Paragraph => {
                return Some(self.paragraph(node, range));
            }
            NodeValue::Heading(h) => {
                let mut b = self.leaf(BlockKind::Heading { level: h.level }, range.clone(), attrs);
                // comrak 把 header attributes 收进 ast.attrs，但 sourcepos 仍含 `{...}`。
                // 与段落的块末属性一样记成 suffix：编辑和渲染都不再出现这一段源码。
                let src = &self.text[range];
                let t = markers::strip_trailing(src);
                if t.attrs.is_some() || t.id.is_some() {
                    if let Some(attrs) = t.attrs {
                        b.attrs = attrs;
                    }
                    b.persisted_id = t.id.map(Arc::from);
                    b.suffix_len = (src.len() - t.content_len) as u32;
                }
                b
            }
            NodeValue::CodeBlock(cb) => {
                let kind = if cb.fenced {
                    BlockKind::CodeFence { info: strip_info_attrs(&cb.info).to_owned() }
                } else {
                    BlockKind::CodeBlock
                };
                self.leaf(kind, range, attrs)
            }
            NodeValue::HtmlBlock(_) => self.leaf(BlockKind::HtmlBlock, range, attrs),
            NodeValue::ThematicBreak => self.leaf(BlockKind::ThematicBreak, range, attrs),
            NodeValue::Table(_) => self.leaf(BlockKind::Table, range, attrs),
            NodeValue::TableRow(_) | NodeValue::TableCell => return None,
            NodeValue::List(l) => {
                let kind =
                    BlockKind::List { ordered: l.list_type == ListType::Ordered, start: l.start as u32 };
                self.container(node, kind, range, attrs)
            }
            NodeValue::Item(_) => self.container(node, BlockKind::ListItem { task: None }, range, attrs),
            NodeValue::TaskItem(t) => {
                let checked = t.symbol.is_some();
                self.container(node, BlockKind::ListItem { task: Some(checked) }, range, attrs)
            }
            NodeValue::BlockQuote | NodeValue::MultilineBlockQuote(_) | NodeValue::Alert(_) => {
                return Some(self.block_quote(node, range, attrs));
            }
            NodeValue::FootnoteDefinition(f) => {
                let kind = BlockKind::FootnoteDefinition { name: f.name.clone() };
                self.container(node, kind, range, attrs)
            }
            NodeValue::BlockDirective(d) => {
                let (name, inline_attrs) = split_directive_info(&d.info);
                let mut attrs = attrs;
                if let Some(a) = inline_attrs {
                    attrs.merge(a);
                }
                self.container(node, BlockKind::Directive { name }, range, attrs)
            }
            // DescriptionList 系、HeexBlock 等未启用或不支持的块：按字节透传
            _ => self.leaf(BlockKind::Raw, range, attrs),
        };
        Some(block)
    }

    fn container<'a>(
        &mut self,
        node: &'a AstNode<'a>,
        kind: BlockKind,
        range: Range<usize>,
        attrs: Attrs,
    ) -> ParsedBlock {
        let mut children = self.convert_children(node, range.clone());
        let mut attrs = attrs;
        let mut persisted_id = None;
        if matches!(kind, BlockKind::ListItem { .. })
            && children.first().is_some_and(|b| {
                b.kind == BlockKind::Marker
                    && b.attrs.pairs.iter().any(|(k, v)| k == "aero-owner" && v == "item")
            })
        {
            let metadata = children.remove(0);
            persisted_id = metadata.persisted_id;
            attrs.merge(metadata.attrs);
        }
        ParsedBlock { hash: self.hash(&range), kind, range, suffix_len: 0, persisted_id, attrs, children }
    }

    fn paragraph<'a>(&mut self, node: &'a AstNode<'a>, range: Range<usize>) -> ParsedBlock {
        // 仅含一个 display math 的段落提升为数学块
        let mut inline = node.children();
        if let (Some(first), None) = (inline.next(), inline.next())
            && matches!(&first.data().value, NodeValue::Math(m) if m.display_math)
        {
            return self.leaf(BlockKind::MathBlock, range, Attrs::default());
        }

        let src = &self.text[range.clone()];
        if let Some((id, attrs)) = markers::parse_marker_line(src) {
            let mut b = self.leaf(BlockKind::Marker, range, attrs.unwrap_or_default());
            b.persisted_id = id.map(Arc::from);
            return b;
        }

        let t = markers::strip_trailing(src);
        let mut b = self.leaf(BlockKind::Paragraph, range, t.attrs.unwrap_or_default());
        b.suffix_len = (src.len() - t.content_len) as u32;
        b.persisted_id = t.id.map(Arc::from);
        b
    }

    fn block_quote<'a>(&mut self, node: &'a AstNode<'a>, range: Range<usize>, attrs: Attrs) -> ParsedBlock {
        let src = &self.text[range.clone()];
        let Some((kind, fold, title, header_end)) = markers::callout_header(src) else {
            return self.container(node, BlockKind::BlockQuote, range, attrs);
        };
        let header_end_abs = range.start + header_end;
        let mut block = self.container(node, BlockKind::Callout { kind, title, fold }, range.clone(), attrs);

        // 首个段落含有 `[!type]` 行：单行则整段去掉，多行则把段首推到下一行正文
        if let Some(first) = block.children.first_mut()
            && first.kind == BlockKind::Paragraph
            && first.range.start < header_end_abs
        {
            if first.range.end <= header_end_abs {
                block.children.remove(0);
            } else {
                let mut pos = (header_end_abs + 1).min(first.range.end);
                let bytes = self.text.as_bytes();
                while pos < first.range.end && matches!(bytes[pos], b' ' | b'>' | b'\t') {
                    pos += 1;
                }
                first.range.start = pos;
                first.hash = self.hash(&first.range);
            }
        }
        block
    }

    /// 顶层块之间含非空白内容的间隙补成 Raw（原则 2：未知语法原样保留）。
    fn fill_gaps(&mut self, roots: &mut Vec<ParsedBlock>) {
        let mut out = Vec::with_capacity(roots.len());
        let mut cursor = 0;
        for b in roots.drain(..) {
            if let Some(raw) = self.gap_block(cursor, b.range.start) {
                out.push(raw);
            }
            cursor = cursor.max(b.range.end);
            out.push(b);
        }
        if let Some(raw) = self.gap_block(cursor, self.text.len()) {
            out.push(raw);
        }
        *roots = out;
    }

    fn gap_block(&mut self, start: usize, end: usize) -> Option<ParsedBlock> {
        if start >= end {
            return None;
        }
        let gap = &self.text[start..end];
        let first = gap.find(|c: char| !c.is_whitespace())?;
        let last = gap.rfind(|c: char| !c.is_whitespace())?;
        // 扩到整行
        let s = start + gap[..first].rfind('\n').map_or(0, |i| i + 1);
        let e = start + gap[last..].find('\n').map_or(gap.len(), |i| last + i);
        self.report.raw_gaps += 1;
        Some(self.leaf(BlockKind::Raw, s..e, Attrs::default()))
    }
}

/// 独占行的 Marker 把 `^id` / 属性挂到前一个兄弟块上。
fn attach_markers(siblings: &mut [ParsedBlock]) {
    for i in 1..siblings.len() {
        if siblings[i].kind != BlockKind::Marker || siblings[i - 1].kind == BlockKind::Marker {
            continue;
        }
        let id = siblings[i].persisted_id.clone();
        let attrs = std::mem::take(&mut siblings[i].attrs);
        let prev = &mut siblings[i - 1];
        if id.is_some() {
            prev.persisted_id = id;
        }
        prev.attrs.merge(attrs);
    }
}

/// 围栏 info string 里的 `{...}` 属性由 comrak 解析，这里只把它从 info 文本里去掉。
fn strip_info_attrs(info: &str) -> &str {
    match (info.find('{'), info.ends_with('}')) {
        (Some(i), true) => info[..i].trim_end(),
        _ => info.trim(),
    }
}

/// `:::name {key=value}` → (`name`, 属性)
fn split_directive_info(info: &str) -> (String, Option<Attrs>) {
    let info = info.trim();
    let (name, rest) = info.split_once(char::is_whitespace).unwrap_or((info, ""));
    let attrs = rest.trim().strip_prefix('{').and_then(|r| r.strip_suffix('}')).and_then(Attrs::parse_inner);
    (name.to_owned(), attrs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{CalloutKind, Fold};

    fn kinds(text: &str) -> Vec<BlockKind> {
        parse(text).blocks.into_iter().map(|b| b.kind).collect()
    }

    #[test]
    fn basic_blocks() {
        let text = "---\ntitle: x\n---\n\n# H1 {.red}\n\npara\n\n- a\n- [x] b\n\n```rust\nfn f() {}\n```\n\n---\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let out = parse(text);
        assert_eq!(out.report, ParseReport::default(), "{:?}", out.report);
        let b = &out.blocks;
        assert_eq!(b[0].kind, BlockKind::FrontMatter);
        assert_eq!(b[1].kind, BlockKind::Heading { level: 1 });
        assert_eq!(b[1].attrs.classes, ["red"]);
        // 标题的块末属性与段落一样算尾部标记，正文里看不到它
        assert_eq!(&text[b[1].range.start..b[1].range.end - b[1].suffix_len as usize], "# H1");
        assert_eq!(b[2].kind, BlockKind::Paragraph);
        assert_eq!(b[3].kind, BlockKind::List { ordered: false, start: 1 });
        assert_eq!(b[3].children[0].kind, BlockKind::ListItem { task: None });
        assert_eq!(b[3].children[1].kind, BlockKind::ListItem { task: Some(true) });
        assert_eq!(b[4].kind, BlockKind::CodeFence { info: "rust".into() });
        assert_eq!(b[5].kind, BlockKind::ThematicBreak);
        assert_eq!(b[6].kind, BlockKind::Table);
        assert_eq!(&text[b[6].range.clone()], "| a | b |\n|---|---|\n| 1 | 2 |");
    }

    #[test]
    fn math_block_and_trailing_markers() {
        let text = "$$\nx^2\n$$\n\n段落。 {.bg-blue} ^p1\n";
        let out = parse(text);
        assert_eq!(out.blocks[0].kind, BlockKind::MathBlock);
        let p = &out.blocks[1];
        assert_eq!(p.kind, BlockKind::Paragraph);
        assert_eq!(p.persisted_id.as_deref(), Some("p1"));
        assert_eq!(p.attrs.classes, ["bg-blue"]);
        let content = &text[p.range.start..p.range.end - p.suffix_len as usize];
        assert_eq!(content, "段落。");
    }

    #[test]
    fn standalone_marker_attaches_to_previous_block() {
        let text = "- a\n- b\n\n^list-id\n\n```\ncode\n```\n\n{.bg-gray}\n";
        let out = parse(text);
        assert_eq!(
            kinds(text),
            [
                BlockKind::List { ordered: false, start: 1 },
                BlockKind::Marker,
                BlockKind::CodeFence { info: String::new() },
                BlockKind::Marker,
            ]
        );
        assert_eq!(out.blocks[0].persisted_id.as_deref(), Some("list-id"));
        assert_eq!(out.blocks[2].attrs.classes, ["bg-gray"]);
    }

    #[test]
    fn callouts() {
        let text = "> [!tip]- 常用参数\n> 正文一\n> 正文二\n\n> [!danger]\n> 危险\n\n> 普通引用\n";
        let out = parse(text);
        let b = &out.blocks;
        assert_eq!(
            b[0].kind,
            BlockKind::Callout {
                kind: CalloutKind::Tip,
                title: Some("常用参数".into()),
                fold: Fold::Collapsed
            }
        );
        assert_eq!(b[0].children.len(), 1);
        assert_eq!(&text[b[0].children[0].range.clone()], "正文一\n> 正文二");
        assert_eq!(
            b[1].kind,
            BlockKind::Callout { kind: CalloutKind::Danger, title: None, fold: Fold::None }
        );
        assert_eq!(&text[b[1].children[0].range.clone()], "危险");
        assert_eq!(b[2].kind, BlockKind::BlockQuote);
    }

    #[test]
    fn link_reference_definition_becomes_raw() {
        let text = "[foo]: /url \"title\"\n\n[foo]\n";
        let out = parse(text);
        assert_eq!(out.report.raw_gaps, 1);
        assert_eq!(out.blocks[0].kind, BlockKind::Raw);
        assert_eq!(&text[out.blocks[0].range.clone()], "[foo]: /url \"title\"");
        assert_eq!(out.blocks[1].kind, BlockKind::Paragraph);
    }

    #[test]
    fn footnote_definition_is_put_back_in_source_order() {
        let text = "a[^1]\n\n[^1]: note\n\nb\n";
        assert_eq!(
            kinds(text),
            [BlockKind::Paragraph, BlockKind::FootnoteDefinition { name: "1".into() }, BlockKind::Paragraph,]
        );
    }

    #[test]
    fn directive_closer_does_not_truncate_the_last_list_paragraph() {
        for body in ["second item", "列表第二项"] {
            let source = format!(":::highlight\n- {body}\n:::\n");
            let parsed = parse(&source);
            let paragraphs = ParsedBlock::iter(&parsed.blocks)
                .filter(|(_, b)| b.kind == BlockKind::Paragraph)
                .map(|(_, b)| &source[b.range.clone()])
                .collect::<Vec<_>>();
            let arena = comrak::Arena::new();
            let root = comrak::parse_document(&arena, &source, &comrak_options());
            let positions = root
                .descendants()
                .map(|n| {
                    let data = n.data();
                    (format!("{:?}", data.value), data.sourcepos)
                })
                .collect::<Vec<_>>();
            assert_eq!(paragraphs, [body], "{positions:?}; {parsed:?}");
        }
    }

    #[test]
    fn directives_nest_by_colon_count() {
        let text = "::::columns {gap=2}\n:::column\nleft\n:::\n:::column\nright\n:::\n::::\n";
        let out = parse(text);
        assert_eq!(out.blocks.len(), 1);
        let outer = &out.blocks[0];
        assert_eq!(outer.kind, BlockKind::Directive { name: "columns".into() });
        assert_eq!(outer.attrs.pairs, [("gap".to_string(), "2".to_string())]);
        assert_eq!(outer.children.len(), 2);
        assert_eq!(outer.children[1].kind, BlockKind::Directive { name: "column".into() });
    }

    #[test]
    fn empty_document() {
        let out = parse("");
        assert!(out.blocks.is_empty());
        let out = parse("\n\n  \n");
        assert!(out.blocks.is_empty());
    }
}
