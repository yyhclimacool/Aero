//! 标签侧栏的分级树（`ui_design.md` 4.3）：`rust/cli` 显示为 `rust` 下的 `cli`。
//!
//! 计数按「点这项会搜到多少篇」算：父标签含子标签（与 `tag:#rust` 同一条 [`aero_core::index::tag_matches`]）。
//! 一篇笔记同时写了 `#rust` 和 `#rust/cli` 时，父节点会计两次——补全目录不自动生成祖先，这种情况很少。

use aero_index::TagCount;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// 完整路径，点它就搜 `tag:#{path}`
    pub path: String,
    /// 这一层的名字（`rust/cli` 的 `cli`）
    pub name: String,
    pub count: u64,
    pub children: Vec<Node>,
}

/// 把笔记级标签计数折成嵌套树。只出现在子标签里的祖先也会占一个节点，方便展开。
pub fn build(counts: &[TagCount]) -> Vec<Node> {
    let exact: std::collections::BTreeMap<&str, u64> =
        counts.iter().map(|item| (item.tag.as_str(), item.note_count)).collect();
    let mut paths = std::collections::BTreeSet::<String>::new();
    for tag in exact.keys() {
        let mut rest = *tag;
        paths.insert(rest.to_owned());
        while let Some((parent, _)) = rest.rsplit_once('/') {
            if !parent.is_empty() {
                paths.insert(parent.to_owned());
            }
            rest = parent;
        }
    }

    fn inclusive(path: &str, exact: &std::collections::BTreeMap<&str, u64>) -> u64 {
        exact
            .iter()
            .filter(|(tag, _)| aero_core::index::tag_matches(path, tag))
            .map(|(_, count)| *count)
            .sum()
    }

    fn children_of(
        prefix: Option<&str>,
        paths: &std::collections::BTreeSet<String>,
        exact: &std::collections::BTreeMap<&str, u64>,
    ) -> Vec<Node> {
        let mut kids = Vec::new();
        for path in paths {
            let parent = path.rsplit_once('/').map(|(p, _)| p);
            if parent != prefix {
                continue;
            }
            let name = path.rsplit_once('/').map(|(_, n)| n).unwrap_or(path.as_str()).to_owned();
            kids.push(Node {
                path: path.clone(),
                name,
                count: inclusive(path, exact),
                children: children_of(Some(path), paths, exact),
            });
        }
        kids.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));
        kids
    }

    children_of(None, &paths, &exact)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(pairs: &[(&str, u64)]) -> Vec<TagCount> {
        pairs.iter().map(|(tag, n)| TagCount { tag: (*tag).into(), note_count: *n }).collect()
    }

    #[test]
    fn nested_paths_become_a_tree_with_inclusive_counts() {
        let tree = build(&counts(&[
            ("rust", 1),
            ("rust/cli", 1),
            ("rust/cli/parse", 1),
            ("cli", 2),
            ("中文/工具", 1),
        ]));
        assert_eq!(tree.len(), 3);
        assert_eq!(tree[0].path, "rust");
        assert_eq!(tree[0].count, 3, "父标签含子标签");
        assert_eq!(tree[0].children[0].path, "rust/cli");
        assert_eq!(tree[0].children[0].name, "cli");
        assert_eq!(tree[0].children[0].count, 2);
        assert_eq!(tree[0].children[0].children[0].path, "rust/cli/parse");
        assert_eq!(tree[0].children[0].children[0].count, 1);
        assert_eq!(tree[1].path, "cli");
        assert_eq!(tree[1].count, 2);
        assert_eq!(tree[1].children, []);
        assert_eq!(tree[2].path, "中文");
        assert_eq!(tree[2].name, "中文");
        assert_eq!(tree[2].count, 1);
        assert_eq!(tree[2].children[0].path, "中文/工具");
    }

    #[test]
    fn missing_ancestors_are_synthesized_and_empty_input_is_empty() {
        let tree = build(&counts(&[("a/b/c", 1)]));
        assert_eq!(tree[0].path, "a");
        assert_eq!(tree[0].count, 1);
        assert_eq!(tree[0].children[0].path, "a/b");
        assert_eq!(tree[0].children[0].children[0].path, "a/b/c");
        assert!(build(&[]).is_empty());
    }

    #[test]
    fn siblings_sort_by_count_then_name() {
        let tree = build(&counts(&[("zeta", 1), ("alpha", 1), ("beta", 3)]));
        assert_eq!(tree.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), ["beta", "alpha", "zeta"]);
    }
}
