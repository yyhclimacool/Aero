//! `/` 块菜单（`ui_design.md` 7.1）的数据部分：条目、触发判定、过滤。纯函数，UI 在 `editor/mod.rs`。
//!
//! M1a 的最小集合：文本、标题 1/2/3、无序 / 有序 / 任务列表、引用、代码块、分割线、今天日期；
//! M2 加上颜色（`/颜色` 开菜单，`/红色`、`/蓝色背景` 直接命中，见 7.7）。
//! callout / 数学 / mermaid / 表格 / 图片 / 嵌入 / 属性随对应里程碑加入。

use super::color_menu;
use super::ops::BlockType;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlashAction {
    Convert(BlockType),
    /// 在触发位置插入 `YYYY-MM-DD`
    InsertDate,
    /// 打开颜色菜单（7.7）
    OpenColors,
    /// 直接给当前块上色：`color_menu` 的条目编号
    Color(usize),
}

/// 菜单里的一条候选。颜色是按查询即时生成的，不在 [`ITEMS`] 里。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Match {
    /// [`ITEMS`] 下标
    Item(usize),
    /// `color_menu` 的条目编号
    Color(usize),
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
        label: "颜色",
        hint: "⌘/",
        keywords: &["color", "ys", "palette"],
        action: SlashAction::OpenColors,
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

/// 按查询过滤，空查询返回全部固定条目。
/// 匹配规则：标签包含查询，或任一别名以查询开头（都不区分大小写）。
///
/// 颜色（7.7 的 20 项）只在输入了东西之后参与匹配：`/红色`、`/蓝色背景` 是"直接命中"，
/// 不该把默认菜单刷满；想挑颜色走 `/颜色` 开菜单。
pub fn filter(query: &str) -> Vec<Match> {
    let q = query.trim().to_lowercase();
    let items = ITEMS
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            q.is_empty()
                || item.label.to_lowercase().contains(&q)
                || item.keywords.iter().any(|k| k.starts_with(&q))
        })
        .map(|(i, _)| Match::Item(i));
    let colors = match q.is_empty() {
        true => Vec::new(),
        false => color_menu::filter(&q).into_iter().map(Match::Color).collect(),
    };
    items.chain(colors).collect()
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

    /// 只取固定条目的标签，颜色候选另外断言。
    fn labels(query: &str) -> Vec<&'static str> {
        filter(query)
            .into_iter()
            .filter_map(|m| match m {
                Match::Item(i) => Some(ITEMS[i].label),
                Match::Color(_) => None,
            })
            .collect()
    }

    #[test]
    fn filter_by_label_or_alias() {
        assert_eq!(filter("").len(), ITEMS.len());
        // "co" 同时是 code 与 color 的前缀
        assert_eq!(labels("co"), ["代码块", "颜色"]);
        assert_eq!(labels("dmk"), ["代码块"]);
        assert_eq!(labels("标题").len(), 3);
        assert_eq!(labels("H2"), ["标题 2"]);
        assert!(filter("zzz").is_empty());
    }

    /// 颜色是"直接命中"：输入了才出现，`/颜色` 仍然是开菜单那一条（7.1 / 7.7）。
    #[test]
    fn colors_join_the_menu_only_once_something_is_typed() {
        assert!(filter("").iter().all(|m| matches!(m, Match::Item(_))));
        assert_eq!(labels("颜色"), ["颜色"]);
        let blue = filter("蓝色背景");
        assert_eq!(blue.len(), 1);
        let Match::Color(ix) = blue[0] else { panic!("{blue:?}") };
        assert_eq!(color_menu::full_name(ix), "蓝色背景");
        // `/红色` 命中文字色那一项；块类型条目不受影响
        let red: Vec<String> = filter("红色")
            .into_iter()
            .filter_map(|m| match m {
                Match::Color(ix) => Some(color_menu::full_name(ix)),
                Match::Item(_) => None,
            })
            .collect();
        assert_eq!(red, ["红色", "红色背景"]);
    }
}
