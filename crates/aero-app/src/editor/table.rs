//! 表格：浏览与编辑是同一张格子，每个单元格是一个文字控件，点哪格改哪格，列宽不因为点击而变。
//!
//! 列宽的来源（依次）：正在拖的列宽 → 编辑中冻结的列宽 → 分隔行里 `-` 的个数（各列不一样多时按比例）
//! → 按内容估算。拖动列边界后把比例写回分隔行：别的 Markdown 工具里它仍是一张普通表格。

use std::ops::Range;

use aero_core::Block;

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Align {
    Left,
    Center,
    Right,
}

struct Row {
    separator: bool,
    /// 这一行在表格源码里的区间（不含换行）
    line: Range<usize>,
    cells: Vec<Range<usize>>,
    align: Vec<Align>,
    /// 分隔行每一列的 `-` 个数
    dashes: Vec<usize>,
    /// 分隔行每一列有没有写 `:`（左对齐的 `:---` 与不写不同，写回时保留）
    colons: Vec<(bool, bool)>,
}

pub(super) struct Grid {
    rows: Vec<Row>,
}

impl Grid {
    /// 数据行（含表头）的单元格区间，相对表格源码。
    pub fn data_rows(&self) -> impl Iterator<Item = &Vec<Range<usize>>> {
        self.rows.iter().filter(|row| !row.separator).map(|row| &row.cells)
    }

    fn separator(&self) -> Option<&Row> {
        self.rows.iter().find(|row| row.separator)
    }

    fn columns(&self) -> usize {
        self.data_rows()
            .map(Vec::len)
            .max()
            .unwrap_or(0)
            .max(self.separator().map_or(0, |row| row.dashes.len()))
    }

    pub fn align(&self) -> Vec<Align> {
        self.separator().map(|row| row.align.clone()).unwrap_or_default()
    }

    /// 分隔行里各列 `-` 的个数不一样时，就是用户定过的列宽比例。
    fn explicit_widths(&self) -> Option<Vec<f32>> {
        let dashes = &self.separator()?.dashes;
        if dashes.len() < 2 || dashes.iter().all(|d| *d == dashes[0]) {
            return None;
        }
        let total: usize = dashes.iter().sum();
        Some(dashes.iter().map(|d| *d as f32 / total as f32).collect())
    }
}

/// 把一张表拆成单元格在源码里的区间（不含两侧空格和 `|`）。分隔行没有可编辑的格子。
pub(super) fn parse(src: &str) -> Option<Grid> {
    if !src.contains('|') {
        return None;
    }
    let mut rows = Vec::new();
    let mut offset = 0;
    for line in src.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        if body.contains('|') {
            let separator = is_separator(body);
            let cells = if separator { Vec::new() } else { split_cells(body, offset) };
            let raw = if separator { split_raw(body) } else { Vec::new() };
            let align = raw.iter().map(|cell| align_of(cell.trim())).collect();
            let dashes = raw.iter().map(|cell| cell.bytes().filter(|b| *b == b'-').count()).collect();
            let colons =
                raw.iter().map(|cell| (cell.trim().starts_with(':'), cell.trim().ends_with(':'))).collect();
            rows.push(Row { separator, line: offset..offset + body.len(), cells, align, dashes, colons });
        }
        offset += line.len();
    }
    let data = rows.iter().filter(|row| !row.separator).count();
    if data == 0 {
        return None;
    }
    Some(Grid { rows })
}

fn is_separator(line: &str) -> bool {
    let cells = split_raw(line);
    !cells.is_empty() && cells.iter().all(|cell| is_align_cell(cell.trim()))
}

fn is_align_cell(cell: &str) -> bool {
    let cell = cell.trim_matches(':');
    !cell.is_empty() && cell.bytes().all(|b| b == b'-')
}

fn align_of(cell: &str) -> Align {
    match (cell.starts_with(':'), cell.ends_with(':')) {
        (true, true) => Align::Center,
        (false, true) => Align::Right,
        _ => Align::Left,
    }
}

fn split_raw(line: &str) -> Vec<&str> {
    let line = line.trim().strip_prefix('|').unwrap_or(line.trim());
    let line = line.strip_suffix('|').unwrap_or(line);
    let mut parts = Vec::new();
    let mut start = 0;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if bytes[i] == b'|' {
            parts.push(&line[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    parts.push(&line[start..]);
    parts
}

fn split_cells(line: &str, line_start: usize) -> Vec<Range<usize>> {
    let raw = line.trim_end();
    let lead = raw.len() - raw.trim_start().len();
    let body = raw.trim_start();
    let mut cells = Vec::new();
    let mut rel = if body.starts_with('|') { 1 } else { 0 };
    let bytes = body.as_bytes();
    while rel < bytes.len() {
        let cell_start = rel;
        while rel < bytes.len() {
            if bytes[rel] == b'\\' && rel + 1 < bytes.len() {
                rel += 2;
                continue;
            }
            if bytes[rel] == b'|' {
                break;
            }
            rel += 1;
        }
        let mut a = cell_start;
        let mut b = rel;
        while a < b && bytes[a] == b' ' {
            a += 1;
        }
        while b > a && bytes[b - 1] == b' ' {
            b -= 1;
        }
        cells.push(line_start + lead + a..line_start + lead + b);
        if rel < bytes.len() && bytes[rel] == b'|' {
            rel += 1;
        }
    }
    cells
}

/// 按比例写出新的分隔行，保留每列原来的冒号（对齐方式）。
fn separator_line(fractions: &[f32], colons: &[(bool, bool)]) -> String {
    let total = (fractions.len() * 12) as f32;
    let mut line = String::from("|");
    for (ix, fraction) in fractions.iter().enumerate() {
        let (left, right) = colons.get(ix).copied().unwrap_or((false, false));
        let dashes = ((fraction * total).round() as usize).max(3);
        line.push(' ');
        if left {
            line.push(':');
        }
        line.push_str(&"-".repeat(dashes));
        if right {
            line.push(':');
        }
        line.push_str(" |");
    }
    line
}

/// 列宽：`Explicit` 是占表格宽度的比例；`Auto` 是每列的 (基础宽度, 多余空间里分到的份额)。
enum Widths {
    Explicit(Vec<f32>),
    Auto(Vec<(f32, f32)>),
}

/// 估算一段文字的宽度（em）。只用来分配列宽，不需要精确：全角字 1 个 em，其余约半个。
fn text_em(text: &str) -> f32 {
    text.chars()
        .map(|c| if c.is_ascii() { if c.is_ascii_uppercase() { 0.62 } else { 0.55 } } else { 1.0 })
        .sum()
}

/// 正在拖动的列边界。
#[derive(Clone)]
pub(super) struct TableResize {
    pub id: BlockId,
    pub col: usize,
}

pub(super) struct ResizeState {
    id: BlockId,
    col: usize,
    start_x: Pixels,
    total: Pixels,
    start: Vec<f32>,
    pub current: Vec<f32>,
}

struct Blank;

impl Render for Blank {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 最窄的列宽（像素）。
const MIN_COLUMN: f32 = 48.;

impl Editor {
    fn table_widths(&self, id: BlockId, grid: &Grid, src: &str) -> Widths {
        let columns = grid.columns();
        if let Some(drag) = self.table_resize.as_ref().filter(|drag| drag.id == id) {
            return Widths::Explicit(drag.current.clone());
        }
        if let Some((_, frozen)) =
            self.table_frozen.as_ref().filter(|(frozen, w)| *frozen == id && w.len() == columns)
        {
            return Widths::Explicit(frozen.clone());
        }
        if let Some(explicit) = grid.explicit_widths().filter(|w| w.len() == columns) {
            return Widths::Explicit(explicit);
        }
        let mut natural = vec![0f32; columns];
        for (row, cells) in grid.data_rows().enumerate() {
            for (col, cell) in cells.iter().enumerate().take(columns) {
                let text = src.get(cell.clone()).unwrap_or("");
                let visible = aero_core::markup::analyze(text).visible_text(text);
                let em = text_em(&visible) * if row == 0 { 1.06 } else { 1.0 };
                natural[col] = natural[col].max(em);
            }
        }
        // 短内容先拿到不折行的宽度（最多 10 个字），剩下的空间按各列还想要多宽来分。
        // 每格两侧内边距共 16px，文字控件右边还给光标留了 10px，再多留一点余量。
        Widths::Auto(
            natural
                .into_iter()
                .map(|em| {
                    let em = em + 2.0;
                    let basis = em.min(11.) * BODY_FONT_SIZE;
                    (basis.max(MIN_COLUMN), (em * BODY_FONT_SIZE - basis).max(0.) + 1.)
                })
                .collect(),
        )
    }

    pub(super) fn render_table_block(&self, block: &Block, cx: &mut Context<Self>) -> AnyElement {
        let range = self.doc.content_range(block);
        let src = self.doc.slice(range.clone()).to_string();
        let Some(grid) = parse(&src) else {
            return self.render_textarea(Some(&block.kind), cx);
        };
        let theme = cx.theme().clone();
        let id = block.id;
        let columns = grid.columns();
        let align = grid.align();
        let widths = self.table_widths(id, &grid, &src);
        let rows: Vec<&Vec<Range<usize>>> = grid.data_rows().collect();
        let row_count = rows.len();
        let layout = self.table_layout.clone();
        let editor = cx.entity();
        let resizing = self.table_resize.as_ref().filter(|drag| drag.id == id).map(|drag| drag.col);
        let rows = rows.into_iter().enumerate().map(|(row_ix, cells)| {
            let head = row_ix == 0;
            let row = h_flex()
                .w_full()
                .items_stretch()
                .border_color(theme.border)
                .when(row_ix + 1 < row_count, |el| el.border_b_1())
                .when(head, |el| {
                    el.bg(theme.table_head)
                        .text_color(theme.table_head_foreground)
                        .font_weight(FontWeight::SEMIBOLD)
                })
                .children((0..columns).map(|col| {
                    let key = presenter::Key { id, part: presenter::Part::Cell(row_ix, col) };
                    let align = align.get(col).copied().unwrap_or(Align::Left);
                    let text = if cells.get(col).is_some() { self.render_presenter(key, cx) } else { None };
                    let natural = self.presenters.get(&key).and_then(|p| p.natural_width);
                    let text = text.map(|text| match (align, natural) {
                        (Align::Left, _) | (_, None) => text,
                        (align, Some(width)) => h_flex()
                            .w_full()
                            .when(align == Align::Center, |el| el.justify_center())
                            .when(align == Align::Right, |el| el.justify_end())
                            .child(div().w(width).max_w_full().child(text))
                            .into_any_element(),
                    });
                    let cell = div()
                        .id(SharedString::from(format!("table-cell-{}-{row_ix}-{col}", id.0)))
                        .debug_selector(move || format!("table-cell-{row_ix}-{col}"))
                        .relative()
                        .min_w(px(MIN_COLUMN))
                        .px_2()
                        .py_1()
                        .cursor_text()
                        .map(|el| match &widths {
                            // 比例是整格（含内边距和边框）占一行的份额，所以用百分比做基础宽度、不再伸缩
                            Widths::Explicit(fractions) => el
                                .flex_basis(relative(
                                    fractions.get(col).copied().unwrap_or(1. / columns as f32),
                                ))
                                .flex_grow(0.)
                                .flex_shrink(0.),
                            Widths::Auto(auto) => {
                                let (basis, grow) = auto.get(col).copied().unwrap_or((MIN_COLUMN, 1.));
                                el.flex_basis(px(basis)).flex_grow(grow).flex_shrink(1.)
                            }
                        })
                        .when(col + 1 < columns, |el| el.border_r_1().border_color(theme.border))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                this.focus_presenter_near(key, event.position, window, cx);
                            }),
                        )
                        .children(text);
                    let handle = (col + 1 < columns).then(|| {
                        let editor = editor.clone();
                        let active = resizing == Some(col);
                        div()
                            .id(SharedString::from(format!("table-resize-{}-{row_ix}-{col}", id.0)))
                            .debug_selector(move || format!("table-resize-{row_ix}-{col}"))
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .right(px(-4.))
                            .w(px(7.))
                            .flex()
                            .justify_center()
                            .cursor_col_resize()
                            .group("table-resize")
                            .child(
                                div()
                                    .w(px(2.))
                                    .h_full()
                                    .when(active, |el| el.bg(theme.ring))
                                    .group_hover("table-resize", |el| el.bg(theme.ring)),
                            )
                            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                cx.stop_propagation();
                                window.prevent_default();
                            })
                            .on_drag(TableResize { id, col }, move |drag, _, window, cx| {
                                let x = window.mouse_position().x;
                                editor.update(cx, |this, cx| this.start_table_resize(drag, x, cx));
                                cx.new(|_| Blank)
                            })
                    });
                    cell.children(handle)
                }));
            // 表头这一行每帧记下各列的矩形：拖列宽、冻结列宽都要按实际像素换算
            if head {
                let layout = layout.clone();
                row.on_children_prepainted(move |bounds, _, _| {
                    layout.borrow_mut().insert(id, bounds);
                })
                .into_any_element()
            } else {
                row.into_any_element()
            }
        });
        // 列多到放不下每列的最小宽度时横向滚动；纵向滚动照常交给正文
        div()
            .id(SharedString::from(format!("table-{}", id.0)))
            .debug_selector(|| "table".to_owned())
            .w_full()
            .border_1()
            .border_color(theme.border)
            .rounded(px(6.))
            .overflow_x_scroll()
            .child(v_flex().w_full().min_w(px(columns as f32 * MIN_COLUMN)).children(rows))
            .into_any_element()
    }

    /// 上一帧各列的宽度占比。
    fn table_fractions(&self, id: BlockId) -> Option<(Vec<f32>, Pixels, Vec<Bounds<Pixels>>)> {
        let layout = self.table_layout.borrow();
        let bounds = layout.get(&id)?.clone();
        let total: Pixels = bounds.iter().map(|b| b.size.width).sum();
        if bounds.is_empty() || total <= px(0.) {
            return None;
        }
        Some((bounds.iter().map(|b| b.size.width / total).collect(), total, bounds))
    }

    fn start_table_resize(&mut self, drag: &TableResize, x: Pixels, cx: &mut Context<Self>) {
        let Some((fractions, total, _)) = self.table_fractions(drag.id) else { return };
        self.table_resize = Some(ResizeState {
            id: drag.id,
            col: drag.col,
            start_x: x,
            total,
            start: fractions.clone(),
            current: fractions,
        });
        cx.notify();
    }

    pub(super) fn track_table_resize(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let Some(drag) = self.table_resize.as_mut() else { return };
        let (col, total) = (drag.col, drag.total);
        let Some((&left, &right)) = drag.start.get(col).zip(drag.start.get(col + 1)) else { return };
        let min = (px(MIN_COLUMN) / total).min((left + right) / 2.);
        let delta = ((x - drag.start_x) / total).clamp(min - left, right - min);
        drag.current = drag.start.clone();
        drag.current[col] = left + delta;
        drag.current[col + 1] = right - delta;
        cx.notify();
    }

    /// 松手：把列宽比例写进分隔行。一次撤销组。
    pub(super) fn finish_table_resize(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.table_resize.take() else { return };
        cx.notify();
        if drag.current == drag.start {
            return;
        }
        let Some(block) = self.doc.blocks().find(drag.id) else { return };
        let range = self.doc.content_range(block);
        let src = self.doc.slice(range.clone()).to_string();
        let Some(grid) = parse(&src) else { return };
        let Some(separator) = grid.separator() else { return };
        let line = separator_line(&drag.current, &separator.colons);
        let at = range.start + separator.line.start..range.start + separator.line.end;
        if let Err(err) = self.doc.edit(Edit::replace(at.clone(), line.clone())) {
            log::error!("edit rejected: {err}");
            return;
        }
        let delta = line.len() as isize - at.len() as isize;
        if let Some(f) = self.focused.as_mut()
            && f.range.start >= at.end
        {
            f.range = (f.range.start as isize + delta) as usize..(f.range.end as isize + delta) as usize;
        }
        if let Some((frozen, widths)) = self.table_frozen.as_mut()
            && *frozen == drag.id
        {
            *widths = drag.current.clone();
        }
        self.last_edit_at = None;
        self.after_doc_change(cx);
    }

    /// 开始编辑某张表：按当前实际列宽冻结，打字时各列不跟着内容伸缩。
    pub(super) fn freeze_table(&mut self, id: BlockId) {
        if self.table_frozen.as_ref().is_some_and(|(frozen, _)| *frozen == id) {
            return;
        }
        self.table_frozen = self.table_fractions(id).map(|(fractions, _, _)| (id, fractions));
    }

    /// ⏎ / Tab 在单元格里：移到下一行 / 下一格。越过表尾就离开表格。
    pub(super) fn move_table_cell(
        &mut self,
        id: BlockId,
        row: usize,
        col: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = presenter::Key { id, part: presenter::Part::Cell(row, col) };
        if let Some(p) = self.presenters.get(&key) {
            let (range, len) = (p.range.clone(), p.range.len());
            self.focus_range(range, Some(id), false, len, window, cx);
            return;
        }
        let Some(ix) = self.unit_index(id) else { return };
        if ix + 1 < self.units.len() {
            self.focus_unit(ix + 1, 0, window, cx);
        } else {
            self.append_paragraph(window, cx);
        }
    }

    /// 键盘进入表格（↑↓、大纲定位）：落在包含 `cursor`（表格源码里的偏移）的格子上，没有就是最近的那格。
    pub(super) fn focus_table_at(
        &mut self,
        id: BlockId,
        cursor: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(block) = self.doc.blocks().find(id) else { return false };
        let range = self.doc.content_range(block);
        let src = self.doc.slice(range.clone()).to_string();
        let Some(grid) = parse(&src) else { return false };
        let cells: Vec<Range<usize>> = grid.data_rows().flat_map(|row| row.iter().cloned()).collect();
        let Some(cell) = cells.iter().min_by_key(|cell| {
            if cell.contains(&cursor) || cell.end == cursor { 0 } else { cell.start.abs_diff(cursor) + 1 }
        }) else {
            return false;
        };
        let local = cursor.clamp(cell.start, cell.end) - cell.start;
        let abs = range.start + cell.start..range.start + cell.end;
        self.focus_range(abs, Some(id), false, local, window, cx);
        true
    }

    /// 单元格里不能有换行和未转义的 `|`：那会拆坏整张表。输入时就地转成空格和 `\|`。
    pub(super) fn sanitize_cell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.input_composing(window, cx) {
            return;
        }
        let Some(f) = self.focused.as_ref() else { return };
        if !self
            .presenter_key_of(&f.textarea)
            .is_some_and(|key| matches!(key.part, presenter::Part::Cell(..)))
        {
            return;
        }
        let ta = f.textarea.clone();
        let text = ta.read(cx).value().to_string();
        let caret = ta.read(cx).selected_range().end;
        let mut fixed = String::with_capacity(text.len());
        let mut new_caret = caret;
        let mut escaped = false;
        for (i, c) in text.char_indices() {
            match c {
                '\n' | '\r' => {
                    fixed.push(' ');
                }
                '|' if !escaped => {
                    fixed.push_str("\\|");
                    if i < caret {
                        new_caret += 1;
                    }
                }
                _ => fixed.push(c),
            }
            escaped = c == '\\' && !escaped;
        }
        if fixed != text {
            ta.update(cx, |ta, cx| {
                ta.set_selected_range(0..text.len(), cx);
                ta.replace(fixed, window, cx);
                ta.set_selected_range(new_caret..new_caret, cx);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, separator_line};

    #[test]
    fn cells_keep_their_source_ranges() {
        let src = "| 甲 | 乙 |\n| --- | ---: |\n| **粗** | 2 |\n";
        let grid = parse(src).unwrap();
        let data: Vec<Vec<&str>> =
            grid.data_rows().map(|row| row.iter().map(|cell| &src[cell.clone()]).collect()).collect();
        assert_eq!(data, vec![vec!["甲", "乙"], vec!["**粗**", "2"]]);
        assert!(grid.explicit_widths().is_none(), "等长的分隔行按内容排宽");
    }

    #[test]
    fn column_widths_round_trip_through_the_separator_row() {
        let line = separator_line(&[0.2, 0.5, 0.3], &[(true, false), (false, false), (true, true)]);
        assert_eq!(line, "| :------- | ------------------ | :-----------: |");
        let src = format!("| a | b | c |\n{line}\n| 1 | 2 | 3 |\n");
        let grid = parse(&src).unwrap();
        let widths = grid.explicit_widths().unwrap();
        assert!((widths[0] - 7. / 36.).abs() < 1e-4 && (widths[1] - 18. / 36.).abs() < 1e-4, "{widths:?}");
        assert_eq!(grid.align(), vec![super::Align::Left, super::Align::Left, super::Align::Center]);
    }
}
