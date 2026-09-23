//! 颜色的承载与写回（方案 6.3.10、UI 设计 7.7）：属性列表 `{.red}` / `{.bg-blue}`。
//!
//! 这里只算源码编辑，不管渲染——具体色值是主题的事。调色板与 Notion 对齐，文字色直接用类名、
//! 背景色加 `bg-` 前缀，两组可以叠加（红字 + 黄底）。属性列表里不认识的类名与 `key=value`
//! 一律原样保留，`^id` 始终留在属性之后（`正文 {.red} ^para-id`）。

use std::ops::Range;

use crate::Edit;
use crate::block::Attrs;

/// 与 Notion 对齐的 9 色（UI 7.7）。
pub const PALETTE: [&str; 9] =
    ["gray", "brown", "orange", "yellow", "green", "blue", "purple", "pink", "red"];

/// 上色的两组：文字色与背景色。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    Text,
    Background,
}

impl Slot {
    /// 这一组里某个颜色对应的类名：文字 `red`、背景 `bg-red`。
    pub fn class(self, color: &str) -> String {
        match self {
            Slot::Text => color.to_owned(),
            Slot::Background => format!("bg-{color}"),
        }
    }

    /// 类名是否属于这一组的调色板（不认识的类名不归我们管，要原样留着）。
    pub fn owns(self, class: &str) -> bool {
        match self {
            Slot::Text => PALETTE.contains(&class),
            Slot::Background => class.strip_prefix("bg-").is_some_and(|c| PALETTE.contains(&c)),
        }
    }

    /// 当前属性列表里这一组的颜色。
    pub fn color_of(self, attrs: &Attrs) -> Option<&str> {
        attrs.classes.iter().find(|c| self.owns(c)).map(|class| match self {
            Slot::Text => class.as_str(),
            Slot::Background => class.strip_prefix("bg-").unwrap_or(class),
        })
    }
}

/// `at` 处的属性列表：`{...}` 里全是合法 token 时返回属性与结束位置。`\{` 不算。
pub(crate) fn attrs_at(src: &str, at: usize) -> Option<(Attrs, usize)> {
    let rest = src.get(at..)?;
    if !rest.starts_with('{') || src[..at].chars().rev().take_while(|&c| c == '\\').count() % 2 == 1 {
        return None;
    }
    let close = rest.find('}')?;
    Attrs::parse_inner(&rest[1..close]).map(|attrs| (attrs, at + close + 1))
}

/// 块末属性列表里的颜色。
pub fn block_color(src: &str, block: Range<usize>, slot: Slot) -> Option<String> {
    let text = src.get(block)?;
    let trailing = crate::parse::markers::strip_trailing(text.trim_end());
    slot.color_of(trailing.attrs.as_ref()?).map(str::to_owned)
}

/// 给一个块上色 / 去色：`color` 为 `None` 时移除这一组（对应菜单里的"默认"）。
///
/// 只改块末的属性列表，不动正文、不动 `^id`、不动其他类名；属性空了就把 ` {...}` 一并删掉。
/// 已经是这个颜色时返回 `None`（无需编辑）。多行块的独占行属性（6.3.10 末尾那种）暂不支持。
pub fn block_edit(src: &str, block: Range<usize>, slot: Slot, color: Option<&str>) -> Option<Edit> {
    if let Some(color) = color
        && !PALETTE.contains(&color)
    {
        return None;
    }
    let text = src.get(block.clone())?;
    // 保留块末的空白 / 换行：只重写正文那一段
    let body_len = text.trim_end().len();
    let (body, tail) = (&text[..body_len], &text[body_len..]);
    let trailing = crate::parse::markers::strip_trailing(body);
    let mut attrs = trailing.attrs.clone().unwrap_or_default();
    let had = slot.color_of(&attrs).map(str::to_owned);
    if had.as_deref() == color {
        return None;
    }
    attrs.classes.retain(|class| !slot.owns(class));
    if let Some(color) = color {
        attrs.classes.push(slot.class(color));
    }
    let mut out = String::with_capacity(body.len() + 16);
    out.push_str(&body[..trailing.content_len]);
    if !attrs.is_empty() {
        out.push(' ');
        out.push('{');
        out.push_str(&render(&attrs));
        out.push('}');
    }
    if let Some(id) = &trailing.id {
        out.push_str(&format!(" ^{id}"));
    }
    out.push_str(tail);
    Some(Edit::replace(block, out))
}

/// 选区当前承载的颜色（菜单画选中标记、工具条显示按下态都要读它）。
///
/// 承载者与 [`inline_edit`] 认的是同一批：`[文本]{...}`、紧跟选区的 `{...}`，
/// 以及等价于黄色背景的 `==文本==`。
pub fn inline_color(src: &str, selection: Range<usize>, slot: Slot) -> Option<String> {
    let selected = src.get(selection.clone())?;
    if selected.is_empty() {
        return None;
    }
    if slot == Slot::Background
        && src[..selection.start].ends_with("==")
        && src[selection.end..].starts_with("==")
    {
        return Some("yellow".to_owned());
    }
    let bracketed = src[..selection.start].ends_with('[') && src[selection.end..].starts_with("]{");
    let brace = if bracketed { selection.end + 1 } else { selection.end };
    let (attrs, _) = attrs_at(src, brace)?;
    slot.color_of(&attrs).map(str::to_owned)
}

/// 行内颜色的写回方式。
pub enum Inline {
    Edit(Edit),
    /// 黄色背景按 6.3.10 写成 `==文本==`（兼容性最好）：调用方走
    /// `inline_format::plan(.., Format::Highlight)` 那条路，切换逻辑与 `⌘⇧H` 完全一致。
    Highlight,
}

/// 给一段选中的文字上色 / 去色（UI 7.7 的"文字"与"背景"两组）。
///
/// 选区已经被 `[文本]{...}` 或紧跟 `{...}` 的行内元素承载时就地改属性，否则包一层 `[文本]{.类名}`。
/// 去色后属性列表空了就连 `{}` 一起收掉，如果 `[ ]` 也不再承载任何属性则一并去掉。
/// 选区比已有的颜色范围更大、并且把它整段盖住时，收掉里面的颜色标记，整段只用这次的颜色
/// （范围逐渐扩大时以最大范围为准）。选区缩在某一段颜色里面时仍套一层，外层保持原色。
/// 选区跨行、跨块或改完结构变了一律拒绝，不写坏文档。
pub fn inline_edit(src: &str, selection: Range<usize>, slot: Slot, color: Option<&str>) -> Option<Inline> {
    if let Some(color) = color
        && color != "yellow"
        && !PALETTE.contains(&color)
    {
        return None;
    }
    let selected = src.get(selection.clone())?;
    if selected.trim().is_empty() || selected.contains(['\n', '\r']) {
        return None;
    }
    if let Some(edit) = flatten_covered(src, &selection, slot, color) {
        return Some(edit);
    }
    if slot == Slot::Background && color == Some("yellow") {
        return Some(Inline::Highlight);
    }
    // 已有承载者：`[文本]{...}`（选区在括号内）或 `文本{...}`（选区恰好是元素本身）
    let bracketed = src[..selection.start].ends_with('[') && src[selection.end..].starts_with("]{");
    let brace = if bracketed { selection.end + 1 } else { selection.end };
    let existing = attrs_at(src, brace).map(|(attrs, end)| (attrs, brace..end));
    let mut attrs = existing.as_ref().map(|(attrs, _)| attrs.clone()).unwrap_or_default();
    if slot.color_of(&attrs).map(str::to_owned).as_deref() == color {
        return None;
    }
    attrs.classes.retain(|class| !slot.owns(class));
    if let Some(color) = color {
        attrs.classes.push(slot.class(color));
    }
    // 改写区间：有承载者就连着它一起重写，否则只包选区
    let range = match &existing {
        Some((_, attrs_range)) => {
            if bracketed {
                selection.start - 1..attrs_range.end
            } else {
                selection.start..attrs_range.end
            }
        }
        None => selection.clone(),
    };
    let body = if bracketed || existing.is_none() { selected } else { &src[selection.clone()] };
    let mut out = String::with_capacity(range.len() + 16);
    if attrs.is_empty() {
        // 没有属性可挂：`[ ]` 也不留（它只是属性的容器）
        out.push_str(body);
    } else {
        // 选区恰好是一个行内元素时属性直接挂在它后面（6.3.10 的写回规则），不再多包一层
        let wrap = match (&existing, bracketed) {
            (Some(_), true) => true,
            (Some(_), false) => false,
            (None, _) => !is_element(selected),
        };
        if wrap {
            out.push('[');
        }
        out.push_str(body);
        if wrap {
            out.push(']');
        }
        out.push('{');
        out.push_str(&render(&attrs));
        out.push('}');
    }
    let mut after = src.to_owned();
    after.replace_range(range.clone(), &out);
    // 结构没变才算安全：包裹不能把段落、链接或表格改坏
    let before = crate::parse::parse(src).blocks;
    if !crate::inline_links::same_structure(&before, &crate::parse::parse(&after).blocks) {
        return None;
    }
    Some(Inline::Edit(Edit::replace(range, out)))
}

/// 替换文本里真正被上色的那一段（不含 `[ ]` / `{...}` / `==`）。上色后用它把选区留在正文上。
pub fn painted_content(replacement: &str) -> Range<usize> {
    if let Some(inner) = replacement.strip_prefix('[')
        && let Some(end) = inner.find("]{")
        && attrs_at(replacement, 1 + end + 1).is_some_and(|(_, at)| at == replacement.len())
    {
        return 1..1 + end;
    }
    if let Some(inner) = replacement.strip_prefix("==")
        && let Some(body) = inner.strip_suffix("==")
        && !body.is_empty()
        && !body.contains("==")
    {
        return 2..2 + body.len();
    }
    if let Some(brace) = replacement.rfind('{')
        && brace > 0
        && attrs_at(replacement, brace).is_some_and(|(_, end)| end == replacement.len())
    {
        return 0..brace;
    }
    0..replacement.len()
}

/// 选区盖住了更小的颜色范围：收掉那些标记，再按这次的颜色写整段。
/// 没有被盖住的颜色时返回 `None`，调用方走原来的就地修改 / 套一层。
fn flatten_covered(src: &str, selection: &Range<usize>, slot: Slot, color: Option<&str>) -> Option<Inline> {
    let holes = covered_markup(src, selection);
    if holes.is_empty() {
        return None;
    }
    let mut range = selection.clone();
    for hole in &holes {
        range.start = range.start.min(hole.start);
        range.end = range.end.max(hole.end);
    }
    let body = splice_out(src, &range, &holes);
    if body.trim().is_empty() || body.contains(['\n', '\r']) {
        return None;
    }
    // 选区正好是外层 `[文本]{...}` 的正文：外层保留，只换这次的这一组，里面的颜色已经收掉
    let out = if src[..selection.start].ends_with('[')
        && src[selection.end..].starts_with("]{")
        && bracket_start(src, selection.end, line_start(src, selection.end)) == Some(selection.start - 1)
    {
        let (mut attrs, end) = attrs_at(src, selection.end + 1)?;
        range.start = selection.start - 1;
        range.end = end;
        if slot == Slot::Background && color == Some("yellow") {
            format!("=={body}==")
        } else {
            attrs.classes.retain(|class| !slot.owns(class));
            if let Some(color) = color {
                attrs.classes.push(slot.class(color));
            }
            if attrs.is_empty() { body } else { format!("[{body}]{{{}}}", render(&attrs)) }
        }
    } else if slot == Slot::Background && color == Some("yellow") {
        format!("=={body}==")
    } else if let Some(color) = color {
        let class = slot.class(color);
        if is_element(&body) { format!("{body}{{.{class}}}") } else { format!("[{body}]{{.{class}}}") }
    } else {
        body
    };
    let mut after = src.to_owned();
    after.replace_range(range.clone(), &out);
    let before = crate::parse::parse(src).blocks;
    if !crate::inline_links::same_structure(&before, &crate::parse::parse(&after).blocks) {
        return None;
    }
    Some(Inline::Edit(Edit::replace(range, out)))
}

/// 被选区整段盖住、并且选区还伸出这段颜色之外的标记。这些标记要删掉。
fn covered_markup(src: &str, selection: &Range<usize>) -> Vec<Range<usize>> {
    let mut holes = Vec::new();
    let mut search = 0;
    while let Some(rel) = src[search..].find("]{") {
        let close = search + rel;
        let Some((attrs, end)) = attrs_at(src, close + 1) else {
            search = close + 2;
            continue;
        };
        let Some(open) = bracket_start(src, close, line_start(src, close)) else {
            search = end;
            continue;
        };
        let content = open + 1..close;
        if expands_over(selection, &content) && has_color(&attrs) && only_colors(&attrs) {
            holes.push(open..open + 1);
            holes.push(close..end);
        }
        search = end;
    }
    search = 0;
    while let Some(rel) = src[search..].find('{') {
        let brace = search + rel;
        if brace > 0 && src.as_bytes().get(brace - 1) == Some(&b']') {
            search = brace + 1;
            continue;
        }
        let Some((attrs, end)) = attrs_at(src, brace) else {
            search = brace + 1;
            continue;
        };
        let Some(element) = element_before(src, brace) else {
            search = end;
            continue;
        };
        if expands_over(selection, &element)
            && has_color(&attrs)
            && only_colors(&attrs)
            && end <= selection.end
        {
            holes.push(brace..end);
        }
        search = end;
    }
    search = selection.start;
    while let Some(rel) = src[search..selection.end].find("==") {
        let start = search + rel;
        if escaped(src, start) {
            search = start + 2;
            continue;
        }
        let Some(end_rel) = src[start + 2..selection.end].find("==") else { break };
        let end = start + 2 + end_rel + 2;
        if escaped(src, end - 2) {
            search = start + 2;
            continue;
        }
        let content = start + 2..end - 2;
        if expands_over(selection, &content)
            && !content.is_empty()
            && !src[content.clone()].contains('\n')
            && !holes.iter().any(|hole| hole.start < end && start < hole.end)
        {
            holes.push(start..start + 2);
            holes.push(end - 2..end);
        }
        search = end;
    }
    holes.retain(|hole| !hole.is_empty());
    holes.sort_by_key(|hole| hole.start);
    holes
}

/// 选区盖住 `content`，并且至少有一侧伸到这段颜色外面。正好重合是就地改色，不算扩大。
fn expands_over(selection: &Range<usize>, content: &Range<usize>) -> bool {
    !content.is_empty()
        && selection.start <= content.start
        && content.end <= selection.end
        && (selection.start < content.start || selection.end > content.end)
}

fn has_color(attrs: &Attrs) -> bool {
    attrs.classes.iter().any(|class| Slot::Text.owns(class) || Slot::Background.owns(class))
}

fn only_colors(attrs: &Attrs) -> bool {
    attrs.id.is_none()
        && attrs.pairs.is_empty()
        && has_color(attrs)
        && attrs.classes.iter().all(|class| Slot::Text.owns(class) || Slot::Background.owns(class))
}

fn element_before(src: &str, brace: usize) -> Option<Range<usize>> {
    let before = &src[..brace];
    for delimiter in ["**", "~~", "==", "`", "*", "_"] {
        if !before.ends_with(delimiter) {
            continue;
        }
        let stem_end = before.len() - delimiter.len();
        let stem = &before[..stem_end];
        let open = stem.rfind(delimiter)?;
        let interior = &src[open + delimiter.len()..stem_end];
        if interior.is_empty() || interior.contains('\n') {
            continue;
        }
        return Some(open..brace);
    }
    None
}

fn splice_out(src: &str, range: &Range<usize>, holes: &[Range<usize>]) -> String {
    let mut out = String::with_capacity(range.len());
    let mut at = range.start;
    for hole in holes {
        if hole.end <= at || hole.start >= range.end {
            continue;
        }
        let start = hole.start.max(range.start);
        let end = hole.end.min(range.end);
        if start < at {
            continue;
        }
        out.push_str(&src[at..start]);
        at = end;
    }
    out.push_str(&src[at..range.end]);
    out
}

fn line_start(src: &str, at: usize) -> usize {
    src[..at].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// `close` 处 `]` 配对的 `[`（同一行内、未转义、允许嵌套）。
fn bracket_start(src: &str, close: usize, limit: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (at, ch) in src[limit..close].char_indices().rev() {
        let at = limit + at;
        if !matches!(ch, '[' | ']') || escaped(src, at) {
            continue;
        }
        if ch == ']' {
            depth += 1;
        } else if depth == 0 {
            return Some(at);
        } else {
            depth -= 1;
        }
    }
    None
}

fn escaped(src: &str, at: usize) -> bool {
    src[..at].chars().rev().take_while(|&c| c == '\\').count() % 2 == 1
}

/// 选区是否恰好是一个完整的行内元素（`**粗**`、`*斜*`、`` `码` ``、`~~删~~`、`==高亮==`）。
/// 链接 / 嵌入等其他形态先按"包一层"处理，由 `same_structure` 兜底拒绝改坏结构的情况。
fn is_element(selected: &str) -> bool {
    ["**", "~~", "==", "`", "*", "_"]
        .iter()
        .any(|d| selected.len() > d.len() * 2 && selected.starts_with(d) && selected.ends_with(d))
}

/// 属性列表内部的源码：`#id` 在前，其次类名，最后 `key=value`，与解析顺序一致。
fn render(attrs: &Attrs) -> String {
    let mut parts = Vec::new();
    if let Some(id) = &attrs.id {
        parts.push(format!("#{id}"));
    }
    parts.extend(attrs.classes.iter().map(|class| format!(".{class}")));
    parts.extend(attrs.pairs.iter().map(|(k, v)| format!("{k}={v}")));
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::{Inline, PALETTE, Slot, block_color, block_edit, inline_color, inline_edit};
    use crate::Document;

    /// 在文档上真正应用一次块上色，返回改完的源码。
    fn apply(source: &str, ix: usize, slot: Slot, color: Option<&str>) -> String {
        let mut doc = Document::from_text(source);
        let block = doc.blocks().iter().nth(ix).map(|(_, b)| doc.range_of(b)).expect("block");
        match block_edit(&doc.to_string(), block, slot, color) {
            Some(edit) => {
                doc.edit(edit).unwrap();
                doc.to_string()
            }
            None => source.to_owned(),
        }
    }

    #[test]
    fn block_colors_write_back_next_to_ids_and_keep_unknown_attributes() {
        // 新上色：属性列表加在块末，前面留一个空格
        assert_eq!(apply("段落\n", 0, Slot::Background, Some("blue")), "段落 {.bg-blue}\n");
        // 文字色与背景色叠加，互不覆盖
        let stacked = apply("段落 {.bg-yellow}\n", 0, Slot::Text, Some("red"));
        assert_eq!(stacked, "段落 {.bg-yellow .red}\n");
        // 换同一组的颜色只替换这一组
        assert_eq!(apply(&stacked, 0, Slot::Background, Some("green")), "段落 {.red .bg-green}\n");
        // "默认"＝移除这一组；属性空了就把 {} 一起删掉
        assert_eq!(apply("段落 {.red}\n", 0, Slot::Text, None), "段落\n");
        assert_eq!(apply("段落 {.red .bg-blue}\n", 0, Slot::Text, None), "段落 {.bg-blue}\n");
        // 不认识的类名、`key=value`、`#id` 全部原样保留
        let kept = apply("段落 {.custom lang=zh #anchor}\n", 0, Slot::Text, Some("pink"));
        assert_eq!(kept, "段落 {#anchor .custom .pink lang=zh}\n");
        assert_eq!(apply(&kept, 0, Slot::Text, None), "段落 {#anchor .custom lang=zh}\n");
        // `^id` 始终留在属性之后
        assert_eq!(apply("段落 ^para\n", 0, Slot::Text, Some("blue")), "段落 {.blue} ^para\n");
        assert_eq!(apply("段落 {.red} ^para\n", 0, Slot::Text, None), "段落 ^para\n");
        // 标题与 Pandoc 一致
        assert_eq!(apply("# 标题\n", 0, Slot::Text, Some("orange")), "# 标题 {.orange}\n");
    }

    #[test]
    fn block_colors_are_read_back_and_no_op_when_unchanged() {
        let doc = Document::from_text("段落 {.red .bg-blue} ^para\n");
        let source = doc.to_string();
        let block = doc.blocks().iter().next().map(|(_, b)| doc.range_of(b)).unwrap();
        assert_eq!(block_color(&source, block.clone(), Slot::Text).as_deref(), Some("red"));
        assert_eq!(block_color(&source, block.clone(), Slot::Background).as_deref(), Some("blue"));
        // 已经是这个颜色 / 本来就没有：不产生编辑
        assert!(block_edit(&source, block.clone(), Slot::Text, Some("red")).is_none());
        assert!(block_edit(&source, block.clone(), Slot::Text, None).is_some());
        let plain = Document::from_text("段落\n");
        let range = plain.blocks().iter().next().map(|(_, b)| plain.range_of(b)).unwrap();
        assert!(block_edit(&plain.to_string(), range.clone(), Slot::Text, None).is_none());
        // 不在调色板里的颜色一律拒绝，不写脏数据
        assert!(block_edit(&plain.to_string(), range, Slot::Text, Some("chartreuse")).is_none());
    }

    /// 行内上色：没有承载者就包 `[文本]{...}`，已有的就地改，去空了连括号一起收掉。
    #[test]
    fn inline_colors_wrap_reuse_and_unwrap_their_carrier() {
        let edit = |src: &str, needle: &str, slot, color| {
            let at = src.find(needle).expect(needle);
            match inline_edit(src, at..at + needle.len(), slot, color) {
                Some(Inline::Edit(edit)) => {
                    let mut doc = Document::from_text(src);
                    doc.edit(edit).unwrap();
                    doc.to_string()
                }
                Some(Inline::Highlight) => "<highlight>".to_owned(),
                None => src.to_owned(),
            }
        };
        // 没有承载者：包一层
        assert_eq!(edit("前 文字 后\n", "文字", Slot::Text, Some("red")), "前 [文字]{.red} 后\n");
        // 已有 `[文本]{...}`：就地改这一组，另一组不动
        let stacked = edit("前 [文字]{.red} 后\n", "文字", Slot::Background, Some("blue"));
        assert_eq!(stacked, "前 [文字]{.red .bg-blue} 后\n");
        assert_eq!(edit(&stacked, "文字", Slot::Text, Some("green")), "前 [文字]{.bg-blue .green} 后\n");
        // 去色：属性空了连 `[ ]` 一起收掉
        assert_eq!(edit("前 [文字]{.red} 后\n", "文字", Slot::Text, None), "前 文字 后\n");
        assert_eq!(edit(&stacked, "文字", Slot::Background, None), "前 [文字]{.red} 后\n");
        // 选区恰好是一个行内元素：属性挂在元素后，不再多包一层
        assert_eq!(edit("前 **粗** 后\n", "**粗**", Slot::Text, Some("blue")), "前 **粗**{.blue} 后\n");
        assert_eq!(
            edit("前 **粗**{.blue} 后\n", "**粗**", Slot::Text, Some("pink")),
            "前 **粗**{.pink} 后\n"
        );
        assert_eq!(edit("前 **粗**{.blue} 后\n", "**粗**", Slot::Text, None), "前 **粗** 后\n");
        // 黄色背景走 `==` 那条路；重复上色与非法颜色不产生编辑
        assert_eq!(edit("前 文字 后\n", "文字", Slot::Background, Some("yellow")), "<highlight>");
        assert_eq!(edit("前 [文字]{.red} 后\n", "文字", Slot::Text, Some("red")), "前 [文字]{.red} 后\n");
        assert_eq!(edit("前 文字 后\n", "文字", Slot::Text, Some("chartreuse")), "前 文字 后\n");
        assert_eq!(edit("前 文字 后\n", "文字", Slot::Text, None), "前 文字 后\n");
        // 跨行、空白选区拒绝
        let source = "第一行\n第二行\n";
        assert!(inline_edit(source, 0..source.len(), Slot::Text, Some("red")).is_none());
        assert!(inline_edit("前 文字\n", 1..2, Slot::Text, Some("red")).is_none());
    }

    /// 选区扩大盖住已有颜色时，以这一次的范围为准，不再一层层套；缩小则外层保留、内层用新颜色。
    #[test]
    fn expanding_a_colored_selection_replaces_inner_colors() {
        let edit = |src: &str, needle: &str, slot, color| {
            let at = src.find(needle).expect(needle);
            match inline_edit(src, at..at + needle.len(), slot, color) {
                Some(Inline::Edit(edit)) => {
                    let mut doc = Document::from_text(src);
                    doc.edit(edit).unwrap();
                    doc.to_string()
                }
                Some(Inline::Highlight) => "<highlight>".to_owned(),
                None => src.to_owned(),
            }
        };
        let grown = "也可以通过[「文件 → 关闭 Vault」回到库选择页，再从最近列]{.blue}表切换。";
        assert_eq!(
            edit(&format!("{grown}\n"), grown, Slot::Text, Some("pink")),
            "[也可以通过「文件 → 关闭 Vault」回到库选择页，再从最近列表切换。]{.pink}\n"
        );
        let nested =
            "[也[可以通过[「文件 → 关闭 Vault」回到库选择页，再从最近列]{.blue}表]{.pink}切换。]{.bg-blue}";
        assert_eq!(
            edit(&format!("{nested}\n"), nested, Slot::Background, Some("green")),
            "[也可以通过「文件 → 关闭 Vault」回到库选择页，再从最近列表切换。]{.bg-green}\n"
        );
        // 选区正好是外层正文：收掉里面的颜色，外层换成本次的颜色
        let outer = "也[可以通过[「文件」]{.blue}表]{.pink}切换。";
        assert_eq!(
            edit(&format!("[{outer}]{{.bg-blue}}\n"), outer, Slot::Text, Some("red")),
            "[也可以通过「文件」表切换。]{.bg-blue .red}\n"
        );
        // 选区缩小到已有颜色里面：外层保持，内层套新颜色
        let wide = "[也可以通过「文件」表切换。]{.bg-blue}\n";
        assert_eq!(
            edit(wide, "「文件」", Slot::Text, Some("blue")),
            "[也可以通过[「文件」]{.blue}表切换。]{.bg-blue}\n"
        );
    }

    /// 读回选区上的颜色：菜单用它画选中标记。
    #[test]
    fn inline_colors_are_read_back_from_their_carrier() {
        let color = |src: &str, needle: &str, slot| {
            let at = src.find(needle).expect(needle);
            inline_color(src, at..at + needle.len(), slot)
        };
        let stacked = "前 [文字]{.red .bg-blue} 后\n";
        assert_eq!(color(stacked, "文字", Slot::Text).as_deref(), Some("red"));
        assert_eq!(color(stacked, "文字", Slot::Background).as_deref(), Some("blue"));
        // 属性挂在元素后的形态
        assert_eq!(color("前 **粗**{.pink} 后\n", "**粗**", Slot::Text).as_deref(), Some("pink"));
        // `==文本==` 就是黄色背景
        assert_eq!(color("前 ==高亮== 后\n", "高亮", Slot::Background).as_deref(), Some("yellow"));
        assert_eq!(color("前 ==高亮== 后\n", "高亮", Slot::Text), None);
        // 没有承载者、未知类名、空选区都读不出颜色
        assert_eq!(color("前 文字 后\n", "文字", Slot::Text), None);
        assert_eq!(color("前 [文字]{.custom} 后\n", "文字", Slot::Text), None);
        assert_eq!(inline_color("文字\n", 0..0, Slot::Text), None);
    }

    #[test]
    fn palette_classes_round_trip_through_both_slots() {
        for color in PALETTE {
            assert_eq!(Slot::Text.class(color), color);
            assert_eq!(Slot::Background.class(color), format!("bg-{color}"));
            assert!(Slot::Text.owns(color) && !Slot::Text.owns(&format!("bg-{color}")));
            assert!(Slot::Background.owns(&format!("bg-{color}")) && !Slot::Background.owns(color));
        }
        // 调色板外的类名不归任何一组
        for class in ["custom", "bg-custom", "bg-", "redish"] {
            assert!(!Slot::Text.owns(class) && !Slot::Background.owns(class), "{class}");
        }
    }
}
