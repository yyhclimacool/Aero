//! L1 标记的文本级识别：段尾 / 独占行的 `^id` 与 `{属性列表}`（6.3.3、6.3.10），
//! callout 首行 `[!type]±  标题`（6.3.7）。这些 comrak 都不认识，由 aero-core 后处理。

use crate::block::{Attrs, CalloutKind, Fold};

fn is_block_id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-'
}

fn is_escaped(src: &str, i: usize) -> bool {
    i > 0 && src.as_bytes()[i - 1] == b'\\'
}

/// 段落末尾的标记。`content_len` 是去掉尾部标记与其前置空白后的正文长度。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Trailing {
    pub content_len: usize,
    pub id: Option<String>,
    pub attrs: Option<Attrs>,
}

/// 识别 `正文 {.red} ^id` 形式的段尾标记。标记必须位于最后一行末尾、前有空格。
pub fn strip_trailing(src: &str) -> Trailing {
    let mut end = src.trim_end().len();
    let line_start = src[..end].rfind('\n').map_or(0, |i| i + 1);
    let mut out = Trailing { content_len: end, id: None, attrs: None };

    // ^id
    if let Some(rel) = src[line_start..end].rfind('^') {
        let caret = line_start + rel;
        let tok = &src[caret + 1..end];
        if !tok.is_empty()
            && tok.bytes().all(is_block_id_byte)
            && caret > line_start
            && src.as_bytes()[caret - 1] == b' '
            && !is_escaped(src, caret)
        {
            out.id = Some(tok.to_owned());
            end = src[..caret].trim_end().len();
        }
    }

    // {attrs}
    if end > line_start
        && src.as_bytes()[end - 1] == b'}'
        && let Some(rel) = src[line_start..end].rfind('{')
    {
        let open = line_start + rel;
        if open > line_start
            && src.as_bytes()[open - 1] == b' '
            && !is_escaped(src, open)
            && let Some(attrs) = Attrs::parse_inner(&src[open + 1..end - 1])
        {
            out.attrs = Some(attrs);
            end = src[..open].trim_end().len();
        }
    }

    out.content_len = end;
    out
}

/// 独占一行的标记：`^id`、`{attrs}`、`{attrs} ^id`。整行除标记外没有别的内容才成立。
pub fn parse_marker_line(line: &str) -> Option<(Option<String>, Option<Attrs>)> {
    let line = line.trim();
    if line.contains('\n') {
        return None;
    }
    if let Some(tok) = line.strip_prefix('^') {
        return (!tok.is_empty() && tok.bytes().all(is_block_id_byte)).then(|| (Some(tok.to_owned()), None));
    }
    if let Some(rest) = line.strip_prefix('{') {
        let close = rest.find('}')?;
        let attrs = Attrs::parse_inner(&rest[..close])?;
        let tail = rest[close + 1..].trim();
        if tail.is_empty() {
            return Some((None, Some(attrs)));
        }
        let tok = tail.strip_prefix('^')?;
        if !tok.is_empty() && tok.bytes().all(is_block_id_byte) {
            return Some((Some(tok.to_owned()), Some(attrs)));
        }
    }
    None
}

/// Callout 首行。返回类型、折叠标记、标题，以及首行在 `src` 中的结束位置（`\n` 的下标或 `src.len()`）。
pub fn callout_header(src: &str) -> Option<(CalloutKind, Fold, Option<String>, usize)> {
    let line_end = src.find('\n').unwrap_or(src.len());
    let line = &src[..line_end];
    let body = line.trim_start().trim_start_matches('>').trim_start_matches(' ');
    let inner = body.strip_prefix("[!")?;
    let close = inner.find(']')?;
    let ty = &inner[..close];
    if ty.is_empty() || !ty.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let mut rest = &inner[close + 1..];
    let fold = if let Some(r) = rest.strip_prefix('-') {
        rest = r;
        Fold::Collapsed
    } else if let Some(r) = rest.strip_prefix('+') {
        rest = r;
        Fold::Expanded
    } else {
        Fold::None
    };
    // 折叠标记之后必须是空白或行尾，否则 `[!note]x` 不算 callout
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let title = rest.trim();
    let title = (!title.is_empty()).then(|| title.to_owned());
    Some((CalloutKind::parse(ty), fold, title, line_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_id_and_attrs() {
        let t = strip_trailing("这是一个段落。 {.red} ^para-id");
        assert_eq!(t.id.as_deref(), Some("para-id"));
        assert_eq!(t.attrs.as_ref().unwrap().classes, ["red"]);
        assert_eq!(&"这是一个段落。 {.red} ^para-id"[..t.content_len], "这是一个段落。");

        let t = strip_trailing("多行\n第二行 ^x");
        assert_eq!(t.id.as_deref(), Some("x"));
        assert_eq!(t.content_len, "多行\n第二行".len());
    }

    #[test]
    fn trailing_negative_cases() {
        // a^b 不是 block id
        assert_eq!(strip_trailing("a^b").id, None);
        // 转义
        assert_eq!(strip_trailing("x \\^id").id, None);
        // 不是属性列表的花括号留在原地
        let t = strip_trailing("f(x) = {1, 2}");
        assert_eq!(t.attrs, None);
        assert_eq!(t.content_len, "f(x) = {1, 2}".len());
        // 整行只有 ^id 时不算段尾标记（那是独占行标记）
        assert_eq!(strip_trailing("^only").id, None);
    }

    #[test]
    fn marker_lines() {
        assert_eq!(parse_marker_line("^list-id"), Some((Some("list-id".into()), None)));
        let (id, attrs) = parse_marker_line("{.bg-gray} ^x").unwrap();
        assert_eq!(id.as_deref(), Some("x"));
        assert_eq!(attrs.unwrap().classes, ["bg-gray"]);
        assert_eq!(parse_marker_line("{.bg-gray}").unwrap().0, None);
        assert_eq!(parse_marker_line("^"), None);
        assert_eq!(parse_marker_line("{1, 2}"), None);
        assert_eq!(parse_marker_line("text ^id"), None);
    }

    #[test]
    fn callout_headers() {
        let (k, f, t, end) = callout_header("> [!tip]- 常用参数\n> 正文").unwrap();
        assert_eq!(k, CalloutKind::Tip);
        assert_eq!(f, Fold::Collapsed);
        assert_eq!(t.as_deref(), Some("常用参数"));
        assert_eq!(end, "> [!tip]- 常用参数".len());

        let (k, f, t, _) = callout_header("> [!NOTE]").unwrap();
        assert_eq!((k, f, t), (CalloutKind::Note, Fold::None, None));

        assert!(callout_header("> [!note]x").is_none());
        assert!(callout_header("> [note]").is_none());
        assert!(callout_header("> 普通引用").is_none());
    }
}
