//! 标题补全、大纲与片段跳转共用的显示文本。

use comrak::nodes::NodeValue;
use comrak::{Arena, parse_document};

use crate::{BlockId, BlockKind, Document};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heading {
    pub id: BlockId,
    pub level: u8,
    pub text: String,
}

pub fn list(doc: &Document) -> Vec<Heading> {
    let source = doc.to_string();
    let mut line_starts = vec![0];
    line_starts.extend(source.match_indices('\n').map(|(i, _)| i + 1));
    let blocks: std::collections::HashMap<_, _> = doc
        .blocks()
        .iter()
        .filter_map(|(_, b)| match b.kind {
            BlockKind::Heading { level } => Some((doc.content_range(b).start, (b.id, level))),
            _ => None,
        })
        .collect();
    let arena = Arena::new();
    // 用整篇的解析上下文保留引用式链接、容器中的 Setext 与实体解码。
    let root = parse_document(&arena, &source, &crate::parse::comrak_options());
    let mut headings = Vec::new();
    for node in root.descendants().filter(|n| matches!(n.data().value, NodeValue::Heading(_))) {
        let pos = node.data().sourcepos.start;
        let Some(start) =
            line_starts.get(pos.line.saturating_sub(1)).map(|line| line + pos.column.saturating_sub(1))
        else {
            continue;
        };
        let Some(&(id, level)) = blocks.get(&start) else { continue };
        let mut text = String::new();
        for child in node.descendants() {
            match &child.data().value {
                NodeValue::Text(value) => text.push_str(value),
                NodeValue::Code(code) => text.push_str(&code.literal),
                NodeValue::Math(math) => text.push_str(&math.literal),
                NodeValue::SoftBreak | NodeValue::LineBreak => text.push(' '),
                _ => {}
            }
        }
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        headings.push((start, Heading { id, level, text }));
    }
    headings.sort_by_key(|(start, _)| *start);
    headings.into_iter().map(|(_, heading)| heading).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_text_uses_rendered_inline_content_and_setext() {
        let doc = Document::from_text(
            "# **粗** _斜_ `码` [链接](https://example.com) [[笔记|别名]] [引用][ref] &amp; C# ###\n\n多行\n标题\n----\n\n## 属性 {.red}\n\n## 带标记\n^heading-id\n\n```md\n# 代码\n```\n\n[ref]: https://example.com\n",
        );
        let headings = list(&doc);
        assert_eq!(
            headings.iter().map(|h| (h.level, h.text.as_str())).collect::<Vec<_>>(),
            [(1, "粗 斜 码 链接 别名 引用 & C#"), (2, "多行 标题"), (2, "属性"), (2, "带标记")]
        );
    }

    #[test]
    fn heading_text_preserves_document_order_and_nested_ids() {
        let doc = Document::from_text(
            "> [!note]- 折叠\n> ## 子标题\n>\n> 多行\n> 标题\n> ----\n\n- ### 列表标题\n\n## 子标题\n",
        );
        let headings = list(&doc);
        assert_eq!(
            headings.iter().map(|h| h.text.as_str()).collect::<Vec<_>>(),
            ["子标题", "多行 标题", "列表标题", "子标题"]
        );
        assert_ne!(headings[0].id, headings[3].id);
    }
}
