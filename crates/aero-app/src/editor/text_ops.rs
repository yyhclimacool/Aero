//! 纯文本层面的小工具：块文本 diff、列表标记、字数统计。不依赖 GPUI，可单测。

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

/// front matter 源码里 `title:` 一行的值：返回值在 `src` 内的字节区间（不含引号）与去引号后的文本。
/// 只认最常见的写法（顶层 `title: xxx`，可带单/双引号）；别的形式当作没有 title。
pub fn front_matter_title(src: &str) -> Option<(std::ops::Range<usize>, String)> {
    let mut at = 0;
    for line in src.split_inclusive('\n') {
        let start = at;
        at += line.len();
        let body = line.trim_end_matches(['\n', '\r']);
        let Some(rest) = body.strip_prefix("title:") else { continue };
        let value_start = start + "title:".len() + (rest.len() - rest.trim_start().len());
        let value = rest.trim();
        let value_end = value_start + value.len();
        let (range, text) = match value.as_bytes() {
            [q @ (b'"' | b'\''), .., last] if *last == *q && value.len() >= 2 => {
                (value_start + 1..value_end - 1, value[1..value.len() - 1].to_owned())
            }
            _ => (value_start..value_end, value.to_owned()),
        };
        return Some((range, text));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // 空值：区间为空，可以往里写
        let (r, t) = front_matter_title("---\ntitle:\n---").unwrap();
        assert_eq!(t, "");
        assert!(r.is_empty());
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
