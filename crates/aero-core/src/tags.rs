//! 标签补全位置与索引共用 Markdown 上下文和字符规则。

use std::ops::Range;

use crate::index::tag_char;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub range: Range<usize>,
    pub cursor: usize,
    /// 不含 `#`，保留用户大小写；匹配时再归一化。
    pub query: String,
}

/// 非行首、空格 / tab 后的 `#`；已有标签中间编辑时替换整枚标签，保留后面的标点。
pub fn completion(text: &str, cursor: usize) -> Option<Completion> {
    let before = text.get(..cursor)?;
    let start = before.rfind('#')?;
    let prefix = &before[..start];
    if !prefix.ends_with([' ', '\t']) || prefix.rsplit('\n').next()?.trim().is_empty() {
        return None;
    }
    // 未闭合的 wikilink 尚未形成 AST 节点，也不能把其中的 fragment 当成标签。
    if prefix.rfind("[[").is_some_and(|open| prefix.rfind("]]").is_none_or(|close| open > close)) {
        return None;
    }
    let query = &before[start + 1..];
    if !query.chars().all(tag_char) || query.chars().next().is_some_and(char::is_numeric) {
        return None;
    }
    let end = cursor + text[cursor..].chars().take_while(|c| tag_char(*c)).map(char::len_utf8).sum::<usize>();
    // 唯一探针保证命中的就是光标位置；交给索引同一套解析器排除代码、数学、标题、URL、属性等。
    let mut marker = "aero_tag_completion".to_owned();
    while text.contains(&marker) {
        marker.push('_');
    }
    let mut probe = text.to_owned();
    probe.replace_range(start..end, &format!("#{marker}"));
    let (sources, _) = crate::parse::inline_scan(&probe);
    if !sources.iter().any(|part| part.contains(&marker)) {
        return None;
    }
    Some(Completion { range: start..end, cursor, query: query.to_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_completion_handles_unicode_hierarchy_and_existing_suffix() {
        for text in ["正文 #", "正文 #中文/工具", "body\t#rust-cli_v2", "> 正文 #rust", "- 正文 #rust"]
        {
            let c = completion(text, text.len()).unwrap();
            assert_eq!(&text[c.range], format!("#{}", c.query));
        }
        let c = completion("body #rust/cli，tail", "body #ru".len()).unwrap();
        assert_eq!(c.query, "ru");
        assert_eq!(c.range, 5..14);
        assert!(completion("中 #中", 6).is_none(), "mid UTF-8 byte is invalid");
    }

    #[test]
    fn tag_completion_obeys_index_context_and_heading_boundary() {
        for text in [
            "#abc",
            "  #abc",
            "body\n #abc",
            "word#abc",
            "body ##abc",
            "body #123",
            "# Heading #abc",
            "body ` #abc`",
            "```md\nbody #abc\n```",
            "    body #abc",
            "body $x #abc$",
            "$$\nbody #abc\n$$",
            "---\ntags: #abc\n---",
            "body \\#abc",
            "[x](https://host/#abc)",
            "body https://host/#abc",
            "body [[name #abc]]",
            "body [[name #abc",
            "body {.red #abc}",
            "<div>\nbody #abc\n</div>",
        ] {
            let cursor = text.find("abc").map_or(text.len(), |i| i + 3);
            assert!(completion(text, cursor).is_none(), "{text}");
        }
    }
}
