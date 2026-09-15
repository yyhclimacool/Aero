//! `/` 块菜单（`ui_design.md` 7.1）的数据部分：条目、触发判定、过滤。纯函数，UI 在 `editor/mod.rs`。
//!
//! M1a 的最小集合：文本、标题 1/2/3、无序 / 有序 / 任务列表、引用、代码块、分割线、今天日期。
//! callout / 数学 / mermaid / 表格 / 图片 / 嵌入 / 属性 / 颜色随对应里程碑加入。

use super::ops::BlockType;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlashAction {
    Convert(BlockType),
    /// 在触发位置插入 `YYYY-MM-DD`
    InsertDate,
}

pub struct SlashItem {
    pub label: &'static str,
    /// 右侧淡色提示（快捷键或说明）
    pub hint: &'static str,
    /// 过滤用的别名（小写、英文 / 拼音首字母），标签本身也参与匹配
    pub keywords: &'static [&'static str],
    pub action: SlashAction,
}

pub const ITEMS: &[SlashItem] = &[
    SlashItem {
        label: "文本",
        hint: "⌘⌥0",
        keywords: &["text", "wb", "paragraph", "p"],
        action: SlashAction::Convert(BlockType::Text),
    },
    SlashItem {
        label: "标题 1",
        hint: "⌘⌥1",
        keywords: &["h1", "heading", "bt1", "title"],
        action: SlashAction::Convert(BlockType::Heading(1)),
    },
    SlashItem {
        label: "标题 2",
        hint: "⌘⌥2",
        keywords: &["h2", "heading", "bt2"],
        action: SlashAction::Convert(BlockType::Heading(2)),
    },
    SlashItem {
        label: "标题 3",
        hint: "⌘⌥3",
        keywords: &["h3", "heading", "bt3"],
        action: SlashAction::Convert(BlockType::Heading(3)),
    },
    SlashItem {
        label: "无序列表",
        hint: "⌘⌥4",
        keywords: &["bullet", "list", "ul", "wxlb", "-"],
        action: SlashAction::Convert(BlockType::Bullet),
    },
    SlashItem {
        label: "有序列表",
        hint: "⌘⌥5",
        keywords: &["number", "numbered", "ol", "yxlb", "1."],
        action: SlashAction::Convert(BlockType::Numbered),
    },
    SlashItem {
        label: "任务",
        hint: "⌘⌥6",
        keywords: &["todo", "task", "checkbox", "rw", "[]"],
        action: SlashAction::Convert(BlockType::Task),
    },
    SlashItem {
        label: "引用",
        hint: "⌘⌥7",
        keywords: &["quote", "blockquote", "yy", ">"],
        action: SlashAction::Convert(BlockType::Quote),
    },
    SlashItem {
        label: "代码块",
        hint: "⌘⌥8",
        keywords: &["code", "fence", "dmk", "```"],
        action: SlashAction::Convert(BlockType::Code),
    },
    SlashItem {
        label: "分割线",
        hint: "---",
        keywords: &["divider", "hr", "rule", "fgx", "---"],
        action: SlashAction::Convert(BlockType::Divider),
    },
    SlashItem {
        label: "今天日期",
        hint: "YYYY-MM-DD",
        keywords: &["date", "today", "jtrq", "rq"],
        action: SlashAction::InsertDate,
    },
];

/// 光标前的文本里是否有正在输入的 `/` 命令：`/` 在块首或空白之后，且 `/` 到光标之间没有空白。
/// 返回 `/` 在 `text` 中的偏移与查询串。
pub fn trigger(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let before = text.get(..cursor)?;
    let slash = before.rfind('/')?;
    let query = &before[slash + 1..];
    if query.contains(char::is_whitespace) || query.contains('/') {
        return None;
    }
    let at_start = slash == 0 || before[..slash].ends_with(char::is_whitespace);
    at_start.then_some((slash, query))
}

/// 按查询过滤条目，返回 `ITEMS` 下标；空查询返回全部。
/// 匹配规则：标签包含查询，或任一别名以查询开头（都不区分大小写）。
pub fn filter(query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return (0..ITEMS.len()).collect();
    }
    ITEMS
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item.label.to_lowercase().contains(&q) || item.keywords.iter().any(|k| k.starts_with(&q))
        })
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_only_at_block_start_or_after_space() {
        assert_eq!(trigger("/", 1), Some((0, "")));
        assert_eq!(trigger("/h1", 3), Some((0, "h1")));
        assert_eq!(trigger("hello /co", 9), Some((6, "co")));
        // 光标不在末尾：只看光标之前
        assert_eq!(trigger("/code more", 5), Some((0, "code")));
        // 词中间的 `/`、有空格的查询、连着的 `/` 都不触发
        assert_eq!(trigger("a/b", 3), None);
        assert_eq!(trigger("/a b", 4), None);
        assert_eq!(trigger("//", 2), None);
        assert_eq!(trigger("no slash", 8), None);
    }

    #[test]
    fn filter_by_label_or_alias() {
        assert_eq!(filter("").len(), ITEMS.len());
        let code = filter("co");
        assert_eq!(code.len(), 1);
        assert_eq!(ITEMS[code[0]].label, "代码块");
        assert_eq!(filter("标题").len(), 3);
        assert_eq!(ITEMS[filter("H2")[0]].label, "标题 2");
        assert!(filter("zzz").is_empty());
    }
}
