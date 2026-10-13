//! File-owned identity. Edits replace scalar spans rather than printing YAML again.

use std::collections::BTreeMap;
use std::ops::Range;

use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_saphyr::Spanned;

use crate::{BlockKind, Document, Edit};

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Metadata {
    pub id: Option<String>,
    pub parent: Option<String>,
    pub order: Option<i64>,
}

#[derive(Default)]
pub struct Patch {
    pub id: Option<String>,
    /// None leaves the relationship alone; Some(None) removes the parent.
    pub parent: Option<Option<String>>,
    pub order: Option<i64>,
}

pub fn new_document_id() -> String {
    format!("d-{}", ulid::Ulid::generate().to_string().to_ascii_lowercase())
}

pub fn new_block_id() -> String {
    format!("b-{}", ulid::Ulid::generate().to_string().to_ascii_lowercase())
}

pub fn is_document_id(id: &str) -> bool {
    valid_id(id, "d-")
}

pub fn is_block_id(id: &str) -> bool {
    valid_id(id, "b-")
}

fn valid_id(id: &str, prefix: &str) -> bool {
    let Some(value) = id.strip_prefix(prefix) else { return false };
    value.len() == 26
        && value.bytes().all(|c| c.is_ascii_digit() || c.is_ascii_lowercase())
        && ulid::Ulid::from_string(value).is_ok()
}

fn front_matter(doc: &Document) -> Option<Range<usize>> {
    let block = doc.blocks().roots.first().filter(|b| b.kind == BlockKind::FrontMatter)?;
    let range = doc.range_of(block);
    let text = doc.slice(range.clone()).to_string();
    let start = text.find('\n')? + 1;
    let end = text.rfind("\n---")? + 1;
    Some(range.start + start..range.start + end)
}

pub fn read(doc: &Document) -> Result<Metadata, String> {
    #[derive(Deserialize)]
    struct Front {
        aero: Option<Metadata>,
    }
    let Some(range) = front_matter(doc) else { return Ok(Metadata::default()) };
    let source = doc.slice(range).to_string();
    // Validate the namespace's mapping type even for an explicit YAML null.
    namespace(&source)?;
    let front: Front = serde_saphyr::from_str(&source).map_err(|e| format!("文档元数据冲突：{e}"))?;
    let meta = front.aero.unwrap_or_default();
    if meta.id.as_deref().is_some_and(|v| !is_document_id(v))
        || meta.parent.as_deref().is_some_and(|v| !is_document_id(v))
    {
        return Err("aero.id / aero.parent 必须是 d- 开头的完整小写 ULID".into());
    }
    Ok(meta)
}

type Mapping = BTreeMap<String, Spanned<IgnoredAny>>;

fn namespace(source: &str) -> Result<Option<Spanned<Mapping>>, String> {
    #[derive(Deserialize)]
    struct Front {
        #[serde(default)]
        aero: Namespace,
    }
    #[derive(Default, Deserialize)]
    #[serde(transparent)]
    struct Namespace(Option<Spanned<Mapping>>);
    let front: Front = serde_saphyr::from_str(source).map_err(|e| format!("文档元数据冲突：{e}"))?;
    // A present null namespace must not be silently replaced.
    let all: BTreeMap<String, IgnoredAny> =
        serde_saphyr::from_str(source).map_err(|e| format!("文档元数据冲突：{e}"))?;
    if all.contains_key("aero") && front.aero.0.is_none() {
        return Err("已有 aero 字段不是映射，不能自动覆盖".into());
    }
    Ok(front.aero.0)
}

fn span<T>(node: &Spanned<T>) -> Result<Range<usize>, String> {
    if node.referenced != node.defined {
        return Err("aero 元数据使用了 YAML 别名或合并，请先展开后再修改".into());
    }
    let span = node.referenced.span();
    let start = span.byte_offset().ok_or("无法定位 aero 元数据")? as usize;
    let len = span.byte_len().ok_or("无法定位 aero 元数据")? as usize;
    Ok(start..start + len)
}

/// All returned edits use the original document's coordinates, in descending order.
pub fn prepare(doc: &Document, patch: &Patch) -> Result<Vec<Edit>, String> {
    read(doc)?;
    let mut values = Vec::new();
    if let Some(id) = &patch.id {
        if !is_document_id(id) {
            return Err("无效文档 ID".into());
        }
        values.push(("id", id.clone()));
    }
    if let Some(parent) = &patch.parent {
        if parent.as_deref().is_some_and(|id| !is_document_id(id)) {
            return Err("无效父文档 ID".into());
        }
        if let Some(parent) = parent {
            values.push(("parent", parent.clone()));
        }
    }
    if let Some(order) = patch.order {
        values.push(("order", order.to_string()));
    }
    if values.is_empty() && patch.parent != Some(None) {
        return Ok(Vec::new());
    }
    let Some(range) = front_matter(doc) else {
        if values.is_empty() {
            return Ok(Vec::new());
        }
        let fields = values.into_iter().map(|(k, v)| format!("  {k}: {v}\n")).collect::<String>();
        return Ok(vec![Edit::insert(0, format!("---\naero:\n{fields}---\n\n"))]);
    };
    let source = doc.slice(range.clone()).to_string();
    let Some(mapping) = namespace(&source)? else {
        if values.is_empty() {
            return Ok(Vec::new());
        }
        let fields = values.into_iter().map(|(k, v)| format!("  {k}: {v}\n")).collect::<String>();
        return Ok(vec![Edit::insert(range.end, format!("aero:\n{fields}"))]);
    };
    let map_span = span(&mapping)?;
    let flow = source.get(map_span.start..).is_some_and(|s| s.starts_with('{'));
    let mut missing = Vec::new();
    let mut edits = Vec::new();
    if patch.parent == Some(None)
        && let Some(node) = mapping.value.get("parent")
    {
        let value = span(node)?;
        let colon = source[..value.start].rfind(':').ok_or("无法定位父文档字段")?;
        let before = source[..colon].trim_end();
        let key = ["\"parent\"", "'parent'", "parent"]
            .into_iter()
            .find(|key| before.ends_with(key))
            .ok_or("无法无损移除父文档字段")?;
        let start = before.len() - key.len();
        if flow {
            let suffix = &source[value.end..];
            let spaces = suffix.len() - suffix.trim_start().len();
            if suffix[spaces..].starts_with(',') {
                edits.push(Edit::delete(range.start + start..range.start + value.end + spaces + 1));
            } else {
                let prefix = &source[map_span.start + 1..start];
                let trimmed = prefix.trim_end();
                let from = if trimmed.ends_with(',') { map_span.start + trimmed.len() } else { start };
                edits.push(Edit::delete(range.start + from..range.start + value.end));
            }
        } else {
            let line = source[..start].rfind('\n').map_or(0, |i| i + 1);
            if !source[line..start].chars().all(char::is_whitespace) {
                return Err("无法无损移除父文档字段".into());
            }
            let end = source[value.end..].find('\n').map_or(source.len(), |i| value.end + i + 1);
            let comment = source[value.end..end].trim_start();
            let replacement = if comment.starts_with('#') {
                format!("{}{comment}", &source[line..start])
            } else {
                String::new()
            };
            edits.push(Edit::replace(range.start + line..range.start + end, replacement));
            if mapping.value.len() == 1 {
                // Keep the namespace a mapping after removing its final key.
                edits.push(Edit::insert(range.start + line, format!("{}{{}}\n", &source[line..start])));
            }
        }
    }
    for (key, value) in values {
        if let Some(node) = mapping.value.get(key) {
            let r = span(node)?;
            if source.get(r.clone()) != Some(value.as_str()) {
                edits.push(Edit::replace(range.start + r.start..range.start + r.end, value));
            }
        } else {
            missing.push(format!("{key}: {value}"));
        }
    }
    if !missing.is_empty() {
        let (at, text) = if flow {
            (
                map_span.start + 1,
                format!("{}{}", missing.join(", "), if mapping.value.is_empty() { "" } else { ", " }),
            )
        } else {
            let at = source[..map_span.start].rfind('\n').map_or(0, |i| i + 1);
            let indent = " ".repeat(mapping.referenced.column().saturating_sub(1) as usize);
            (at, missing.iter().map(|s| format!("{indent}{s}\n")).collect())
        };
        edits.push(Edit::insert(range.start + at, text));
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.range.start));
    // Reject any YAML shape the span patch cannot round-trip safely.
    let mut probe = doc.clone();
    probe.edit_group(edits.clone()).map_err(|e| e.to_string())?;
    probe.reparse();
    let result = read(&probe)?;
    if patch.id.as_ref().is_some_and(|v| result.id.as_ref() != Some(v))
        || patch.parent.as_ref().is_some_and(|v| &result.parent != v)
        || patch.order.is_some_and(|v| result.order != Some(v))
    {
        return Err("无法无损修改此种 YAML 元数据结构".into());
    }
    Ok(edits)
}

pub fn ensure(doc: &Document) -> Result<(String, Vec<Edit>), String> {
    if let Some(id) = read(doc)?.id {
        return Ok((id, Vec::new()));
    }
    let id = new_document_id();
    let edits = prepare(doc, &Patch { id: Some(id.clone()), ..Patch::default() })?;
    Ok((id, edits))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_patch_preserves_comments_unknown_values_and_undo() {
        for source in [
            "正文\n",
            "---\ntitle: 标题 # comment\nextra: [a, b]\n---\n正文\n",
            "---\naero: # namespace\n  extra: 'a b' # keep\ntitle: 标题\n---\n正文\n",
            "---\naero: {extra: 3, order: 42} # keep\n---\n正文\n",
        ] {
            let mut doc = Document::from_text(source);
            let (id, edits) = ensure(&doc).unwrap();
            assert!(is_document_id(&id));
            doc.edit_group(edits).unwrap();
            doc.reparse();
            assert_eq!(read(&doc).unwrap().id, Some(id));
            assert!(doc.to_string().contains("正文\n"));
            for comment in ["# comment", "# namespace", "# keep", "extra: 'a b'"] {
                if source.contains(comment) {
                    assert!(doc.to_string().contains(comment));
                }
            }
            doc.undo().unwrap();
            assert_eq!(doc.to_string(), source);
        }
    }

    #[test]
    fn invalid_namespace_and_duplicates_are_not_overwritten() {
        for yaml in ["aero: custom", "aero: null", "aero: []", "aero: {}\naero: {}", "aero:\n  id: bad"] {
            let doc = Document::from_text(&format!("---\n{yaml}\n---\nbody"));
            assert!(ensure(&doc).is_err(), "{yaml}");
        }
    }

    #[test]
    fn clearing_parent_omits_key_and_preserves_other_fields_and_comments() {
        let parent = new_document_id();
        for yaml in [
            format!("aero:\n  parent: {parent} # relationship\n  extra: keep\n"),
            format!("aero:\n  parent: {parent}\n"),
            format!("aero: {{parent: {parent}, extra: keep}} # namespace\n"),
            format!("aero: {{extra: keep, 'parent': {parent}}}\n"),
            format!("aero: {{\"parent\": {parent}}}\n"),
        ] {
            let source = format!("---\n{yaml}---\nbody\n");
            let mut doc = Document::from_text(&source);
            let edits = prepare(&doc, &Patch { parent: Some(None), ..Default::default() }).unwrap();
            doc.edit_group(edits).unwrap();
            doc.reparse();
            assert_eq!(read(&doc).unwrap().parent, None);
            assert!(!doc.to_string().contains("parent"), "{}", doc.to_string());
            for preserved in ["extra: keep", "# relationship", "# namespace"] {
                if source.contains(preserved) {
                    assert!(doc.to_string().contains(preserved));
                }
            }
            doc.undo();
            assert_eq!(doc.to_string(), source);
        }
        let doc = Document::from_text("body\n");
        assert!(prepare(&doc, &Patch { parent: Some(None), ..Default::default() }).unwrap().is_empty());
    }
}
