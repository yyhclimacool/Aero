//! 纯文本层面的小工具：块文本 diff、列表标记、字数统计。不依赖 GPUI，可单测。

use std::ops::Range;

use aero_core::Edit;

/// 把"块的旧文本 → 控件里的新文本"收缩成一次最小编辑（公共前后缀之外的部分），
/// 坐标换算到文档绝对偏移。两者相同时返回 `None`。
pub fn minimal_edit(range_start: usize, old: &str, new: &str) -> Option<Edit> {
    if old == new {
        return None;
    }
    let prefix = common_prefix(old, new);
    let max_suffix = old.len().min(new.len()) - prefix;
    let suffix = common_suffix(&old[prefix..], &new[prefix..]).min(max_suffix);
    let old_range = range_start + prefix..range_start + old.len() - suffix;
    Some(Edit::replace(old_range, &new[prefix..new.len() - suffix]))
}

fn common_prefix(a: &str, b: &str) -> usize {
    let mut n = 0;
    for (ca, cb) in a.chars().zip(b.chars()) {
        if ca != cb {
            break;
        }
        n += ca.len_utf8();
    }
    n
}

fn common_suffix(a: &str, b: &str) -> usize {
    let mut n = 0;
    for (ca, cb) in a.chars().rev().zip(b.chars().rev()) {
        if ca != cb {
            break;
        }
        n += ca.len_utf8();
    }
    n
}

/// `offset` 所在行的起点（上一个 `\n` 之后）。
pub fn line_start(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].rfind('\n').map_or(0, |i| i + 1)
}

/// 0-based 行号与行内 UTF-16 列（与 gpui-base 的 `Position` 同单位）。
pub fn line_col(text: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(text.len());
    let line = text[..offset].matches('\n').count() as u32;
    let start = line_start(text, offset);
    let col = text[start..offset].encode_utf16().count() as u32;
    (line, col)
}

/// `text` 的行数（空串为 1 行）。
pub fn line_count(text: &str) -> u32 {
    text.matches('\n').count() as u32 + 1
}

/// 列表项的标记与缩进：`"  - "`、`"3. "`、`"- [ ] "`。用于 Enter 续行（7.3）。
pub struct ListMarker {
    pub indent: String,
    pub marker: String,
}

impl ListMarker {
    /// 从列表项源文本推断标记。`ordered` 为 `Some(下一个序号)` 时生成有序标记，
    /// 复用原项的分隔符（`.` / `)`）；`task` 为任务项时追加未勾选的复选框。
    pub fn from_item_source(item_src: &str, ordered: Option<u32>, task: bool) -> ListMarker {
        let indent: String = item_src.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        let rest = &item_src[indent.len()..];
        let marker = match ordered {
            Some(n) => {
                let delim = rest.chars().find(|c| *c == '.' || *c == ')').unwrap_or('.');
                format!("{n}{delim} ")
            }
            None => {
                let bullet = rest.chars().next().filter(|c| matches!(c, '-' | '*' | '+')).unwrap_or('-');
                format!("{bullet} ")
            }
        };
        let marker = if task { format!("{marker}[ ] ") } else { marker };
        ListMarker { indent, marker }
    }

    pub fn text(&self) -> String {
        format!("{}{}", self.indent, self.marker)
    }
}

/// 字数：CJK 按字计，其他文字按空白分隔的词计（`ui_design.md` 10.1）。
pub fn word_count(text: &str) -> usize {
    let mut count = 0;
    let mut in_word = false;
    for c in text.chars() {
        if is_cjk(c) {
            count += 1;
            in_word = false;
        } else if c.is_alphanumeric() {
            if !in_word {
                count += 1;
                in_word = true;
            }
        } else {
            in_word = false;
        }
    }
    count
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x20000..=0x2A6DF | 0x3040..=0x30FF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF)
}

/// 标题前缀 `#{1,6} ` 的长度（字节），不是标题时为 0。
pub fn heading_prefix_len(text: &str) -> usize {
    let hashes = text.bytes().take_while(|b| *b == b'#').count();
    if (1..=6).contains(&hashes) && text.as_bytes().get(hashes).is_some_and(|b| *b == b' ') {
        hashes + 1
    } else if (1..=6).contains(&hashes) && text.len() == hashes {
        hashes
    } else {
        0
    }
}

/// 光标该停的位置：不停在藏起来的行首标记（`# `、`> `）前面或中间，在那里打字会拆坏块标记。
/// 行内成对标记中间可以停——自动配对跳过闭合符号时，光标就在那里。
pub fn settle_caret(conceal: &[Range<usize>], text: &str, mut caret: usize) -> usize {
    loop {
        let before = caret;
        for range in conceal {
            let at_line_start = range.start == 0 || text.as_bytes().get(range.start - 1) == Some(&b'\n');
            if at_line_start && range.start <= caret && caret < range.end {
                caret = range.end;
            }
        }
        if caret == before {
            return caret.min(text.len());
        }
    }
}

/// 在隐藏了标记的文字里按 ⌫ / ⌦ 删什么。
#[derive(Debug, PartialEq, Eq)]
pub enum Deletion {
    /// 与普通文本框一样删一个字
    Native,
    /// 光标前面只剩藏起来的标记：等于在块首退格
    AtStart,
    /// 后面只剩藏起来的标记：什么都不删
    Nothing,
    /// 删掉控件文字里的这一段，光标停在 `caret`
    Range { range: Range<usize>, caret: usize },
}

/// ⌫：删光标前一个看得见的字，中间隔着的隐藏标记保留。退到上一行时连同那一行藏起来的 `> `
/// 一起删；删空了一对标记（`**a**` 里的 `a`）就连标记一起删，免得剩下 `****` 露出来。
pub fn plan_backspace(
    text: &str,
    conceal: &[Range<usize>],
    spans: &[Range<usize>],
    caret: usize,
) -> Deletion {
    let hidden = |i: usize| conceal.iter().any(|range| range.contains(&i));
    let mut end = caret.min(text.len());
    let start = loop {
        let Some((prev, _)) = text[..end].char_indices().next_back() else { return Deletion::AtStart };
        if hidden(prev) {
            end = prev;
        } else {
            break prev;
        }
    };
    let range = if &text[start..end] == "\n" { start..caret } else { start..end };
    let range = widen_emptied_span(range, conceal, spans);
    if range.end == caret && end == caret && text[range.clone()].chars().count() == 1 {
        return Deletion::Native;
    }
    Deletion::Range { caret: range.start, range }
}

/// ⌦：删光标后一个看得见的字。删到行尾的换行时，下一行藏起来的 `> ` 跟着删。
pub fn plan_delete(text: &str, conceal: &[Range<usize>], spans: &[Range<usize>], caret: usize) -> Deletion {
    let hidden = |i: usize| conceal.iter().any(|range| range.contains(&i));
    let next = |i: usize| text[i..].chars().next().map_or(i, |c| i + c.len_utf8());
    let mut start = caret.min(text.len());
    while start < text.len() && hidden(start) {
        start = next(start);
    }
    if start >= text.len() {
        return if caret >= text.len() { Deletion::Native } else { Deletion::Nothing };
    }
    let mut end = next(start);
    if &text[start..end] == "\n" {
        while end < text.len() && hidden(end) && text.as_bytes()[end] != b'\n' {
            end = next(end);
        }
    }
    let range = widen_emptied_span(start..end, conceal, spans);
    if range.start == caret && range.end == next(caret) {
        return Deletion::Native;
    }
    Deletion::Range { caret: caret.min(range.start), range }
}

/// 要删的正好是一对标记里仅剩的内容时，把两侧的标记也算进去；嵌套的成对标记逐层往外扩。
fn widen_emptied_span(
    mut range: Range<usize>,
    conceal: &[Range<usize>],
    spans: &[Range<usize>],
) -> Range<usize> {
    while spans.contains(&range) {
        let open = conceal.iter().find(|c| c.end == range.start);
        let close = conceal.iter().find(|c| c.start == range.end);
        let (Some(open), Some(close)) = (open, close) else { break };
        range = open.start..close.end;
    }
    range
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str, caret: usize, backspace: bool) -> String {
        let analysis = aero_core::markup::analyze(src);
        let spans: Vec<Range<usize>> = analysis.spans().cloned().collect();
        let plan = if backspace {
            plan_backspace(src, &analysis.conceal, &spans, caret)
        } else {
            plan_delete(src, &analysis.conceal, &spans, caret)
        };
        match plan {
            Deletion::Range { range, .. } => format!("{}{}", &src[..range.start], &src[range.end..]),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn deleting_around_hidden_markers_keeps_the_syntax_intact() {
        // 光标在 `**` 后：删的是 d，标记保留
        assert_eq!(run("a **bold** c", 10, true), "a **bol** c");
        // 删空一对标记：连标记一起删
        assert_eq!(run("a **b** c", 7, true), "a  c");
        assert_eq!(run("a **b** c", 3, false), "a  c");
        assert_eq!(run("x [站](u) y", "x [站](u)".len(), true), "x  y");
        // 引用第二行行首退格：换行与 `> ` 一起删，两行接起来
        assert_eq!(run("> 一\n> 二", 8, true), "> 一二");
        assert_eq!(run("> 一\n> 二", 5, false), "> 一二");
        // 前面只有藏起来的 `# `：交给块首退格
        assert_eq!(run("# 标题", 2, true), "AtStart");
        // 普通字符照常
        assert_eq!(run("abc", 2, true), "Native");
        assert_eq!(run("abc", 1, false), "Native");
        // 后面只剩隐藏标记
        assert_eq!(run("**b**", 3, false), "Nothing");
    }

    #[test]
    fn caret_settles_after_hidden_line_prefixes() {
        let src = "> 一\n> **二**";
        let conceal = aero_core::markup::analyze(src).conceal;
        assert_eq!(settle_caret(&conceal, src, 0), 2);
        assert_eq!(settle_caret(&conceal, src, 1), 2);
        let second = src.find('\n').unwrap() + 1;
        assert_eq!(settle_caret(&conceal, src, second), second + 2);
        // 行内的 `**` 前面与中间都可以停：在前面打字落在粗体外面，中间是自动配对跳过闭合符号的地方
        assert_eq!(settle_caret(&conceal, src, second + 2), second + 2);
        assert_eq!(settle_caret(&conceal, src, second + 3), second + 3);
    }

    #[test]
    fn minimal_edit_shrinks_to_changed_span() {
        let e = minimal_edit(10, "hello world", "hello brave world").unwrap();
        assert_eq!(e.range, 16..16);
        assert_eq!(e.text, "brave ");
        let e = minimal_edit(0, "中文测试", "中测试").unwrap();
        assert_eq!(e.range, 3..6);
        assert_eq!(e.text, "");
        assert!(minimal_edit(0, "same", "same").is_none());
        // 重复字符：后缀不能吃进前缀
        let e = minimal_edit(0, "aa", "aaa").unwrap();
        assert_eq!(e.range, 2..2);
        assert_eq!(e.text, "a");
    }

    #[test]
    fn markers() {
        let m = ListMarker::from_item_source("  * item", None, false);
        assert_eq!(m.text(), "  * ");
        let m = ListMarker::from_item_source("2) item", Some(3), false);
        assert_eq!(m.text(), "3) ");
        let m = ListMarker::from_item_source("- [x] done", None, true);
        assert_eq!(m.text(), "- [ ] ");
    }

    #[test]
    fn counts() {
        assert_eq!(word_count("hello world 你好"), 4);
        assert_eq!(word_count(""), 0);
        assert_eq!(line_col("ab\ncd", 4), (1, 1));
        assert_eq!(line_count("a\nb\n"), 3);
        assert_eq!(heading_prefix_len("## x"), 3);
        assert_eq!(heading_prefix_len("#"), 1);
        assert_eq!(heading_prefix_len("#x"), 0);
    }
}
