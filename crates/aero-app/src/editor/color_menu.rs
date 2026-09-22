//! 颜色菜单（`ui_design.md` 7.7）的数据部分：条目、名字、过滤、网格走位。纯函数，UI 在 `colors_ui.rs`。
//!
//! 条目编号固定：`0..=9` 是文字组（0 为「默认」，其后按调色板顺序），`10..=19` 是背景组。
//! 过滤与 `/` 菜单共用（7.1 的 `/红色`、`/蓝色背景` 直接命中就是这里的条目）。

use aero_core::colors::{PALETTE, Slot};

/// 菜单里的一项：某一组的某个颜色。`color` 为 `None` 是「默认」，也就是移除这一组。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Item {
    pub slot: Slot,
    pub color: Option<&'static str>,
}

/// 调色板颜色的中文名，与 [`PALETTE`] 同序。
const NAMES: [&str; 9] = ["灰", "棕", "橙", "黄", "绿", "蓝", "紫", "粉", "红"];

/// 一组里的条目数：默认 + 9 色。
const PER_SLOT: usize = PALETTE.len() + 1;

/// 条目总数（文字组 + 背景组）。
pub const COUNT: usize = 2 * PER_SLOT;

/// 一行放几个样本（7.7 的排版：默认 + 4 色一行，其余 5 色一行）。
pub const COLUMNS: usize = 5;

/// 编号对应的条目。
pub fn item(ix: usize) -> Item {
    let slot = if ix < PER_SLOT { Slot::Text } else { Slot::Background };
    let within = ix % PER_SLOT;
    Item { slot, color: within.checked_sub(1).map(|c| PALETTE[c]) }
}

/// 样本旁边的短名：「默认」「红」。
pub fn label(ix: usize) -> &'static str {
    match item(ix).color {
        Some(color) => NAMES[PALETTE.iter().position(|&c| c == color).expect("调色板里的颜色")],
        None => "默认",
    }
}

/// 完整名字：「红色」「蓝色背景」「默认颜色」「默认背景」。"上次使用"那一行与 `/` 菜单用它。
pub fn full_name(ix: usize) -> String {
    let item = item(ix);
    let color = match item.color {
        Some(_) => format!("{}色", label(ix)),
        None => "默认".to_owned(),
    };
    match item.slot {
        Slot::Text => match item.color {
            Some(_) => color,
            None => "默认颜色".to_owned(),
        },
        Slot::Background => format!("{color}背景"),
    }
}

/// 组名：过滤时「文字」「背景」「bg」能整组命中。
fn slot_names(slot: Slot) -> [&'static str; 3] {
    match slot {
        Slot::Text => ["文字", "text", "fg"],
        Slot::Background => ["背景", "bj", "bg"],
    }
}

/// 按查询过滤，返回条目编号；空查询返回全部。
///
/// 中文名（「蓝」）、完整名（「蓝色背景」）、英文颜色名（`blue`）、类名（`bg-blue`）、
/// 组名（「背景」/`bg`）都能匹配，都不区分大小写。
pub fn filter(query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return (0..COUNT).collect();
    }
    (0..COUNT)
        .filter(|&ix| {
            let item = item(ix);
            let class = item.color.map(|color| item.slot.class(color)).unwrap_or_default();
            full_name(ix).contains(&q)
                || class.contains(&q)
                || item.color.is_some_and(|color| color.starts_with(&q))
                || slot_names(item.slot).iter().any(|name| *name == q)
        })
        .collect()
}

/// 过滤结果的网格：两组各自按 [`COLUMNS`] 折行。格子里放的是 `matches` 里的位置，不是条目编号。
pub fn rows(matches: &[usize]) -> Vec<Vec<usize>> {
    let mut rows: Vec<Vec<usize>> = Vec::new();
    for slot in [Slot::Text, Slot::Background] {
        let group: Vec<usize> = (0..matches.len()).filter(|&at| item(matches[at]).slot == slot).collect();
        rows.extend(group.chunks(COLUMNS).map(<[usize]>::to_vec));
    }
    rows
}

/// 键盘走一步，收发的都是 `matches` 里的位置：`dx` 沿阅读顺序左右走（两头回绕），
/// `dy` 上下换行并尽量保持列号（目标行更短时落到行尾）。
pub fn step(matches: &[usize], selected: usize, dx: isize, dy: isize) -> usize {
    let rows = rows(matches);
    let Some((row, col)) = rows
        .iter()
        .enumerate()
        .find_map(|(r, cells)| cells.iter().position(|&at| at == selected).map(|c| (r, c)))
    else {
        return selected;
    };
    if dy != 0 {
        let target = (row as isize + dy).rem_euclid(rows.len() as isize) as usize;
        let cells = &rows[target];
        return cells[col.min(cells.len() - 1)];
    }
    let flat: Vec<usize> = rows.iter().flatten().copied().collect();
    let at = rows[..row].iter().map(Vec::len).sum::<usize>() + col;
    flat[(at as isize + dx).rem_euclid(flat.len() as isize) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_cover_both_slots_and_the_default_entry() {
        assert_eq!(COUNT, 20);
        assert_eq!(item(0), Item { slot: Slot::Text, color: None });
        assert_eq!(item(1), Item { slot: Slot::Text, color: Some("gray") });
        assert_eq!(item(9), Item { slot: Slot::Text, color: Some("red") });
        assert_eq!(item(10), Item { slot: Slot::Background, color: None });
        assert_eq!(item(19), Item { slot: Slot::Background, color: Some("red") });
        assert_eq!(label(0), "默认");
        assert_eq!(label(6), "蓝");
        assert_eq!(full_name(0), "默认颜色");
        assert_eq!(full_name(6), "蓝色");
        assert_eq!(full_name(10), "默认背景");
        assert_eq!(full_name(16), "蓝色背景");
    }

    #[test]
    fn filter_matches_chinese_english_class_names_and_whole_groups() {
        assert_eq!(filter("").len(), COUNT);
        // 中文：两组的蓝都命中
        assert_eq!(filter("蓝"), [6, 16]);
        // 英文颜色名与类名
        assert_eq!(filter("blue"), [6, 16]);
        assert_eq!(filter("bg-blue"), [16]);
        // 组名整组命中，"bg" 是背景组的别名
        assert_eq!(filter("bg"), (10..20).collect::<Vec<_>>());
        assert_eq!(filter("文字"), (0..10).collect::<Vec<_>>());
        // 完整名
        assert_eq!(filter("蓝色背景"), [16]);
        assert_eq!(filter("默认"), [0, 10]);
        assert!(filter("chartreuse").is_empty());
    }

    #[test]
    fn keyboard_walks_the_grid_by_rows_and_wraps() {
        let all: Vec<usize> = (0..COUNT).collect();
        // 每组两行，每行 5 个
        assert_eq!(rows(&all).len(), 4);
        assert_eq!(rows(&all)[0], [0, 1, 2, 3, 4]);
        assert_eq!(rows(&all)[3], [15, 16, 17, 18, 19]);
        // 左右在整个列表里走，两头回绕
        assert_eq!(step(&all, 0, 1, 0), 1);
        assert_eq!(step(&all, 0, -1, 0), COUNT - 1);
        assert_eq!(step(&all, COUNT - 1, 1, 0), 0);
        // 上下换行保持列号
        assert_eq!(step(&all, 2, 0, 1), 7);
        assert_eq!(step(&all, 7, 0, 1), 12);
        assert_eq!(step(&all, 2, 0, -1), 17);
        // 过滤后网格重排，列号超出的一行落到最后一个
        let filtered = filter("蓝");
        assert_eq!(rows(&filtered), [[0], [1]]);
        assert_eq!(step(&filtered, 0, 0, 1), 1);
        assert_eq!(step(&filtered, 1, 1, 0), 0);
    }
}
