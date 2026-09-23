//! 编辑单位（方案 7.1：光标永远位于某个 Leaf Block 内）。
//!
//! M1a 的取舍（方案 18 M1a）：引用 / callout / 脚注定义 / directive 整体作为一个单位、以源码编辑；
//! 列表项的正文段落是单位，列表标记由容器绘制；没有子块的空列表项（`- `）自身是单位，编辑区间为空。

use std::ops::Range;

use aero_core::{Block, BlockId, BlockKind, Document};

/// 一个单位所处的列表上下文，Enter / Backspace 需要它来续行或脱离列表。
#[derive(Clone, Debug)]
pub struct ListCtx {
    /// 列表项（含子块）的源区间
    pub item_range: Range<usize>,
    /// 在所属 List 里的序号（0 起）
    pub item_index: usize,
    /// 有序列表的起始编号
    pub ordered: Option<u32>,
    pub task: bool,
    /// 任务项是否已勾选；不是任务项时为 `None`
    pub checked: Option<bool>,
    /// 嵌套列表：所属的外层列表项
    pub parent: Option<Box<ListCtx>>,
}

#[derive(Clone, Debug)]
pub struct Unit {
    pub id: BlockId,
    pub kind: BlockKind,
    /// 控件里显示 / 编辑的区间（去掉尾部 `^id` / 属性标记）
    pub range: Range<usize>,
    pub list: Option<ListCtx>,
}

impl Unit {
    /// 这类块在控件里 Enter 应插入换行而不是拆块。
    pub fn enter_inserts_newline(&self) -> bool {
        matches!(
            self.kind,
            BlockKind::CodeFence { .. }
                | BlockKind::CodeBlock
                | BlockKind::MathBlock
                | BlockKind::Table
                | BlockKind::HtmlBlock
                | BlockKind::Raw
                | BlockKind::FrontMatter
                | BlockKind::BlockQuote
                | BlockKind::Callout { .. }
                | BlockKind::FootnoteDefinition { .. }
                | BlockKind::Directive { .. }
        )
    }

    /// 段落 / 标题：可以与相邻段落合并、可以在中间拆开。
    pub fn is_text(&self) -> bool {
        matches!(self.kind, BlockKind::Paragraph | BlockKind::Heading { .. })
    }
}

/// 是否把这个块当作一个整体单位（不再深入子块）。
pub fn is_atomic(kind: &BlockKind) -> bool {
    match kind {
        BlockKind::Marker | BlockKind::List { .. } | BlockKind::ListItem { .. } => false,
        k if !k.is_container() => true,
        BlockKind::BlockQuote
        | BlockKind::Callout { .. }
        | BlockKind::FootnoteDefinition { .. }
        | BlockKind::Directive { .. } => true,
        _ => false,
    }
}

/// 按文档顺序列出全部编辑单位。
pub fn units(doc: &Document) -> Vec<Unit> {
    let mut out = Vec::new();
    walk(doc, &doc.blocks().roots, None, &mut out);
    out
}

fn walk(doc: &Document, blocks: &[Block], list: Option<&ListCtx>, out: &mut Vec<Unit>) {
    for b in blocks {
        match &b.kind {
            BlockKind::Marker => {}
            BlockKind::List { ordered, start } => {
                let items: Vec<&Block> =
                    b.children.iter().filter(|c| matches!(c.kind, BlockKind::ListItem { .. })).collect();
                for (i, item) in items.iter().enumerate() {
                    let checked = match item.kind {
                        BlockKind::ListItem { task } => task,
                        _ => None,
                    };
                    let ctx = ListCtx {
                        item_range: doc.range_of(item),
                        item_index: i,
                        ordered: ordered.then_some(*start),
                        task: checked.is_some(),
                        checked,
                        parent: list.cloned().map(Box::new),
                    };
                    let content: Vec<&Block> =
                        item.children.iter().filter(|c| c.kind != BlockKind::Marker).collect();
                    if content.is_empty() {
                        let end = ctx.item_range.end;
                        out.push(Unit {
                            id: item.id,
                            kind: item.kind.clone(),
                            range: end..end,
                            list: Some(ctx),
                        });
                    } else {
                        let children: Vec<Block> = content.into_iter().cloned().collect();
                        walk(doc, &children, Some(&ctx), out);
                    }
                }
            }
            BlockKind::ListItem { .. } => {
                // 只会出现在 List 之下，上面已处理
                walk(doc, &b.children, list, out);
            }
            k if is_atomic(k) => {
                out.push(Unit {
                    id: b.id,
                    kind: b.kind.clone(),
                    range: doc.content_range(b),
                    list: list.cloned(),
                });
            }
            _ => walk(doc, &b.children, list, out),
        }
    }
}

/// 包含 `offset` 的单位（闭区间；多个候选取最深 / 最后一个开始的）。
pub fn unit_at(units: &[Unit], offset: usize) -> Option<usize> {
    units.iter().rposition(|u| u.range.start <= offset && offset <= u.range.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_and_atomic_blocks() {
        let doc = Document::from_text("# T\n\n- a\n- \n\n> q\n> r\n\n```rs\nx\n```\n");
        let us = units(&doc);
        let kinds: Vec<&str> = us.iter().map(|u| u.kind.tag()).collect();
        assert_eq!(kinds, ["heading", "paragraph", "list_item", "block_quote", "code_fence"]);
        // 空列表项：区间为空、有列表上下文
        let empty = &us[2];
        assert!(empty.range.is_empty());
        let ctx = empty.list.as_ref().unwrap();
        assert_eq!(ctx.item_index, 1);
        assert!(ctx.parent.is_none());
        // 段落 a 的正文区间不含 "- "
        assert_eq!(&doc.to_string()[us[1].range.clone()], "a");
        assert_eq!(us[1].range, 7..8);
        assert_eq!(unit_at(&us, 7), Some(1));
        assert_eq!(unit_at(&us, 8), Some(1));
        // 落在列表标记 `- ` 上不属于任何单位
        assert_eq!(unit_at(&us, 6), None);
    }
}
