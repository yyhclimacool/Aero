//! Source-range attribute edits shared by block commands.

use std::ops::Range;

use crate::{Attrs, Block, BlockId, BlockKind, Document, Edit};

pub fn render(attrs: &Attrs) -> String {
    let mut parts = Vec::new();
    if let Some(id) = &attrs.id {
        parts.push(format!("#{id}"));
    }
    parts.extend(attrs.classes.iter().map(|class| format!(".{class}")));
    parts.extend(attrs.pairs.iter().map(|(key, value)| {
        if value.is_empty()
            || value.contains(|c: char| c.is_whitespace() || matches!(c, '"' | '\\' | '{' | '}'))
            || value.starts_with('#')
        {
            format!("{key}=\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
        } else {
            format!("{key}={value}")
        }
    }));
    parts.join(" ")
}

pub fn set(attrs: &mut Attrs, key: &str, value: Option<&str>) {
    attrs.pairs.retain(|(k, _)| k != key);
    if let Some(value) = value {
        attrs.pairs.push((key.into(), value.into()));
    }
}

fn siblings(blocks: &[Block], id: BlockId) -> Option<(&Block, Option<&Block>)> {
    for (i, block) in blocks.iter().enumerate() {
        if block.id == id {
            return Some((block, blocks.get(i + 1)));
        }
        if let Some(found) = siblings(&block.children, id) {
            return Some(found);
        }
    }
    None
}

/// Includes the metadata line owned by a non-leaf block.
pub fn owned_range(doc: &Document, id: BlockId) -> Option<Range<usize>> {
    let (block, next) = siblings(&doc.blocks().roots, id)?;
    let mut range = doc.range_of(block);
    if let Some(marker) = next.filter(|b| b.kind == BlockKind::Marker) {
        range.end = doc.range_of(marker).end;
    }
    Some(range)
}

fn marker(attrs: &Attrs, id: Option<&str>) -> String {
    let mut text = if attrs.is_empty() { String::new() } else { format!("{{{}}}", render(attrs)) };
    if let Some(id) = id {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push('^');
        text.push_str(id);
    }
    text
}

/// Replaces only the owning suffix/metadata, keeping body bytes unchanged.
pub fn edit(
    doc: &Document,
    id: BlockId,
    attrs: &Attrs,
    persistent: Option<&str>,
) -> Result<Option<Edit>, String> {
    let (block, next) = siblings(&doc.blocks().roots, id).ok_or("目标块已不存在")?;
    let source = doc.to_string();
    let range = doc.range_of(block);
    let src = &source[range.clone()];
    let metadata = marker(attrs, persistent);
    if matches!(block.kind, BlockKind::Directive { .. }) {
        let first_end = src.find('\n').unwrap_or(src.len());
        let opening = crate::parse::markers::strip_trailing(&src[..first_end]);
        let mut appearance = attrs.clone();
        set(&mut appearance, "aero-aliases", None);
        let mut identity = Attrs::default();
        for (_, aliases) in attrs.pairs.iter().filter(|(key, _)| key == "aero-aliases") {
            identity.pairs.push(("aero-aliases".into(), aliases.clone()));
        }
        let mut body = src.to_owned();
        let style =
            if appearance.is_empty() { String::new() } else { format!(" {{{}}}", render(&appearance)) };
        body.replace_range(opening.content_len..first_end, &style);
        let identity = marker(&identity, persistent);
        if !identity.is_empty() {
            let prefix = crate::containers::continuation(doc, id);
            body.push_str(&format!("\n{prefix}\n{prefix}{identity}"));
        }
        let owned = owned_range(doc, id).unwrap();
        return Ok((source[owned.clone()] != body).then(|| Edit::replace(owned, body)));
    }
    if matches!(block.kind, BlockKind::CodeFence { .. } | BlockKind::Directive { .. }) {
        let first_end = src.find('\n').unwrap_or(src.len());
        let opening = crate::parse::markers::strip_trailing(&src[..first_end]);
        if opening.attrs.is_some() {
            let owned = owned_range(doc, id).unwrap();
            let mut body = src.to_owned();
            body.replace_range(opening.content_len..first_end, "");
            let prefix = crate::containers::continuation(doc, id);
            if !metadata.is_empty() {
                body.push_str(&format!("\n{prefix}\n{prefix}{metadata}"));
            }
            return Ok(Some(Edit::replace(owned, body)));
        }
    }
    let edit = if matches!(block.kind, BlockKind::Paragraph | BlockKind::Heading { .. }) {
        let trailing = crate::parse::markers::strip_trailing(src);
        let end = src.trim_end().len();
        let replacement = if metadata.is_empty() { String::new() } else { format!(" {metadata}") };
        Edit::replace(range.start + trailing.content_len..range.start + end, replacement)
    } else if matches!(block.kind, BlockKind::ListItem { .. }) {
        return item_edit(doc, block, attrs, persistent);
    } else if let Some(next) = next.filter(|b| b.kind == BlockKind::Marker) {
        Edit::replace(doc.range_of(next), metadata)
    } else {
        if metadata.is_empty() {
            return Ok(None);
        }
        let line_start = source[..range.start].rfind('\n').map_or(0, |i| i + 1);
        let prefix: String = source[line_start..range.start]
            .chars()
            .map(|c| if c == '>' || c == '\t' { c } else { ' ' })
            .collect();
        Edit::insert(range.end, format!("\n{prefix}\n{prefix}{metadata}"))
    };
    if source.get(edit.range.clone()) == Some(edit.text.as_str()) { Ok(None) } else { Ok(Some(edit)) }
}

fn item_edit(
    doc: &Document,
    block: &Block,
    attrs: &Attrs,
    persistent: Option<&str>,
) -> Result<Option<Edit>, String> {
    let range = doc.range_of(block);
    let source = doc.to_string();
    let src = &source[range.clone()];
    let line_end = src.find('\n').unwrap_or(src.len());
    let mut attrs = attrs.clone();
    set(&mut attrs, "aero-owner", Some("item"));
    let metadata = marker(&attrs, persistent);
    if let Some(at) = src[..line_end].find('{')
        && crate::parse::markers::parse_marker_line(&src[at..line_end]).is_some()
        && src[at..line_end].contains("aero-owner=item")
    {
        let edit = Edit::replace(range.start + at..range.start + line_end, metadata);
        return Ok((source[edit.range.clone()] != edit.text).then_some(edit));
    }
    let body = block
        .children
        .first()
        .map(|b| doc.range_of(b).start)
        .filter(|start| *start <= range.start + line_end)
        .ok_or("此列表项暂时无法安全写入元数据")?;
    let prefix = &source[range.start..body];
    let line_start = source[..range.start].rfind('\n').map_or(0, |i| i + 1);
    let marker_width = prefix.find('[').unwrap_or(prefix.len());
    let indent = " ".repeat(range.start - line_start + marker_width);
    let text = format!("{prefix}{metadata}\n\n{indent}{}", &src[prefix.len()..]);
    Ok(Some(Edit::replace(range, text)))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Alignment {
    #[default]
    Left,
    Center,
    Right,
    Justify,
}

impl Alignment {
    pub fn of(attrs: &Attrs) -> Self {
        match attrs.pairs.iter().rev().find(|(k, _)| k == "align").map(|(_, v)| v.as_str()) {
            Some("center") => Self::Center,
            Some("right") => Self::Right,
            Some("justify") => Self::Justify,
            _ => Self::Left,
        }
    }
}

pub fn indent(attrs: &Attrs) -> u8 {
    attrs
        .pairs
        .iter()
        .rev()
        .find(|(key, _)| key == "indent")
        .and_then(|(_, value)| value.parse::<i64>().ok())
        .unwrap_or(0)
        .clamp(0, 8) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directive_appearance_stays_on_opening_and_identity_after_closing() {
        let source = ":::highlight {.bg-yellow custom=keep}\nchild **body** ^child\n:::\n";
        let mut doc = Document::from_text(source);
        let block = &doc.blocks().roots[0];
        let id = block.id;
        let mut attrs = block.attrs.clone();
        attrs.classes = vec!["bg-blue".into()];
        set(&mut attrs, "aero-aliases", Some("old"));
        let persistent = crate::identity::new_block_id();
        doc.edit(edit(&doc, id, &attrs, Some(&persistent)).unwrap().unwrap()).unwrap();
        doc.reparse();
        let text = doc.to_string();
        assert!(
            text.starts_with(":::highlight {.bg-blue custom=keep}\nchild **body** ^child\n:::"),
            "{text}"
        );
        assert!(text.contains(&format!("{{aero-aliases=old}} ^{persistent}")), "{text}");
        assert_eq!(doc.blocks().roots[0].persisted_id.as_deref(), Some(persistent.as_str()));
        doc.undo();
        assert_eq!(doc.to_string(), source);
    }

    #[test]
    fn item_metadata_belongs_to_item_and_leaves_first_paragraph_editable() {
        let mut doc = Document::from_text("- [ ] first {.red}\n\n  second\n\n  - nested\n");
        let item = doc.blocks().roots[0].children[0].id;
        let mut attrs = Attrs::default();
        attrs.classes.push("bg-blue".into());
        let persistent = crate::identity::new_block_id();
        let plan = edit(&doc, item, &attrs, Some(&persistent)).unwrap().unwrap();
        doc.edit(plan).unwrap();
        doc.reparse();
        let item = &doc.blocks().roots[0].children[0];
        assert_eq!(item.persisted_id.as_deref(), Some(persistent.as_str()));
        assert_eq!(item.attrs.classes, ["bg-blue"]);
        assert_eq!(item.children.len(), 3);
        assert_eq!(item.children[0].kind, BlockKind::Paragraph);
        assert_eq!(item.children[0].attrs.classes, ["red"]);
        assert!(doc.to_string().contains("  - nested"));
        let mut changed = item.attrs.clone();
        set(&mut changed, "align", Some("right"));
        let update = edit(&doc, item.id, &changed, Some(&persistent)).unwrap().unwrap();
        doc.edit(update).unwrap();
        doc.reparse();
        assert_eq!(Alignment::of(&doc.blocks().roots[0].children[0].attrs), Alignment::Right);
    }
}
