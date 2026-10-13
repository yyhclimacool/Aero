//! Logical parent/order relationships are derived from file-owned metadata.
use crate::identity::{self, Metadata, Patch};
use crate::transaction::FileChange;
use crate::{Document, Vault, VaultPath};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct Note {
    pub path: VaultPath,
    pub title: String,
    pub metadata: Result<Metadata, String>,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub path: VaultPath,
    pub depth: usize,
    pub issue: Option<String>,
}

pub fn rows(notes: &[Note]) -> Vec<Row> {
    let mut owners: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, note) in notes.iter().enumerate() {
        if let Ok(meta) = &note.metadata
            && let Some(id) = &meta.id
        {
            owners.entry(id).or_default().push(index);
        }
    }
    let mut children: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
    let mut issues = HashMap::new();
    for (index, note) in notes.iter().enumerate() {
        let mut parent = None;
        match &note.metadata {
            Err(error) => {
                issues.insert(index, format!("元数据冲突：{error}"));
            }
            Ok(meta) => {
                if meta.id.as_deref().is_some_and(|id| owners[id].len() > 1) {
                    issues.insert(index, "文档 ID 重复".into());
                } else if let Some(id) = &meta.parent {
                    match owners.get(id.as_str()).map(Vec::as_slice) {
                        Some([owner]) => parent = Some(*owner),
                        Some(_) => {
                            issues.insert(index, "父文档 ID 重复".into());
                        }
                        None => {
                            issues.insert(index, "父文档缺失".into());
                        }
                    }
                }
            }
        }
        // Detect parent cycles without dropping any document.
        let mut seen = HashSet::from([index]);
        let mut ancestor = parent;
        while let Some(next) = ancestor {
            if !seen.insert(next) {
                issues.insert(index, "父子关系成环".into());
                parent = None;
                break;
            }
            ancestor = notes[next]
                .metadata
                .as_ref()
                .ok()
                .and_then(|m| m.parent.as_deref())
                .and_then(|id| owners.get(id))
                .filter(|group| group.len() == 1)
                .map(|group| group[0]);
        }
        children.entry(parent).or_default().push(index);
    }
    for group in children.values_mut() {
        group.sort_by(|&a, &b| {
            let order = |i: usize| notes[i].metadata.as_ref().ok().and_then(|m| m.order);
            match (order(a), order(b)) {
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            }
            .then_with(|| natural(&notes[a].title).cmp(&natural(&notes[b].title)))
            .then_with(|| notes[a].path.cmp(&notes[b].path))
        });
    }
    fn walk(
        parent: Option<usize>,
        depth: usize,
        notes: &[Note],
        children: &HashMap<Option<usize>, Vec<usize>>,
        issues: &HashMap<usize, String>,
        seen: &mut HashSet<usize>,
        result: &mut Vec<Row>,
    ) {
        for &index in children.get(&parent).into_iter().flatten() {
            if !seen.insert(index) {
                continue;
            }
            result.push(Row { path: notes[index].path.clone(), depth, issue: issues.get(&index).cloned() });
            walk(Some(index), depth + 1, notes, children, issues, seen, result);
        }
    }
    let mut result = Vec::new();
    walk(None, 0, notes, &children, &issues, &mut HashSet::new(), &mut result);
    result
}

fn natural(value: &str) -> String {
    let mut result = String::new();
    let mut digits = String::new();
    for ch in value.to_lowercase().chars().chain(std::iter::once('\0')) {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            if !digits.is_empty() {
                let significant = digits.trim_start_matches('0');
                result.push_str(&format!("{:020}{}", significant.len(), significant));
                digits.clear();
            }
            result.push(ch);
        }
    }
    result
}

pub fn child(
    vault: &Vault,
    parent_path: &VaultPath,
    parent: &Document,
) -> Result<(VaultPath, Vec<FileChange>), String> {
    child_at(vault, parent_path, parent, parent.len()..parent.len())
}

pub fn child_at(
    vault: &Vault,
    parent_path: &VaultPath,
    parent: &Document,
    range: std::ops::Range<usize>,
) -> Result<(VaultPath, Vec<FileChange>), String> {
    let before = vault.read_bytes(parent_path).map_err(|e| e.to_string())?.0;
    if before != parent.to_bytes() {
        return Err("父文档版本已变化，请重新执行".into());
    }
    let (parent_id, edits) = identity::ensure(parent)?;
    let mut parent = parent.clone();
    let mut order = 0i64;
    let mut observed = Vec::new();
    let mut ids = HashSet::new();
    for path in vault.notes().map_err(|e| e.to_string())? {
        let (bytes, _) = vault.read_bytes(&path).map_err(|e| e.to_string())?;
        let doc = Document::from_bytes(&bytes).map_err(|e| e.to_string())?;
        let meta = identity::read(&doc).map_err(|e| format!("{path}：{e}"))?;
        if let Some(id) = &meta.id
            && !ids.insert(id.clone())
        {
            return Err("文档 ID 重复，请先解决冲突".into());
        }
        if meta.parent.as_deref() == Some(&parent_id) {
            order = order.max(meta.order.unwrap_or(0));
        }
        if &path != parent_path {
            observed.push(FileChange { path, before: Some(bytes.clone()), after: Some(bytes) });
        }
    }
    let path = (0..)
        .map(|i| {
            let name = if i == 0 { "未命名.md".to_owned() } else { format!("未命名 {i}.md") };
            VaultPath::new(&if parent_path.parent().is_empty() {
                name
            } else {
                format!("{}/{name}", parent_path.parent())
            })
            .unwrap()
        })
        .find(|path| !vault.exists(path))
        .unwrap();
    let mut child = Document::default();
    let id = identity::new_document_id();
    let child_edits = identity::prepare(
        &child,
        &Patch {
            id: Some(id.clone()),
            parent: Some(Some(parent_id)),
            order: Some(order.checked_add(1024).ok_or("同级顺序超出范围")?),
        },
    )?;
    child.edit_group(child_edits).map_err(|e| e.to_string())?;
    let link = format!("[[id:{id}|{}]]", path.stem());
    let link = if range.is_empty() && range.start == parent.len() { format!("\n\n{link}\n") } else { link };
    if edits.iter().any(|edit| edit.range.end > range.start) {
        return Err("不能在文档元数据中创建子文档".into());
    }
    parent.edit(crate::Edit::replace(range, link)).map_err(|e| e.to_string())?;
    parent.edit_group(edits).map_err(|e| e.to_string())?;
    observed.extend([
        FileChange { path: parent_path.clone(), before: Some(before), after: Some(parent.to_bytes()) },
        FileChange { path, before: None, after: Some(child.to_bytes()) },
    ]);
    Ok((observed.last().unwrap().path.clone(), observed))
}

/// Reparent/reorder only metadata. All managed notes are readable and checked
/// before planning, so an unreadable sibling can never be silently skipped.
pub fn reparent(
    vault: &Vault,
    path: &VaultPath,
    parent: Option<&VaultPath>,
    before: Option<&VaultPath>,
) -> Result<Vec<FileChange>, String> {
    let mut docs = std::collections::BTreeMap::new();
    let mut originals = std::collections::BTreeMap::new();
    let mut metas = std::collections::BTreeMap::new();
    let mut ids = HashMap::new();
    for path in vault.notes().map_err(|e| e.to_string())? {
        let (bytes, _) = vault.read_bytes(&path).map_err(|e| e.to_string())?;
        let doc = Document::from_bytes(&bytes).map_err(|e| format!("{path}：{e}"))?;
        let meta = identity::read(&doc).map_err(|e| format!("{path}：{e}"))?;
        if let Some(id) = &meta.id
            && ids.insert(id.clone(), path.clone()).is_some()
        {
            return Err("文档 ID 重复，请先解决冲突".into());
        }
        originals.insert(path.clone(), bytes);
        metas.insert(path.clone(), meta);
        docs.insert(path, doc);
    }
    if !docs.contains_key(path) {
        return Err("目标文档已不存在".into());
    }
    if parent == Some(path) {
        return Err("文档不能成为自己的子文档".into());
    }
    let parent_id = if let Some(parent) = parent {
        let doc = docs.get_mut(parent).ok_or("父文档已不存在")?;
        let (id, edits) = identity::ensure(doc)?;
        doc.edit_group(edits).map_err(|e| e.to_string())?;
        doc.reparse();
        let mut cursor = Some(parent.clone());
        let mut seen = HashSet::new();
        while let Some(current) = cursor {
            if &current == path || !seen.insert(current.clone()) {
                return Err("此移动会形成父子关系循环".into());
            }
            cursor = metas.get(&current).and_then(|m| m.parent.as_ref()).and_then(|id| ids.get(id)).cloned();
        }
        Some(id)
    } else {
        None
    };
    let mut siblings: Vec<_> = metas
        .iter()
        .filter(|(p, m)| *p != path && m.parent == parent_id)
        .map(|(p, m)| (p.clone(), m.order, docs[p].title_or(p.stem())))
        .collect();
    siblings.sort_by(|a, b| {
        a.1.is_none()
            .cmp(&b.1.is_none())
            .then(a.1.cmp(&b.1))
            .then_with(|| natural(&a.2).cmp(&natural(&b.2)))
            .then(a.0.cmp(&b.0))
    });
    let insert = match before {
        Some(before) => siblings.iter().position(|s| &s.0 == before).ok_or("排序目标不是同级文档")?,
        None => siblings.len(),
    };
    siblings.insert(insert, (path.clone(), None, String::new()));
    let gap_order = if siblings.iter().enumerate().all(|(i, s)| i == insert || s.1.is_some()) {
        let previous = insert.checked_sub(1).and_then(|i| siblings[i].1);
        let next = siblings.get(insert + 1).and_then(|s| s.1);
        match (previous, next) {
            (Some(a), Some(b)) => {
                b.checked_sub(a).filter(|gap| *gap > 1).and_then(|gap| a.checked_add(gap / 2))
            }
            (Some(a), None) => a.checked_add(1024),
            (None, Some(b)) => b.checked_sub(1024),
            (None, None) => Some(1024),
        }
    } else {
        None
    };
    for (index, (sibling, _, _)) in siblings.iter().enumerate() {
        if gap_order.is_some() && sibling != path {
            continue;
        }
        let doc = docs.get_mut(sibling).unwrap();
        let patch = Patch {
            id: if sibling == path && metas[path].id.is_none() {
                Some(identity::new_document_id())
            } else {
                None
            },
            parent: (sibling == path).then(|| parent_id.clone()),
            order: Some(gap_order.unwrap_or((index as i64 + 1).checked_mul(1024).ok_or("同级顺序超出范围")?)),
        };
        let edits = identity::prepare(doc, &patch)?;
        doc.edit_group(edits).map_err(|e| e.to_string())?;
        doc.reparse();
    }
    Ok(docs
        .into_iter()
        .map(|(path, doc)| {
            let before = originals.remove(&path).unwrap();
            let after = doc.to_bytes();
            FileChange { path, before: Some(before), after: Some(after) }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reorder_uses_existing_gap_without_writing_unaffected_siblings() {
        let root = tempfile::tempdir().unwrap();
        let vault = Vault::open(root.path()).unwrap();
        let paths: Vec<_> =
            ["a", "b", "c"].into_iter().map(|name| VaultPath::new(&format!("{name}.md")).unwrap()).collect();
        for (i, path) in paths.iter().enumerate() {
            vault
                .create_bytes(
                    path,
                    format!(
                        "---\naero: {{id: {}, order: {}}}\n---\n{}",
                        identity::new_document_id(),
                        (i + 1) * 1024,
                        path.stem()
                    )
                    .as_bytes(),
                )
                .unwrap();
        }
        let changes = reparent(&vault, &paths[2], None, Some(&paths[1])).unwrap();
        assert_eq!(changes.iter().filter(|c| c.before != c.after).count(), 1);
        let changed = changes.iter().find(|c| c.path == paths[2]).unwrap();
        assert_eq!(
            identity::read(&Document::from_bytes(changed.after.as_ref().unwrap()).unwrap()).unwrap().order,
            Some(1536)
        );
    }
    #[test]
    fn child_creation_captures_command_location_and_undo_restores_every_file() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let journal = tempfile::tempdir().unwrap();
        let parent_path = VaultPath::new("folder/parent.md").unwrap();
        let original = "before /子文档 after\n";
        vault.create_bytes(&parent_path, original.as_bytes()).unwrap();
        let parent = Document::from_text(original);
        let start = original.find('/').unwrap();
        let (path, changes) =
            child_at(&vault, &parent_path, &parent, start..start + "/子文档".len()).unwrap();
        assert_eq!(path.as_str(), "folder/未命名.md");
        crate::transaction::Transaction::prepare(&vault, journal.path(), changes.clone())
            .unwrap()
            .commit(&vault)
            .unwrap();
        let (child, _) = vault.read(&path).unwrap();
        let child = identity::read(&child).unwrap();
        let (parent, _) = vault.read(&parent_path).unwrap();
        assert_eq!(child.parent, identity::read(&parent).unwrap().id);
        assert!(parent.to_string().contains(&format!("before [[id:{}|未命名]] after", child.id.unwrap())));
        crate::transaction::Transaction::prepare(
            &vault,
            journal.path(),
            changes.iter().map(FileChange::reverse).collect(),
        )
        .unwrap()
        .commit(&vault)
        .unwrap();
        assert_eq!(vault.read_bytes(&parent_path).unwrap().0, original.as_bytes());
        assert!(!vault.exists(&path));
    }

    #[test]
    fn child_creation_rejects_invalid_managed_metadata_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let parent = VaultPath::new("parent.md").unwrap();
        vault.create_bytes(&parent, b"parent").unwrap();
        vault.create_bytes(&VaultPath::new("bad.md").unwrap(), b"---\naero: invalid\n---\n").unwrap();
        assert!(child(&vault, &parent, &Document::from_text("parent")).is_err());
        assert_eq!(vault.notes().unwrap().len(), 2);
    }

    #[test]
    fn tree_recovers_orphans_preserves_cycles_and_orders_naturally() {
        let parent = identity::new_document_id();
        let note = |name: &str, id: Option<String>, parent: Option<String>, order| Note {
            path: VaultPath::new(&format!("{name}.md")).unwrap(),
            title: name.into(),
            metadata: Ok(Metadata { id, parent, order }),
        };
        let notes = vec![
            note("n10", None, None, None),
            note("n2", None, None, None),
            note("child", None, Some(parent.clone()), Some(1024)),
        ];
        let missing = rows(&notes);
        assert!(missing.iter().any(|r| r.issue.as_deref() == Some("父文档缺失")));
        let mut restored = notes;
        restored.push(note("parent", Some(parent.clone()), None, None));
        let result = rows(&restored);
        assert_eq!(result.iter().find(|r| r.path.stem() == "child").unwrap().depth, 1);
        assert!(
            result.iter().position(|r| r.path.stem() == "n2").unwrap()
                < result.iter().position(|r| r.path.stem() == "n10").unwrap()
        );
        restored.push(note("cycle", Some("cycle".into()), Some("cycle".into()), None));
        assert_eq!(rows(&restored).len(), restored.len());
    }
}
