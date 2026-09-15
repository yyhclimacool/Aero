//! Anchor：随编辑自动漂移的文本位置（ADR 0004）。
//!
//! 实现是一张 marker 表：每个 anchor 一个 `(pos, bias)`，每次编辑对全表做一次 O(m) 平移。
//! 10 MB 文档约 19 万 marker，一次平移 ≈ 50 µs。表在每次重解析后整体重建，
//! 旧 anchor 通过 generation 识别为失效，避免拿着过期句柄读出错误位置。

use std::ops::Range;

use crate::text::Edit;

/// 在 anchor 所在位置插入文本时，anchor 留在插入文本之前（Left）还是之后（Right）。
///
/// 块区间用 `start: Left, end: Right`，这样在块首、块尾输入的字符都归入该块。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Bias {
    Left,
    Right,
}

/// marker 表中的一个位置句柄。`Copy`，可以放进 Block、选区、光标。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Anchor {
    index: u32,
    generation: u32,
}

/// 一组 anchor 及其当前位置。
#[derive(Clone, Debug, Default)]
pub struct MarkerTable {
    generation: u32,
    pos: Vec<usize>,
    bias: Vec<Bias>,
}

impl MarkerTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// 建一张新表，generation 比 `prev` 大，使 `prev` 发出的 anchor 全部失效。
    pub fn successor(prev: &MarkerTable) -> Self {
        MarkerTable { generation: prev.generation.wrapping_add(1), pos: Vec::new(), bias: Vec::new() }
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn len(&self) -> usize {
        self.pos.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pos.is_empty()
    }

    pub fn anchor(&mut self, pos: usize, bias: Bias) -> Anchor {
        let index = u32::try_from(self.pos.len()).expect("more than u32::MAX anchors");
        self.pos.push(pos);
        self.bias.push(bias);
        Anchor { index, generation: self.generation }
    }

    /// 块区间：起点 Left、终点 Right。
    pub fn anchor_range(&mut self, range: Range<usize>) -> Range<Anchor> {
        self.anchor(range.start, Bias::Left)..self.anchor(range.end, Bias::Right)
    }

    /// anchor 是否由本表（当前 generation）发出。
    pub fn owns(&self, a: Anchor) -> bool {
        a.generation == self.generation && (a.index as usize) < self.pos.len()
    }

    pub fn try_resolve(&self, a: Anchor) -> Option<usize> {
        self.owns(a).then(|| self.pos[a.index as usize])
    }

    /// 解析 anchor 的当前位置。拿着过期 anchor 来解析是逻辑错误，直接 panic 而不是返回错位。
    pub fn resolve(&self, a: Anchor) -> usize {
        self.try_resolve(a).unwrap_or_else(|| {
            panic!("stale anchor: generation {} (table is {})", a.generation, self.generation)
        })
    }

    pub fn resolve_range(&self, r: &Range<Anchor>) -> Range<usize> {
        self.resolve(r.start)..self.resolve(r.end)
    }

    /// 文本 `edit.range` 被替换为 `edit.text` 后，平移全部 marker。
    pub fn apply(&mut self, edit: &Edit) {
        let Range { start, end } = edit.range;
        let new_len = edit.text.len();
        let delta = edit.delta();
        for (p, bias) in self.pos.iter_mut().zip(&self.bias) {
            *p = shift(*p, *bias, start, end, new_len, delta);
        }
    }
}

#[inline]
fn shift(p: usize, bias: Bias, start: usize, end: usize, new_len: usize, delta: isize) -> usize {
    if p < start {
        p
    } else if p >= end && !(p == start && bias == Bias::Left) {
        // 完全在编辑之后（含 Right 偏置的 start 位置）：整体平移
        (p as isize + delta) as usize
    } else if p == start {
        // Left 偏置停在 start；Right 偏置到替换文本之后
        match bias {
            Bias::Left => p,
            Bias::Right => start + new_len,
        }
    } else {
        // 落在被删除区间内部：按偏置塌缩到替换区间的一端
        match bias {
            Bias::Left => start,
            Bias::Right => start + new_len,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_with(range: Range<usize>) -> (MarkerTable, Range<Anchor>) {
        let mut t = MarkerTable::new();
        let r = t.anchor_range(range);
        (t, r)
    }

    #[test]
    fn insert_before_shifts_whole_range() {
        let (mut t, r) = table_with(10..20);
        t.apply(&Edit::insert(0, "abc"));
        assert_eq!(t.resolve_range(&r), 13..23);
    }

    #[test]
    fn insert_at_start_grows_range() {
        let (mut t, r) = table_with(10..20);
        t.apply(&Edit::insert(10, "ab"));
        assert_eq!(t.resolve_range(&r), 10..22);
    }

    #[test]
    fn insert_at_end_grows_range() {
        let (mut t, r) = table_with(10..20);
        t.apply(&Edit::insert(20, "ab"));
        assert_eq!(t.resolve_range(&r), 10..22);
    }

    #[test]
    fn insert_after_is_noop() {
        let (mut t, r) = table_with(10..20);
        t.apply(&Edit::insert(21, "ab"));
        assert_eq!(t.resolve_range(&r), 10..20);
    }

    #[test]
    fn delete_inside_shrinks() {
        let (mut t, r) = table_with(10..20);
        t.apply(&Edit::delete(12..15));
        assert_eq!(t.resolve_range(&r), 10..17);
    }

    #[test]
    fn delete_across_start_collapses_start_to_edit_start() {
        let (mut t, r) = table_with(10..20);
        t.apply(&Edit::delete(5..15));
        assert_eq!(t.resolve_range(&r), 5..10);
    }

    #[test]
    fn delete_across_end_collapses_end() {
        let (mut t, r) = table_with(10..20);
        t.apply(&Edit::replace(15..25, "xy"));
        // end 落在被删区间内，Right 偏置塌缩到替换文本之后
        assert_eq!(t.resolve_range(&r), 10..17);
    }

    #[test]
    fn delete_covering_whole_range_gives_empty_range() {
        let (mut t, r) = table_with(10..20);
        t.apply(&Edit::delete(0..30));
        assert_eq!(t.resolve_range(&r), 0..0);
    }

    #[test]
    fn stale_anchor_is_detected() {
        let (t, r) = table_with(0..1);
        let t2 = MarkerTable::successor(&t);
        assert!(!t2.owns(r.start));
        assert_eq!(t2.try_resolve(r.start), None);
    }
}
