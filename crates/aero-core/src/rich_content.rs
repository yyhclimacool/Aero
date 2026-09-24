#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PdfEmbed {
    pub target: String,
    pub page: Result<u32, String>,
    pub width: Option<u32>,
}

pub fn mermaid_source(source: &str) -> Option<String> {
    let arena = comrak::Arena::new();
    let root = comrak::parse_document(&arena, source, &crate::parse::comrak_options());
    let node = root.first_child()?;
    if node.next_sibling().is_some() {
        return None;
    }
    let data = node.data();
    let comrak::nodes::NodeValue::CodeBlock(code) = &data.value else { return None };
    if source.lines().enumerate().any(|(index, line)| {
        (index + 1 < data.sourcepos.start.line || index + 1 > data.sourcepos.end.line)
            && !line.trim().is_empty()
    }) {
        return None;
    }
    (code.fenced && code.info.split_whitespace().next() == Some("mermaid")).then(|| code.literal.clone())
}

pub fn pdf_embed(source: &str) -> Option<PdfEmbed> {
    let embeds = crate::links::scan_embeds(source);
    let [embed] = embeds.as_slice() else { return None };
    let start = source.len() - source.trim_start().len();
    if embed.range != (start..source.trim_end().len())
        || crate::attachments::Kind::for_path(&embed.target) != Some(crate::attachments::Kind::Pdf)
    {
        return None;
    }
    let source = source.trim();
    let inner = &source[3..source.len() - 2];
    let (head, _) = crate::links::split_alias(inner);
    let (_, fragment) = crate::links::split_target(head);
    let page = match fragment {
        None => Ok(1),
        Some(fragment) => fragment
            .strip_prefix("page=")
            .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|digits| digits.parse::<u32>().ok())
            .filter(|page| *page > 0)
            .ok_or_else(|| "PDF 页码必须是正整数（#page=1）".to_owned()),
    };
    Some(PdfEmbed {
        target: embed.target.clone(),
        page,
        width: embed.parameter.as_deref().and_then(|width| width.trim().parse().ok()),
    })
}

pub fn pdf_source_with_page(source: &str, page: u32) -> Option<String> {
    if page == 0 {
        return None;
    }
    pdf_embed(source)?.page.ok()?;
    let trimmed = source.trim();
    let base = source.len() - source.trim_start().len() + 3;
    let inner = &trimmed[3..trimmed.len() - 2];
    let (head, _) = crate::links::split_alias(inner);
    let (target, fragment) = crate::links::split_unescaped(head, '#');
    let mut next = source.to_owned();
    match fragment {
        Some(fragment) => {
            let start = base + head.len() - fragment.trim_start().len();
            let end = start + fragment.trim().len();
            next.replace_range(start..end, &format!("page={page}"));
        }
        None => next.insert_str(base + target.trim_end().len(), &format!("#page={page}")),
    }
    Some(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mermaid_reads_fenced_code_without_changing_the_source() {
        for source in [
            "```mermaid\nflowchart LR\n  A[开始] --> B[完成]\n```\n",
            "~~~mermaid\r\nflowchart LR\r\n  A[开始] --> B[完成]\r\n~~~\r\n",
            "```mermaid\nflowchart LR\n  A[开始] --> B[完成]\n",
        ] {
            let expected = if source.contains("\r\n") {
                "flowchart LR\r\n  A[开始] --> B[完成]\r\n"
            } else {
                "flowchart LR\n  A[开始] --> B[完成]\n"
            };
            assert_eq!(mermaid_source(source).as_deref(), Some(expected));
        }
        assert_eq!(mermaid_source("```mermaid\n```"), Some(String::new()));
    }

    #[test]
    fn mermaid_does_not_capture_other_blocks_or_languages() {
        for source in [
            "```rust\nlet x = 1;\n```",
            "`mermaid`",
            "    mermaid",
            "```mermaid\nA\n```\n\ntext",
            "> ```mermaid\n> A\n> ```",
        ] {
            assert!(mermaid_source(source).is_none(), "{source}");
        }
    }

    #[test]
    fn pdf_recognizes_only_standalone_embeds() {
        assert_eq!(
            pdf_embed(" \n![[论文.PDF]]\r\n"),
            Some(PdfEmbed { target: "论文.PDF".into(), page: Ok(1), width: None })
        );
        assert_eq!(
            pdf_embed("![[目录/论文.pdf#page=3|400]]"),
            Some(PdfEmbed { target: "目录/论文.pdf".into(), page: Ok(3), width: Some(400) })
        );
        for source in [
            "文字 ![[a.pdf]]",
            "![[a.pdf]] ![[b.pdf]]",
            "`![[a.pdf]]`",
            "\\![[a.pdf]]",
            "![[a.png]]",
            "![[a.pdf.md]]",
            "[[a.pdf]]",
        ] {
            assert!(pdf_embed(source).is_none(), "{source}");
        }
    }

    #[test]
    fn rich_content_does_not_capture_indented_code_or_reference_definitions() {
        for source in ["    ![[a.pdf]]", "\t![[a.pdf]]"] {
            assert!(pdf_embed(source).is_none(), "{source}");
            assert!(pdf_source_with_page(source, 2).is_none());
        }
        for source in ["[x]: /x\n\n```mermaid\nA\n```", "```mermaid\nA\n```\n\n[x]: /x"] {
            assert!(mermaid_source(source).is_none(), "{source}");
        }
    }

    #[test]
    fn pdf_invalid_pages_remain_pdf_blocks_with_an_error() {
        for fragment in ["page=0", "page=-1", "page=+2", "page=no", "page=4294967296", "chapter=1", ""] {
            let source = format!("![[a.pdf#{fragment}]]");
            assert!(pdf_embed(&source).unwrap().page.is_err(), "{source}");
        }
    }

    #[test]
    fn pdf_page_edit_preserves_every_other_byte() {
        let source = "  ![[  文档\\#甲.pdf #page=2  | 400 ]]\r\n";
        assert_eq!(
            pdf_source_with_page(source, 3).as_deref(),
            Some("  ![[  文档\\#甲.pdf #page=3  | 400 ]]\r\n")
        );
        assert_eq!(
            pdf_source_with_page("![[  文档\\#甲.pdf  |400]]", 2).as_deref(),
            Some("![[  文档\\#甲.pdf#page=2  |400]]")
        );
        assert_eq!(
            pdf_source_with_page("![[a.pdf#page=3\\|400]]", 1).as_deref(),
            Some("![[a.pdf#page=1\\|400]]")
        );
    }

    #[test]
    fn pdf_page_edits_roundtrip_document_metadata_and_undo() {
        let source = "\u{feff}# 标题\r\n\r\n![[论文.pdf#page=1|400]] {.red} ^paper\r\n\r\n结尾\r\n";
        let mut doc = crate::Document::from_bytes(source.as_bytes()).unwrap();
        let block = doc
            .blocks()
            .iter()
            .map(|(_, block)| block)
            .find(|block| block.persisted_id.as_deref() == Some("paper"))
            .unwrap();
        let id = block.id;
        let range = doc.content_range(block);
        let before = doc.slice(range.clone()).to_string();
        let next = pdf_source_with_page(&before, 2).unwrap();
        doc.edit(crate::Edit::replace(range, next)).unwrap();
        assert_eq!(doc.to_bytes(), source.replace("page=1", "page=2").as_bytes());
        assert!(
            doc.blocks()
                .iter()
                .any(|(_, block)| block.id == id && block.persisted_id.as_deref() == Some("paper"))
        );
        assert!(doc.undo().is_some());
        assert_eq!(doc.to_bytes(), source.as_bytes());
        assert!(doc.redo().is_some());
        assert_eq!(doc.to_bytes(), source.replace("page=1", "page=2").as_bytes());
    }

    #[test]
    fn pdf_page_edit_refuses_unrelated_or_invalid_source() {
        for source in ["![[a.pdf#chapter=2]]", "paragraph", "![[a.png]]", "![[a.pdf#page=0]]"] {
            assert!(pdf_source_with_page(source, 2).is_none(), "{source}");
        }
        assert!(pdf_source_with_page("![[a.pdf]]", 0).is_none());
        assert_eq!(pdf_source_with_page("![[a.pdf#page=3]]", 3).as_deref(), Some("![[a.pdf#page=3]]"));
    }
}
