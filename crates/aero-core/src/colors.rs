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
/// 选区跨行、跨块或改完结构变了一律拒绝，不写坏文档。
pub fn inline_edit(src: &str, selection: Range<usize>, slot: Slot, color: Option<&str>) -> Option<Inline> {
    if slot == Slot::Background && color == Some("yellow") {
        return Some(Inline::Highlight);
    }
    if let Some(color) = color
        && !PALETTE.contains(&color)
    {
        return None;
    }
    let selected = src.get(selection.clone())?;
    if selected.trim().is_empty() || selected.contains(['\n', '\r']) {
        return None;
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
