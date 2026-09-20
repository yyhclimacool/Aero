//! 只跟踪本次编辑中自动产生的配对，不把文件里已有的右括号当成自动配对。

use std::ops::Range;

use aero_core::Edit;

#[derive(Clone, Debug)]
struct Pair {
    open: Range<usize>,
    close: Range<usize>,
    left: &'static str,
    right: &'static str,
}

pub(super) struct Pending {
    pub key: String,
    pub selection: Range<usize>,
    pub before: String,
}

#[derive(Default)]
pub(super) struct State {
    pub pending: Option<Pending>,
    pairs: Vec<Pair>,
}

pub(super) struct Plan {
    pub text: String,
    pub cursor: usize,
}

impl State {
    pub fn is_idle(&self) -> bool {
        self.pending.is_none() && self.pairs.is_empty()
    }

    /// 控件已处理原生输入；只有结果与该按键完全对应时才补全。
    /// IME、粘贴、替换、程序化修改没有对应 Pending，沿用原始文本。
    pub fn complete(&mut self, old: &str, new: &str, selection: Range<usize>) -> Option<Plan> {
        let pending = self.pending.take()?;
        let at = pending.selection.start;
        if pending.before != old || !pending.selection.is_empty() || !selection.is_empty() {
            return None;
        }
        let backspace = pending.key == "backspace";
        let native = if backspace {
            let prev = old.get(..at)?.char_indices().next_back()?.0;
            Edit::delete(prev..at)
        } else {
            Edit::insert(at, &pending.key)
        };
        let mut expected = old.to_owned();
        expected.replace_range(native.range.clone(), &native.text);
        if expected != new || selection.start != native.range.start + native.text.len() {
            return None;
        }
        self.pairs.retain(|p| {
            old.get(p.open.clone()) == Some(p.left)
                && old.get(p.close.clone()) == Some(p.right)
                && !escaped(old, p.open.start)
        });
        if backspace {
            let p = self.pairs.iter().rev().find(|p| p.open.end == at && p.close.start == at)?;
            let start = p.open.start;
            let mut text = old.to_owned();
            text.replace_range(start..p.close.end, "");
            self.adjust(&Edit::delete(start..p.close.end));
            return Some(Plan { text, cursor: start });
        }
        if !escaped(old, at)
            && let Some(ix) = self.pairs.iter().rposition(|p| {
                p.close.contains(&at) && old.get(at..at + pending.key.len()) == Some(pending.key.as_str())
            })
        {
            let cursor = at + pending.key.len();
            if cursor == self.pairs[ix].close.end {
                self.pairs.remove(ix);
            }
            return Some(Plan { text: old.to_owned(), cursor });
        }
        let (left, right, start) = opener(old, at, &pending.key)?;
        let mut text = old.to_owned();
        text.replace_range(start..at, &format!("{left}{right}"));
        self.adjust(&Edit::replace(start..at, format!("{left}{right}")));
        let cursor = start + left.len();
        self.pairs.push(Pair { open: start..cursor, close: cursor..cursor + right.len(), left, right });
        Some(Plan { text, cursor })
    }

    /// 普通输入、粘贴或删除使区间漂移；标记本身发生改动时丢弃那一对。
    pub fn adjust(&mut self, edit: &Edit) {
        self.pairs.retain_mut(|p| {
            let Some(open) = shifted(&p.open, edit) else { return false };
            let Some(close) = shifted(&p.close, edit) else { return false };
            p.open = open;
            p.close = close;
            true
        });
    }
}

fn shifted(range: &Range<usize>, edit: &Edit) -> Option<Range<usize>> {
    if edit.range.end <= range.start {
        let delta = edit.delta();
        Some((range.start as isize + delta) as usize..(range.end as isize + delta) as usize)
    } else if edit.range.start >= range.end {
        Some(range.clone())
    } else {
        None
    }
}

fn escaped(text: &str, at: usize) -> bool {
    text[..at].bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 1
}

fn opener(text: &str, at: usize, key: &str) -> Option<(&'static str, &'static str, usize)> {
    let before = text.get(..at)?;
    let after = text.get(at..)?;
    let (left, right, start) = match key {
        "*" | "=" | "[" if before.ends_with(key) => {
            let start = at - 1;
            if before[..start].ends_with(key) {
                return None;
            }
            match key {
                "*" => ("**", "**", start),
                "=" => ("==", "==", start),
                _ => ("[[", "]]", start),
            }
        }
        "(" => ("(", ")", at),
        "\"" if !before.chars().next_back().is_some_and(|c| c.is_alphanumeric()) => ("\"", "\"", at),
        "`" if !before.ends_with('`') => ("`", "`", at),
        "$" if !before.ends_with('$') => ("$", "$", at),
        _ => return None,
    };
    if escaped(text, start)
        || after.chars().next().is_some_and(|c| !c.is_whitespace() && !")]}.,:;!?\"*=$".contains(c))
    {
        return None;
    }
    // 将输入点换成可解析的探针，复用 Markdown 的代码 / 数学 / URL / 元数据排除规则。
    let embed = left == "[[" && text[..start].ends_with('!');
    let marker = "[[aero-pair-probe]]";
    let mut probe = text.to_owned();
    probe.replace_range(start..at, marker);
    let valid = if embed {
        aero_core::links::scan_embeds(&probe).iter().any(|s| s.range == (start - 1..start + marker.len()))
    } else {
        aero_core::links::scan(&probe).iter().any(|s| s.range == (start..start + marker.len()))
    };
    valid.then_some((left, right, start))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(state: &mut State, text: &mut String, cursor: &mut usize, key: &str) -> bool {
        state.pending = Some(Pending { key: key.into(), selection: *cursor..*cursor, before: text.clone() });
        let edit = if key == "backspace" {
            Edit::delete(text[..*cursor].char_indices().next_back().unwrap().0..*cursor)
        } else {
            Edit::insert(*cursor, key)
        };
        let mut raw = text.clone();
        raw.replace_range(edit.range.clone(), &edit.text);
        let at = edit.range.start + edit.text.len();
        if let Some(plan) = state.complete(text, &raw, at..at) {
            *text = plan.text;
            *cursor = plan.cursor;
            true
        } else {
            state.adjust(&edit);
            *text = raw;
            *cursor = at;
            false
        }
    }

    #[test]
    fn pairs_insert_all_delimiters_and_skip_generated_closers() {
        for (left, right) in
            [("**", "**"), ("==", "=="), ("[[", "]]"), ("`", "`"), ("$", "$"), ("(", ")"), ("\"", "\"")]
        {
            let mut s = State::default();
            let mut text = "中 ".to_owned();
            let mut at = text.len();
            for c in left.chars() {
                key(&mut s, &mut text, &mut at, &c.to_string());
            }
            assert_eq!(text, format!("中 {left}{right}"));
            assert_eq!(at, "中 ".len() + left.len());
            key(&mut s, &mut text, &mut at, "文😀");
            for c in right.chars() {
                assert!(key(&mut s, &mut text, &mut at, &c.to_string()));
            }
            assert_eq!(text, format!("中 {left}文😀{right}"));
            assert_eq!(at, text.len());
            assert!(s.pairs.is_empty());
        }
    }

    #[test]
    fn pairs_empty_backspace_nested_ranges_and_manual_closers() {
        let mut s = State::default();
        let mut text = String::new();
        let mut at = 0;
        for c in "((".chars() {
            key(&mut s, &mut text, &mut at, &c.to_string());
        }
        assert_eq!(text, "(())");
        assert!(key(&mut s, &mut text, &mut at, "backspace"));
        assert_eq!(text, "()");
        assert!(key(&mut s, &mut text, &mut at, "backspace"));
        assert!(text.is_empty());
        text = ")".into();
        at = 0;
        assert!(!key(&mut s, &mut text, &mut at, ")"));
        assert_eq!(text, "))");
    }

    #[test]
    fn pairs_context_excludes_code_math_links_metadata_and_escaped_openers() {
        for (src, delimiter) in [
            ("`x|y`", "("),
            ("```\nx|y\n```", "("),
            ("$x|y$", "("),
            ("[x](https://host/|)", "("),
            ("[[note|]]", "("),
            ("---\ntitle: |\n---\n", "\""),
            ("<b title='|'>", "("),
            ("\\|", "("),
            ("\\*|", "*"),
            ("\\[|", "["),
            ("***|", "*"),
            ("word|", "\""),
            ("|word", "("),
            ("``|", "`"),
            ("$$|", "$"),
        ] {
            let at = src.find('|').unwrap();
            let text = src.replacen('|', "", 1);
            assert!(opener(&text, at, delimiter).is_none(), "{src} {delimiter}");
        }
        for src in ["> [!note]\n> §", "| A | B |\n| --- | --- |\n| § | x |", "- §"] {
            let at = src.find('§').unwrap();
            let text = src.replace('§', "");
            assert!(opener(&text, at, "(").is_some(), "{src}");
        }
        assert!(opener("![", 2, "[").is_some());
    }

    #[test]
    fn pairs_do_not_transform_paste_selection_or_stale_pending() {
        let mut s = State::default();
        assert!(s.complete("", "(", 1..1).is_none());
        s.pending = Some(Pending { key: "(".into(), selection: 0..0, before: "".into() });
        assert!(s.complete("", "（", 3..3).is_none());
        s.pending = Some(Pending { key: "(".into(), selection: 0..1, before: "x".into() });
        assert!(s.complete("x", "(", 1..1).is_none());
        s.pending = Some(Pending { key: "(".into(), selection: 0..0, before: "old".into() });
        assert!(s.complete("new", "(new", 1..1).is_none());
    }

    #[test]
    fn pairs_track_edits_before_and_inside_but_drop_modified_markers() {
        let mut s = State::default();
        let mut text = "中 ".into();
        let mut at = "中 ".len();
        key(&mut s, &mut text, &mut at, "(");
        at = 0;
        key(&mut s, &mut text, &mut at, "😀");
        at = text.len() - 1;
        key(&mut s, &mut text, &mut at, "正文");
        assert!(key(&mut s, &mut text, &mut at, ")"));
        assert_eq!(text, "😀中 (正文)");
        at = text.len();
        key(&mut s, &mut text, &mut at, "(");
        let open = at - 1;
        s.adjust(&Edit::replace(open..at, "x"));
        text.replace_range(open..at, "x");
        assert!(!key(&mut s, &mut text, &mut at, ")"));
        assert_eq!(text, "😀中 (正文)x))");
    }
}
