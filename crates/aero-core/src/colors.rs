//! 颜色的承载与写回（方案 6.3.10、UI 设计 7.7）：属性列表 `{.red}` / `{.bg-blue}`。
//!
//! 这里只算源码编辑，不管渲染——具体色值是主题的事。调色板与 Notion 对齐，文字色直接用类名、
//! 背景色加 `bg-` 前缀，两组可以叠加（红字 + 黄底）。属性列表里不认识的类名与 `key=value`
//! 一律原样保留，`^id` 始终留在属性之后（`正文 {.red} ^para-id`）。

use std::ops::Range;

use comrak::{Arena, parse_document};

use crate::Edit;
use crate::block::{Attrs, tokenize_attrs};
use crate::highlight::{self, Carrier};
use crate::inline_links::SourceMap;

/// 与 Notion 对齐的 9 色（UI 7.7）。
pub const PALETTE: [&str; 9] =
    ["gray", "brown", "orange", "yellow", "green", "blue", "purple", "pink", "red"];

/// 上色的两组：文字色与背景色。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    Text,
    Background,
    Border,
}

impl Slot {
    /// 这一组里某个颜色对应的类名：文字 `red`、背景 `bg-red`。
    pub fn class(self, color: &str) -> String {
        match self {
            Slot::Text => color.to_owned(),
            Slot::Background => format!("bg-{color}"),
            Slot::Border => format!("border-{color}"),
        }
    }

    /// 类名是否属于这一组的调色板（不认识的类名不归我们管，要原样留着）。
    pub fn owns(self, class: &str) -> bool {
        match self {
            Slot::Text => PALETTE.contains(&class),
            Slot::Background => class.strip_prefix("bg-").is_some_and(|c| PALETTE.contains(&c)),
            Slot::Border => class.strip_prefix("border-").is_some_and(|c| PALETTE.contains(&c)),
        }
    }

    /// 当前属性列表里这一组的颜色。
    pub fn color_of(self, attrs: &Attrs) -> Option<&str> {
        if let Some((_, value)) = attrs
            .pairs
            .iter()
            .rev()
            .find(|(key, value)| key == self.key() && CustomColor::parse(value).is_some())
        {
            return Some(value);
        }
        attrs.classes.iter().find(|c| self.owns(c)).map(|class| match self {
            Slot::Text => class.as_str(),
            Slot::Background => class.strip_prefix("bg-").unwrap_or(class),
            Slot::Border => class.strip_prefix("border-").unwrap_or(class),
        })
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Text => "color",
            Self::Background => "background",
            Self::Border => "border-color",
        }
    }

    pub fn write(self, attrs: &mut Attrs, color: Option<&str>) {
        attrs.classes.retain(|class| !self.owns(class));
        attrs.pairs.retain(|(key, _)| key != self.key());
        if let Some(color) = color {
            if PALETTE.contains(&color) {
                attrs.classes.push(self.class(color));
            } else if let Some(color) = normalize_custom(color) {
                attrs.pairs.push((self.key().into(), color));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CustomColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: f32,
}

impl CustomColor {
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if let Some(hex) = value.strip_prefix('#') {
            if !matches!(hex.len(), 6 | 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let byte = |i| u8::from_str_radix(&hex[i..i + 2], 16).ok();
            return Some(Self {
                red: byte(0)?,
                green: byte(2)?,
                blue: byte(4)?,
                alpha: if hex.len() == 8 { byte(6)? as f32 / 255. } else { 1. },
            });
        }
        let body = value.strip_prefix("rgba(")?.strip_suffix(')')?;
        let parts: Vec<_> = body.split(',').map(str::trim).collect();
        if parts.len() != 4
            || parts.iter().take(3).any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
            || parts[3].is_empty()
            || !parts[3].bytes().all(|b| b.is_ascii_digit() || b == b'.')
        {
            return None;
        }
        let alpha = parts[3].parse::<f64>().ok()?;
        if !(0. ..=1.).contains(&alpha) {
            return None;
        }
        Some(Self {
            red: parts[0].parse().ok()?,
            green: parts[1].parse().ok()?,
            blue: parts[2].parse().ok()?,
            alpha: alpha as f32,
        })
    }
}

pub fn normalize_custom(value: &str) -> Option<String> {
    CustomColor::parse(value)?;
    let value = value.trim();
    if value.starts_with('#') {
        Some(value.to_ascii_uppercase())
    } else {
        let parts: Vec<_> = value[5..value.len() - 1].split(',').map(str::trim).collect();
        // Keep the decimal alpha spelling; floating-point rendering is never serialized.
        Some(format!("rgba({},{},{},{})", parts[0], parts[1], parts[2], parts[3]))
    }
}

fn valid_color(color: &str) -> bool {
    PALETTE.contains(&color) || CustomColor::parse(color).is_some()
}

/// `at` 处的属性列表：`{...}` 里全是合法 token 时返回属性与结束位置。`\{` 不算。
pub(crate) fn attrs_at(src: &str, at: usize) -> Option<(Attrs, usize)> {
    let rest = src.get(at..)?;
    if !rest.starts_with('{') || src[..at].chars().rev().take_while(|&c| c == '\\').count() % 2 == 1 {
        return None;
    }
    let mut quoted = false;
    let mut escaped = false;
    for (offset, ch) in rest.char_indices().skip(1) {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '}' if !quoted => {
                return Attrs::parse_inner(&rest[1..offset]).map(|attrs| (attrs, at + offset + 1));
            }
            _ => {}
        }
    }
    None
}

/// 块末属性列表里的颜色。多行块写在后面独占行上的颜色不在这段正文里，读块上的 `attrs`。
pub fn block_color(src: &str, block: Range<usize>, slot: Slot) -> Option<String> {
    let text = src.get(block)?;
    let trailing = crate::parse::markers::strip_trailing(text.trim_end());
    slot.color_of(trailing.attrs.as_ref()?).map(str::to_owned)
}

/// Return the byte length of visible inline content, excluding attributes and trailing markers.
pub fn inline_content_len(src: &str) -> usize {
    let mut visible = String::new();
    let mut last = 0;
    for (at, _) in src.char_indices().filter(|(_, ch)| *ch == '{') {
        if let Some((_, end)) = attrs_at(src, at) {
            visible.push_str(&src[last..at]);
            last = end;
        }
    }
    visible.push_str(&src[last..]);
    crate::parse::markers::strip_trailing(&visible).content_len
}

/// 给一个块上色 / 去色：`color` 为 `None` 时移除这一组（对应菜单里的"默认"）。
///
/// 单行块把属性列表写在块末。多行块写在后面的独占行 `{.bg-gray}`，规则同 `^id`。
/// 只改这一组颜色，不动正文、不动 `^id`、不动其他类名；属性空了就把标记删掉。
/// 已经是这个颜色时返回 `None`（无需编辑）。
pub fn block_edit(src: &str, block: Range<usize>, slot: Slot, color: Option<&str>) -> Option<Edit> {
    if let Some(color) = color
        && !valid_color(color)
    {
        return None;
    }
    let text = src.get(block.clone())?;
    if text.trim_end().contains('\n') {
        return marker_line_edit(src, block.end, slot, color);
    }
    trailing_block_edit(src, block, slot, color)
}

fn trailing_block_edit(src: &str, block: Range<usize>, slot: Slot, color: Option<&str>) -> Option<Edit> {
    let text = src.get(block.clone())?;
    // 保留块末的空白 / 换行：只重写正文那一段
    let body_len = text.trim_end().len();
    let (body, tail) = (&text[..body_len], &text[body_len..]);
    let trailing = crate::parse::markers::strip_trailing(body);
    let mut attrs = trailing.attrs.clone().unwrap_or_default();
    if color.is_some() && slot.color_of(&attrs) == color {
        return None;
    }
    let before = attrs.clone();
    slot.write(&mut attrs, color);
    if before == attrs {
        return None;
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

/// 多行块的颜色写在块后面的独占行。已有 `{...}` / `^id` 行就改那一行。
fn marker_line_edit(src: &str, at: usize, slot: Slot, color: Option<&str>) -> Option<Edit> {
    if let Some((range, mut attrs, id)) = marker_line_at(src, at) {
        if color.is_some() && slot.color_of(&attrs) == color {
            return None;
        }
        let before = attrs.clone();
        slot.write(&mut attrs, color);
        if before == attrs {
            return None;
        }
        let mut line = String::new();
        if !attrs.is_empty() {
            line.push('{');
            line.push_str(&render(&attrs));
            line.push('}');
        }
        if let Some(id) = id {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push('^');
            line.push_str(&id);
        }
        if line.is_empty() {
            let end = (range.end + 1).min(src.len());
            let start = range.start.saturating_sub(1);
            return Some(Edit::delete(start..end));
        }
        return Some(Edit::replace(range, line));
    }
    let color = color?;
    let mut attrs = Attrs::default();
    slot.write(&mut attrs, Some(color));
    Some(Edit::insert(at, format!("\n\n{{{}}}\n", render(&attrs))))
}

fn marker_line_at(src: &str, at: usize) -> Option<(Range<usize>, Attrs, Option<String>)> {
    let rest = src.get(at..)?;
    let rest = rest.strip_prefix('\n')?;
    let mut line_at = at + 1;
    let mut line = rest;
    if let Some(stripped) = line.strip_prefix('\n') {
        line_at += 1;
        line = stripped;
    }
    let line_len = line.find('\n').unwrap_or(line.len());
    let (id, attrs) = crate::parse::markers::parse_marker_line(&line[..line_len])?;
    Some((line_at..line_at + line_len, attrs.unwrap_or_default(), id))
}

/// 源码态里要淡化的颜色标记：`[`、`]` 和 `{.red}` / `{.bg-blue}`。
pub fn marker_ranges(src: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut search = 0;
    while let Some(rel) = src[search..].find('{') {
        let open = search + rel;
        let Some((attrs, end)) = attrs_at(src, open) else {
            search = open + 1;
            continue;
        };
        if [Slot::Text, Slot::Background].iter().any(|slot| slot.color_of(&attrs).is_some()) {
            ranges.push(open..end);
            if open > 0 && src.as_bytes()[open - 1] == b']' {
                ranges.push(open - 1..open);
                if let Some(bracket) = bracket_start(src, open - 1, line_start(src, open)) {
                    ranges.push(bracket..bracket + 1);
                }
            }
        }
        search = end;
    }
    ranges.sort_by_key(|range| range.start);
    ranges
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

/// Return an invalid custom color value attached to an inline selection.
pub fn inline_invalid_color(src: &str, selection: Range<usize>, slot: Slot) -> Option<String> {
    if selection.is_empty() {
        return None;
    }
    let bracketed = src[..selection.start].ends_with('[') && src[selection.end..].starts_with("]{");
    let brace = if bracketed { selection.end + 1 } else { selection.end };
    let (attrs, _) = attrs_at(src, brace)?;
    attrs
        .pairs
        .iter()
        .find(|(key, value)| *key == slot.key() && CustomColor::parse(value).is_none())
        .map(|(_, value)| value.clone())
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
/// 扩大选区时只移除被完整覆盖的同通道颜色。其他属性仍附着在原来的文字上。
/// 选区跨行、跨块或改完结构变了一律拒绝，不写坏文档。
pub fn inline_edit(src: &str, selection: Range<usize>, slot: Slot, color: Option<&str>) -> Option<Inline> {
    if slot == Slot::Border {
        return None;
    }
    if let Some(color) = color
        && !valid_color(color)
    {
        return None;
    }
    let selected = src.get(selection.clone())?;
    // 没选字时黄色背景就是 `==`：插入一对空标记，光标在中间
    if selected.is_empty() && slot == Slot::Background && color == Some("yellow") {
        return Some(Inline::Highlight);
    }
    if selected.trim().is_empty() || selected.contains(['\n', '\r']) {
        return None;
    }
    let arena = Arena::new();
    let root = parse_document(&arena, src, &crate::parse::comrak_options());
    let positions = SourceMap::new(src);
    let line = line_start(src, selection.start)
        ..src[selection.end..].find('\n').map_or(src.len(), |i| selection.end + i);
    let carriers = highlight::carriers(src, &line, root, &positions);
    let existing =
        carriers.iter().find(|carrier| carrier.content == selection || carrier.full_range() == selection);
    let content = existing.map_or(selection.clone(), |carrier| carrier.content.clone());
    let mut body_range = content.clone();
    let mut edits = Vec::new();
    for carrier in &carriers {
        let whole = carrier.full_range();
        let intersects = whole.start < content.end && content.start < whole.end;
        let inside = carrier.content.start <= content.start && content.end <= carrier.content.end;
        if intersects && !inside && !expands_over(&content, &carrier.content) {
            // Wrapping across a partial carrier would move its attributes onto a different span.
            return None;
        }
        if !expands_over(&content, &carrier.content) {
            continue;
        }
        body_range.start = body_range.start.min(whole.start);
        body_range.end = body_range.end.max(whole.end);
        let original = attribute_source(src, carrier);
        let attrs = write_inline_attrs(original, slot, None);
        if attrs.is_empty() {
            if !carrier.before.is_empty() {
                edits.push(Edit::delete(carrier.before.clone()));
            }
            edits.push(Edit::delete(carrier.after.clone()));
        } else if attrs != original {
            edits.push(Edit::replace(carrier.attrs_range(), format!("{{{attrs}}}")));
        }
    }
    if slot == Slot::Background && src.contains("==") {
        for range in highlight::ranges_in(root, &positions) {
            let open = range.start - 2..range.start;
            let close = range.end..range.end + 2;
            let exact = content == range && open.start <= content.start && content.end <= close.end;
            let expanded =
                expands_over(&content, &range) && content.start <= open.start && close.end <= content.end;
            if color != Some("yellow")
                && (exact || expanded)
                && !carriers.iter().any(|carrier| {
                    [&open, &close].iter().any(|marker| {
                        [&carrier.before, &carrier.after].iter().any(|meta| {
                            !meta.is_empty() && meta.start < marker.end && marker.start < meta.end
                        })
                    })
                })
            {
                body_range.start = body_range.start.min(open.start);
                body_range.end = body_range.end.max(close.end);
                edits.push(Edit::delete(open));
                edits.push(Edit::delete(close));
            }
        }
    }
    let yellow = slot == Slot::Background && color == Some("yellow");
    if yellow && existing.is_none() && edits.is_empty() && body_range == selection {
        return Some(Inline::Highlight);
    }
    let original = existing.map_or("", |carrier| attribute_source(src, carrier));
    let attrs = write_inline_attrs(original, slot, color);
    if edits.is_empty() && attrs == original {
        return None;
    }
    let bracketed = existing.map_or_else(
        || {
            !root.descendants().any(|node| {
                highlight::is_inline_element(&node.data().value)
                    && positions.range(node).is_some_and(|range| range == body_range)
            })
        },
        |carrier| !carrier.before.is_empty(),
    );
    let mut body = src[body_range.clone()].to_owned();
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
    for edit in edits {
        body.replace_range(
            edit.range.start - body_range.start..edit.range.end - body_range.start,
            &edit.text,
        );
    }
    let out = if yellow && write_inline_attrs(original, slot, None).is_empty() {
        format!("=={body}==")
    } else if attrs.is_empty() {
        body
    } else if bracketed {
        format!("[{body}]{{{attrs}}}")
    } else {
        format!("{body}{{{attrs}}}")
    };
    let range = existing.map_or(body_range.clone(), Carrier::full_range);
    let mut after = src.to_owned();
    after.replace_range(range.clone(), &out);
    let before = crate::parse::parse(src).blocks;
    if !crate::inline_links::same_structure(&before, &crate::parse::parse(&after).blocks) {
        return None;
    }
    Some(Inline::Edit(Edit::replace(range, out)))
}

fn attribute_source<'a>(src: &'a str, carrier: &Carrier) -> &'a str {
    let range = carrier.attrs_range();
    &src[range.start + 1..range.end - 1]
}

fn write_inline_attrs(source: &str, slot: Slot, color: Option<&str>) -> String {
    let mut tokens = tokenize_attrs(source).unwrap_or_default();
    let count = tokens.len();
    tokens.retain(|token| {
        !token.strip_prefix('.').is_some_and(|class| slot.owns(class))
            && !token.split_once('=').is_some_and(|(key, _)| key == slot.key())
    });
    if color.is_none() && tokens.len() == count {
        return source.to_owned();
    }
    let mut added = Attrs::default();
    slot.write(&mut added, color);
    let added = render(&added);
    if !added.is_empty() {
        let at = if added.starts_with('.') {
            tokens.iter().position(|token| token.contains('=')).unwrap_or(tokens.len())
        } else {
            tokens.len()
        };
        tokens.insert(at, &added);
    }
    tokens.join(" ")
}

/// 替换文本里真正被上色的那一段（不含 `[ ]` / `{...}` / `==`）。上色后用它把选区留在正文上。
pub fn painted_content(replacement: &str) -> Range<usize> {
    let range = 0..replacement.len();
    if replacement.ends_with('}') || replacement.starts_with("==") {
        let arena = Arena::new();
        let root = parse_document(&arena, replacement, &crate::parse::comrak_options());
        let positions = SourceMap::new(replacement);
        if let Some(carrier) = highlight::carriers(replacement, &range, root, &positions)
            .into_iter()
            .find(|carrier| carrier.full_range() == range)
        {
            return carrier.content;
        }
        if let Some(inner) = highlight::ranges_in(root, &positions)
            .into_iter()
            .find(|inner| inner.start == 2 && inner.end + 2 == range.end)
        {
            return inner;
        }
    }
    range
}

/// 选区盖住 `content`，并且至少有一侧伸到这段颜色外面。正好重合是就地改色，不算扩大。
fn expands_over(selection: &Range<usize>, content: &Range<usize>) -> bool {
    !content.is_empty()
        && selection.start <= content.start
        && content.end <= selection.end
        && (selection.start < content.start || selection.end > content.end)
}

pub(crate) fn line_start(src: &str, at: usize) -> usize {
    src[..at].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// `close` 处 `]` 配对的 `[`（同一行内、未转义、允许嵌套）。
pub(crate) fn bracket_start(src: &str, close: usize, limit: usize) -> Option<usize> {
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

/// 属性列表内部的源码：`#id` 在前，其次类名，最后 `key=value`，与解析顺序一致。
fn render(attrs: &Attrs) -> String {
    crate::properties::render(attrs)
}

#[cfg(test)]
mod tests {
    use super::{
        Inline, PALETTE, Slot, block_color, block_edit, inline_color, inline_content_len, inline_edit,
    };
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
        let list = Document::from_text("- a\n- b\n");
        let list_range = list.blocks().iter().next().map(|(_, b)| list.range_of(b)).unwrap();
        let edited = block_edit(&list.to_string(), list_range, Slot::Background, Some("gray")).unwrap();
        let mut doc = list;
        doc.edit(edited).unwrap();
        doc.reparse();
        assert!(doc.to_string().contains("\n\n{.bg-gray}"), "{}", doc.to_string());
        assert_eq!(
            Slot::Background
                .color_of(&doc.blocks().iter().next().unwrap().1.attrs)
                .map(str::to_owned)
                .as_deref(),
            Some("gray")
        );
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

    #[test]
    fn restoring_default_removes_exact_highlight_delimiters() {
        let source = "前 ==高亮== 后\n";
        let at = source.find("高亮").unwrap();
        let Some(Inline::Edit(edit)) = inline_edit(source, at..at + "高亮".len(), Slot::Background, None)
        else {
            panic!("exact highlight selection should be editable");
        };
        let mut doc = Document::from_text(source);
        doc.edit(edit).unwrap();
        assert_eq!(doc.to_string(), "前 高亮 后\n");
    }

    #[test]
    fn inline_content_length_excludes_attributes_and_markers() {
        assert_eq!(inline_content_len("第一段{background=\"rgba(1,2,3,1.5)\"} {background=\"#bad\"}"), 9);
        assert_eq!(inline_content_len("第一段"), 9);
    }

    #[test]
    fn restoring_default_handles_expanded_and_attributed_highlights() {
        let source = "前 ==高亮== 后\n";
        let Some(Inline::Edit(edit)) =
            inline_edit(source, 0..source.trim_end().len(), Slot::Background, None)
        else {
            panic!("expanded highlight selection should be editable");
        };
        let mut doc = Document::from_text(source);
        doc.edit(edit).unwrap();
        assert_eq!(doc.to_string(), "前 高亮 后\n");

        let source = "前 ==高亮=={.red} 后\n";
        let at = source.find("高亮").unwrap();
        let Some(Inline::Edit(edit)) = inline_edit(source, at..at + "高亮".len(), Slot::Background, None)
        else {
            panic!("attributed highlight should be editable");
        };
        let mut doc = Document::from_text(source);
        doc.edit(edit).unwrap();
        assert_eq!(doc.to_string(), "前 高亮{.red} 后\n");

        assert!(matches!(
            inline_edit(source, at..at + "高亮".len(), Slot::Background, Some("yellow")),
            Some(Inline::Highlight)
        ));
        assert!(inline_edit(source, at + 1..at + 2, Slot::Background, None).is_none());
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
            "[也[可以通过[「文件 → 关闭 Vault」回到库选择页，再从最近列]{.blue}表]{.pink}切换。]{.bg-green}\n"
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

    #[test]
    fn expanding_inline_color_preserves_other_channels_and_attributes() {
        let cases = [
            (
                r#"前 [字]{.red background="rgba(12, 34, 56, 0.374900)" note="keep } verbatim"} 后"#,
                Slot::Text,
                "blue",
                r#"[前 [字]{background="rgba(12, 34, 56, 0.374900)" note="keep } verbatim"} 后]{.blue}"#,
            ),
            (
                r##"前 **粗**{.bg-blue color="#112233"} 后"##,
                Slot::Background,
                "green",
                r##"[前 **粗**{color="#112233"} 后]{.bg-green}"##,
            ),
            ("前 ==亮== 与 [字]{.red} 后", Slot::Text, "blue", "[前 ==亮== 与 字 后]{.blue}"),
            (
                r#"前 [**粗**{.bg-blue} 字]{.red #kept note="a b"} 后"#,
                Slot::Background,
                "yellow",
                r#"前 [**粗** 字]{.red #kept .bg-yellow note="a b"} 后"#,
            ),
        ];
        for (source, slot, color, expected) in cases {
            let selection = if slot == Slot::Background && color == "yellow" {
                source.find("**").unwrap()..source.find("]{").unwrap()
            } else {
                0..source.len()
            };
            let Some(Inline::Edit(edit)) = inline_edit(source, selection, slot, Some(color)) else {
                panic!("missing color edit for {source}");
            };
            let mut doc = Document::from_text(source);
            doc.edit(edit).unwrap();
            assert_eq!(doc.to_string(), expected);
        }
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
