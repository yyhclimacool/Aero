//! Wikilink（`[[...]]`，方案 6.3.1）的扫描、解析与预览链接生成。
//!
//! 无 UI 依赖、纯函数，供三处复用：
//! - aero-app 预览：`to_markdown` 把 `[[...]]` 换成标准链接再交给 TextView（它不认 wikilink）；
//! - aero-index：保存时扫描全文，把链接关系写进 links 表；
//! - aero-app 点击：`decode_note_url` 还原目标与 fragment，[`Resolver`] 按三级顺序解析。

use std::collections::HashMap;
use std::ops::Range;

use comrak::nodes::{NodeValue, Sourcepos};
use comrak::{Arena, parse_document};

use crate::vault::VaultPath;

/// 一个 `[[...]]` 区间。`range` 覆盖双方括号，字节偏移，char 边界。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkSpan {
    pub range: Range<usize>,
    /// `|` 前、`#` 前，trim 后（`\[` 转义已还原）。`[[#标题]]` 这类同篇锚点为空串（此时 `fragment` 必有）。
    pub target: String,
    /// `|` 后的别名（trim 后）；无则 None
    pub alias: Option<String>,
    /// target 中 `#` 后的 fragment（trim 后）；无则 None
    pub fragment: Option<String>,
}

/// 解析结果：命中一篇笔记（带 fragment）、多篇同名候选（歧义，交给用户消歧）、或不存在。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    Resolved {
        path: VaultPath,
        fragment: Option<String>,
    },
    /// 文件名 / 标题命中多篇：不自动挑、不创建，按路径排序的候选交给调用方提示。
    Ambiguous {
        candidates: Vec<VaultPath>,
    },
    Unresolved,
}

pub const NOTE_URL_PREFIX: &str = "aero://note/";

/// 解析目标用的笔记表：路径、文件名、标题都做过小写归一，查找不区分大小写
/// （macOS 默认文件系统不区分大小写，`[[Cargo]]` 与 `cargo.md` 是同一篇）。
///
/// 全部查找都是哈希级别；一次全量索引只建一份，逐条解析时不再遍历笔记列表。
#[derive(Clone, Debug, Default)]
pub struct Resolver {
    /// 小写完整路径 → 真实路径
    by_path: HashMap<String, VaultPath>,
    /// 小写文件名（去 `.md`）→ 路径们（有序，便于稳定的歧义提示）
    by_stem: HashMap<String, Vec<VaultPath>>,
    /// 小写标题 → 路径们
    by_title: HashMap<String, Vec<VaultPath>>,
    /// 小写路径 → 登记时的小写标题（移除时 O(1) 找到 `by_title` 里的那一组）
    title_of: HashMap<String, String>,
}

impl Resolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// 只用路径建表（没有标题信息时，比如按磁盘列表解析）。
    pub fn from_notes(notes: impl IntoIterator<Item = VaultPath>) -> Self {
        let mut r = Self::new();
        for p in notes {
            r.insert(p, None);
        }
        r
    }

    /// 登记一篇笔记；`title` 是 front matter 标题（与文件名相同时不必给，给了也无妨）。
    /// 同一路径重复登记会先移除旧登记。
    pub fn insert(&mut self, path: VaultPath, title: Option<&str>) {
        self.remove(&path);
        let key = fold(path.as_str());
        insert_sorted(self.by_stem.entry(fold(path.stem())).or_default(), path.clone());
        if let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) {
            let t = fold(t);
            insert_sorted(self.by_title.entry(t.clone()).or_default(), path.clone());
            self.title_of.insert(key.clone(), t);
        }
        self.by_path.insert(key, path);
    }

    pub fn remove(&mut self, path: &VaultPath) {
        let key = fold(path.as_str());
        if self.by_path.remove(&key).is_none() {
            return;
        }
        remove_from_group(&mut self.by_stem, &fold(path.stem()), path);
        if let Some(t) = self.title_of.remove(&key) {
            remove_from_group(&mut self.by_title, &t, path);
        }
    }

    pub fn contains(&self, path: &VaultPath) -> bool {
        self.by_path.contains_key(&fold(path.as_str()))
    }

    /// 一篇笔记登记 / 移除 / 改标题后，判定可能受影响的笔记：与它同文件名、同标题，
    /// 以及文件名等于它标题、标题等于它文件名的（含它自己）。`title` 给当前登记的标题以外还想查的标题（比如改前的）。
    pub fn neighbors(&self, path: &VaultPath, title: Option<&str>) -> Vec<VaultPath> {
        let key = fold(path.as_str());
        let stem = fold(path.stem());
        let mut keys = vec![stem];
        if let Some(t) = self.title_of.get(&key) {
            keys.push(t.clone());
        }
        if let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) {
            keys.push(fold(t));
        }
        let mut out: Vec<VaultPath> = vec![path.clone()];
        for k in &keys {
            for group in [self.by_stem.get(k), self.by_title.get(k)].into_iter().flatten() {
                out.extend(group.iter().cloned());
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// 名字有竞争的笔记：与别的笔记同文件名、同标题，或标题撞上别的笔记的文件名（反之亦然）。
    /// 指向这些笔记的链接行在任何一篇入库后都可能改判（唯一 → 歧义、标题让位给文件名），要重新解析。
    /// 全量同步收尾用；单篇场景用 [`Self::neighbors`]。
    pub fn contested(&self) -> Vec<VaultPath> {
        let mut out: Vec<VaultPath> = Vec::new();
        for group in self.by_stem.values().chain(self.by_title.values()).filter(|g| g.len() > 1) {
            out.extend(group.iter().cloned());
        }
        for (key, titled) in &self.by_title {
            if let Some(named) = self.by_stem.get(key)
                && (titled.len() != 1 || named.len() != 1 || titled[0] != named[0])
            {
                out.extend(titled.iter().cloned());
                out.extend(named.iter().cloned());
            }
        }
        out.sort();
        out.dedup();
        out
    }

    pub fn len(&self) -> usize {
        self.by_path.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }

    /// 精确路径（相对链接所在目录，再相对 vault 根，各自 ±`.md`）→ 唯一文件名 → 唯一 front matter 标题。
    /// 文件名 / 标题命中多篇时为歧义；含 `..`、空段或绝对路径的目标直接判未解析，不落到文件名 / 标题匹配。
    pub fn resolve(&self, target: &str, fragment: Option<&str>, note_dir: &str) -> Resolution {
        let target = target.trim();
        if target.is_empty() || escapes(target) {
            return Resolution::Unresolved;
        }
        let fragment = fragment.map(str::trim).filter(|f| !f.is_empty()).map(str::to_owned);
        let resolved = |path: VaultPath| Resolution::Resolved { path, fragment: fragment.clone() };

        let normalized = target.replace('\\', "/");
        let plain = normalized.strip_prefix("./").unwrap_or(&normalized);
        let mut candidates = Vec::with_capacity(4);
        if !note_dir.is_empty() {
            candidates.push(format!("{note_dir}/{plain}"));
        }
        candidates.push(plain.to_owned());
        for c in candidates {
            for key in [format!("{c}.md"), c.clone()] {
                if let Some(p) = self.by_path.get(&fold(&key)) {
                    return resolved(p.clone());
                }
            }
        }

        let name = plain.rsplit('/').next().unwrap_or(plain);
        let name = name.strip_suffix(".md").unwrap_or(name);
        match self.by_stem.get(&fold(name)).map(Vec::as_slice) {
            Some([one]) => return resolved(one.clone()),
            Some(many) if many.len() > 1 => return Resolution::Ambiguous { candidates: many.to_vec() },
            _ => {}
        }
        match self.by_title.get(&fold(target)).map(Vec::as_slice) {
            Some([one]) => resolved(one.clone()),
            Some(many) if many.len() > 1 => Resolution::Ambiguous { candidates: many.to_vec() },
            _ => Resolution::Unresolved,
        }
    }
}

fn fold(s: &str) -> String {
    s.to_lowercase()
}

fn insert_sorted(v: &mut Vec<VaultPath>, p: VaultPath) {
    if let Err(i) = v.binary_search(&p) {
        v.insert(i, p);
    }
}

fn remove_from_group(groups: &mut HashMap<String, Vec<VaultPath>>, key: &str, path: &VaultPath) {
    if let Some(v) = groups.get_mut(key) {
        v.retain(|p| p != path);
        if v.is_empty() {
            groups.remove(key);
        }
    }
}

/// 路径逃逸：绝对路径、`..`、`.`、空段（含反斜杠写法）。
fn escapes(target: &str) -> bool {
    let t = target.replace('\\', "/");
    let t = t.strip_prefix("./").unwrap_or(&t);
    t.starts_with('/') || t.split('/').any(|c| c.is_empty() || c == "." || c == "..")
}

/// 扫描源文本里的全部 wikilink。
///
/// 以 comrak 的识别为准（与文档解析同一套选项），代码块、行内代码、数学、
/// 转义、链接 URL 里的 `[[...]]` 天然不产 WikiLink 节点；`![[` 嵌入与
/// 链接文本内的 wikilink 在这里排除。区间用文本锚定修正 comrak 的 sourcepos
/// 缺陷（表格单元格等）。跨行的 `[[...]]` 不算链接（Obsidian 同）。
pub fn scan(src: &str) -> Vec<LinkSpan> {
    if !src.contains("[[") {
        return Vec::new();
    }
    let arena = Arena::new();
    let root = parse_document(&arena, src, &crate::parse::comrak_options());
    scan_tree(root, src)
}

/// [`scan`] 的树版本：`root` 是用 [`crate::parse::comrak_options`] 解析 `src` 得到的（索引时与标签抽取共用一次解析）。
pub(crate) fn scan_tree<'a>(root: &'a comrak::nodes::AstNode<'a>, src: &str) -> Vec<LinkSpan> {
    if !src.contains("[[") {
        return Vec::new();
    }
    let line_starts = line_starts(src);
    let mut out = Vec::new();
    for node in root.descendants().filter(|n| matches!(n.data().value, NodeValue::WikiLink(_))) {
        if node.ancestors().any(|a| matches!(a.data().value, NodeValue::Image(_) | NodeValue::Link(_))) {
            continue;
        }
        let Some(cand) = sourcepos_range(&line_starts, src, node.data().sourcepos) else { continue };
        let Some(range) = anchor(src, cand) else { continue };
        // `![[x]]`：comrak 产出 Text("!") + WikiLink，靠前导 `!` 排除嵌入
        if range.start > 0 && src.as_bytes()[range.start - 1] == b'!' {
            continue;
        }
        let inner = &src[range.start + 2..range.end - 2];
        if inner.contains('\n') {
            continue;
        }
        let (head, alias) = split_alias(inner);
        let (target_raw, fragment) = split_unescaped(head, '#');
        let target = unescape(target_raw.trim());
        let fragment = fragment.map(|f| unescape(f.trim())).filter(|f| !f.is_empty());
        if target.is_empty() && fragment.is_none() {
            continue;
        }
        out.push(LinkSpan {
            range,
            target,
            alias: alias.map(|a| unescape(a.trim())).filter(|a| !a.is_empty()),
            fragment,
        });
    }
    out.sort_by_key(|s| s.range.start);
    out.dedup_by_key(|s| s.range.start);
    out
}

/// 未解析目标要创建的文件路径：相对链接所在笔记目录，`[[foo.md]]` 与 `[[foo]]` 都建 `<dir>/foo.md`。
/// 路径逃逸（`..`、绝对路径、空段、反斜杠逃逸）、控制字符返回 None，与 [`Resolver::resolve`] 的逃逸判定一致。
pub fn creation_path(note_dir: &str, target: &str) -> Option<VaultPath> {
    let target = target.trim();
    if target.is_empty() || escapes(target) || target.chars().any(char::is_control) {
        return None;
    }
    let stem = target.strip_suffix(".md").unwrap_or(target);
    let s = if note_dir.is_empty() { format!("{stem}.md") } else { format!("{note_dir}/{stem}.md") };
    VaultPath::new(&s).ok()
}

/// `aero://note/<target>#<fragment>`（percent 编码，`/` 保留）。
pub fn encode_note_url(target: &str, fragment: Option<&str>) -> String {
    let mut url = String::from(NOTE_URL_PREFIX);
    push_encoded(&mut url, target);
    if let Some(f) = fragment {
        url.push('#');
        push_encoded(&mut url, f);
    }
    url
}

/// 还原 [`encode_note_url`]；不是本 scheme 或编码非法时返回 None。
pub fn decode_note_url(url: &str) -> Option<(String, Option<String>)> {
    let rest = url.strip_prefix(NOTE_URL_PREFIX)?;
    let (target, fragment) = match rest.split_once('#') {
        Some((t, f)) => (t, Some(f)),
        None => (rest, None),
    };
    let target = decode_component(target)?;
    let fragment = match fragment {
        Some(f) => Some(decode_component(f)?),
        None => None,
    };
    Some((target, fragment))
}

/// 预览预处理：`[[a|别名]]` → `[别名](aero://note/a)`，`[[a#h]]` → `[a > h](aero://note/a#h)`，
/// `[[#h]]` → `[h](aero://note/#h)`。只替换扫描到的区间，区间外字节原样；没有 `[[` 的文本零拷贝以外不做事。
pub fn to_markdown(src: &str) -> String {
    let spans = scan(src);
    if spans.is_empty() {
        return src.to_owned();
    }
    let mut out = String::with_capacity(src.len() + spans.len() * 16);
    let mut pos = 0;
    for s in &spans {
        out.push_str(&src[pos..s.range.start]);
        let display = match (&s.alias, &s.fragment) {
            (Some(alias), _) => escape_link_text(alias),
            (None, Some(f)) if s.target.is_empty() => escape_link_text(f),
            (None, Some(f)) => format!("{} > {}", escape_link_text(&s.target), escape_link_text(f)),
            (None, None) => escape_link_text(&s.target),
        };
        out.push_str(&format!("[{display}]({})", encode_note_url(&s.target, s.fragment.as_deref())));
        pos = s.range.end;
    }
    out.push_str(&src[pos..]);
    out
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut v = vec![0];
    v.extend(text.bytes().enumerate().filter(|&(_, b)| b == b'\n').map(|(i, _)| i + 1));
    v
}

fn sourcepos_range(line_starts: &[usize], text: &str, sp: Sourcepos) -> Option<Range<usize>> {
    if sp.start.line == 0 || sp.end.line == 0 {
        return None;
    }
    let last = line_starts.len() - 1;
    let sl = (sp.start.line - 1).min(last);
    let el = (sp.end.line - 1).min(last);
    let start = line_starts[sl] + sp.start.column.saturating_sub(1);
    let end = line_starts[el] + sp.end.column.saturating_sub(1);
    if start > end || start > text.len() {
        return None;
    }
    let start = text.floor_char_boundary(start);
    let end = text.floor_char_boundary(end.min(text.len()));
    (start < end).then_some(start..end)
}

/// 用文本锚定修正 sourcepos：候选处恰是 `[[` 就照用；否则（表格等已知缺陷）在该行
/// 找 `]]` 收尾离候选终点最近的 `[[...]]`。
fn anchor(src: &str, cand: Range<usize>) -> Option<Range<usize>> {
    if src[cand.start..].starts_with("[[") {
        let end = cand.start + 2 + src[cand.start + 2..].find("]]")? + 2;
        return Some(cand.start..end);
    }
    let line_start = src[..cand.start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = src[cand.end..].find('\n').map_or(src.len(), |i| cand.end + i);
    let mut best: Option<(usize, Range<usize>)> = None;
    let mut pos = line_start;
    while let Some(rel) = src[pos..line_end].find("[[") {
        let start = pos + rel;
        let end = start + 2 + src[start + 2..line_end].find("]]")? + 2;
        let dist = end.abs_diff(cand.end);
        if best.as_ref().is_none_or(|(d, _)| dist < *d) {
            best = Some((dist, start..end));
        }
        pos = start + 2;
    }
    best.map(|(_, r)| r)
}

/// 按第一个 `|` 切出别名。`\|` 同样算分隔符（表格单元格里必须这样写，Obsidian 同），
/// 切出来的 target 不带那个反斜杠。
fn split_alias(s: &str) -> (&str, Option<&str>) {
    match s.find('|') {
        Some(i) => {
            let head = s[..i].strip_suffix('\\').unwrap_or(&s[..i]);
            (head, Some(&s[i + 1..]))
        }
        None => (s, None),
    }
}

/// 按未转义的 `sep` 切分（`\` 转义的 `sep` 不算分隔符）。
fn split_unescaped(s: &str, sep: char) -> (&str, Option<&str>) {
    let bytes = s.as_bytes();
    let mut backslashes = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\\' {
            backslashes += 1;
            continue;
        }
        if b as char == sep && backslashes.is_multiple_of(2) {
            return (&s[..i], Some(&s[i + 1..]));
        }
        backslashes = 0;
    }
    (s, None)
}

/// 还原 comrak wikilink 组件允许的 `\` + 标点转义。
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek().is_some_and(|n| n.is_ascii_punctuation()) {
            out.push(chars.next().unwrap());
        } else {
            out.push(c);
        }
    }
    out
}

fn push_encoded(out: &mut String, s: &str) {
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
}

fn decode_component(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = hex(*bytes.get(i + 1)?)?;
                let lo = hex(*bytes.get(i + 2)?)?;
                out.push(hi * 16 + lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn escape_link_text(s: &str) -> String {
    s.replace('\\', "\\\\").replace('[', "\\[").replace(']', "\\]")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(s: &str) -> VaultPath {
        VaultPath::new(s).unwrap()
    }

    fn resolver(notes: &[&str], titles: &[(&str, &str)]) -> Resolver {
        let mut r = Resolver::from_notes(notes.iter().map(|s| note(s)));
        for &(path, title) in titles {
            r.insert(note(path), Some(title));
        }
        r
    }

    fn one(src: &str) -> LinkSpan {
        let spans = scan(src);
        assert_eq!(spans.len(), 1, "{src:?}");
        spans.into_iter().next().unwrap()
    }

    #[test]
    fn basic_target_and_alias() {
        let s = one("[[a]]");
        assert_eq!(s.target, "a");
        assert_eq!(s.alias, None);
        assert_eq!(s.fragment, None);
        assert_eq!(&"[[a]]"[s.range.clone()], "[[a]]");

        let s = one("[[b|别名]]");
        assert_eq!(s.target, "b");
        assert_eq!(s.alias.as_deref(), Some("别名"));
        assert_eq!(s.fragment, None);
    }

    #[test]
    fn fragment_forms() {
        let s = one("[[a#标题]]");
        assert_eq!(s.target, "a");
        assert_eq!(s.fragment.as_deref(), Some("标题"));
        assert_eq!(s.alias, None);

        let s = one("[[a#^block-id]]");
        assert_eq!(s.fragment.as_deref(), Some("^block-id"));

        let s = one("[[a#h|显示]]");
        assert_eq!(s.target, "a");
        assert_eq!(s.fragment.as_deref(), Some("h"));
        assert_eq!(s.alias.as_deref(), Some("显示"));

        let s = one("[[a#h1#h2]]");
        assert_eq!(s.target, "a");
        assert_eq!(s.fragment.as_deref(), Some("h1#h2"));

        // 同篇锚点：target 为空、fragment 必有
        let s = one("见 [[#小节]]");
        assert_eq!(s.target, "");
        assert_eq!(s.fragment.as_deref(), Some("小节"));
        assert!(scan("[[#]]").is_empty());
    }

    #[test]
    fn byte_ranges_on_unicode_text() {
        let src = "中文前缀[[链接|L]]后缀";
        let s = one(src);
        assert_eq!(&src[s.range.clone()], "[[链接|L]]");
        assert!(src.is_char_boundary(s.range.start) && src.is_char_boundary(s.range.end));
    }

    #[test]
    fn multiple_links_in_one_line() {
        let src = "[[a]] and [[b]] and [[c|三]]";
        let spans = scan(src);
        assert_eq!(spans.len(), 3);
        let texts: Vec<&str> = spans.iter().map(|s| &src[s.range.clone()]).collect();
        assert_eq!(texts, ["[[a]]", "[[b]]", "[[c|三]]"]);
        assert_eq!(spans[2].alias.as_deref(), Some("三"));
    }

    #[test]
    fn skips_code_escapes_embeds_and_multiline() {
        let src = concat!(
            "```\n[[fenced]]\n```\n\n",
            "    [[indented]]\n\n",
            "`[[inline]]`\n\n",
            "\\[[escaped]]\n\n",
            "![[embed]]\n\n",
            "![[embed|400]]\n\n",
            "$[[math]]$\n\n",
            "$$\n[[mathblock]]\n$$\n\n",
            "[链接](http://x/[[in-url]])\n\n",
            "[[跨\n行]]\n",
        );
        assert!(scan(src).is_empty(), "{:?}", scan(src));
        assert!(scan("没有链接的普通段落").is_empty());
    }

    #[test]
    fn skips_wikilink_inside_markdown_link_text() {
        assert!(scan("[见 [[a]]](http://x)").is_empty());
    }

    #[test]
    fn trims_components_and_handles_escaped_separators() {
        let s = one("[[ a | b ]]");
        assert_eq!(s.target, "a");
        assert_eq!(s.alias.as_deref(), Some("b"));

        // 表格单元格里只能写 `\|`：它就是别名分隔符，反斜杠不进 target
        let s = one("[[a\\|b]]");
        assert_eq!(s.target, "a");
        assert_eq!(s.alias.as_deref(), Some("b"));
        let table = "| a |\n|---|\n| [[x\\|别名]] |\n";
        let s = one(table);
        assert_eq!((s.target.as_str(), s.alias.as_deref()), ("x", Some("别名")));

        let s = one("[[a\\#b]]");
        assert_eq!(s.target, "a#b");
        assert_eq!(s.fragment, None);
    }

    #[test]
    fn table_cells_get_every_link() {
        let src = "| a | b |\n|---|---|\n| [[x]] | 中 [[y]] 中 [[y]] |\n";
        let spans = scan(src);
        let texts: Vec<&str> = spans.iter().map(|s| &src[s.range.clone()]).collect();
        assert_eq!(texts, ["[[x]]", "[[y]]", "[[y]]"]);
    }

    #[test]
    fn resolve_exact_path_relative_then_root() {
        let r = resolver(&["rust/cargo.md", "other/cargo.md", "other/sub/cargo.md"], &[]);
        // 相对链接所在目录
        let got = r.resolve("cargo", None, "rust");
        assert_eq!(got, Resolution::Resolved { path: note("rust/cargo.md"), fragment: None });
        // 带 .md 同样命中
        let got = r.resolve("cargo.md", None, "rust");
        assert_eq!(got, Resolution::Resolved { path: note("rust/cargo.md"), fragment: None });
        // 根相对路径
        let got = r.resolve("other/cargo", None, "");
        assert_eq!(got, Resolution::Resolved { path: note("other/cargo.md"), fragment: None });
        // 链接在子目录、目标也带子目录
        let got = r.resolve("sub/cargo", None, "other");
        assert_eq!(got, Resolution::Resolved { path: note("other/sub/cargo.md"), fragment: None });
        // 精确路径先于文件名：三篇同名时从 rust/ 目录里点 [[cargo]] 不算歧义
        let got = r.resolve("./cargo", None, "rust");
        assert_eq!(got, Resolution::Resolved { path: note("rust/cargo.md"), fragment: None });
    }

    #[test]
    fn resolve_unique_filename_and_ambiguity() {
        let r = resolver(&["a/cargo.md"], &[]);
        let got = r.resolve("cargo", None, "b");
        assert_eq!(got, Resolution::Resolved { path: note("a/cargo.md"), fragment: None });

        let r = resolver(&["b/cargo.md", "a/cargo.md"], &[]);
        assert_eq!(
            r.resolve("cargo", None, "c"),
            Resolution::Ambiguous { candidates: vec![note("a/cargo.md"), note("b/cargo.md")] }
        );
        // 同名两篇里从其中一篇的目录点：精确路径命中，不歧义
        assert_eq!(
            r.resolve("cargo", None, "a"),
            Resolution::Resolved { path: note("a/cargo.md"), fragment: None }
        );
    }

    #[test]
    fn resolve_is_case_insensitive() {
        let r = resolver(&["rust/cargo.md"], &[("rust/cargo.md", "Cargo 指南")]);
        for target in ["Cargo", "CARGO.md", "Rust/Cargo", "cargo 指南", "CARGO 指南"] {
            assert_eq!(
                r.resolve(target, None, ""),
                Resolution::Resolved { path: note("rust/cargo.md"), fragment: None },
                "{target}"
            );
        }
    }

    #[test]
    fn resolve_title_and_title_ambiguity() {
        let r = resolver(&["rust/learn.md"], &[("rust/learn.md", "Rust 学习")]);
        let got = r.resolve("Rust 学习", None, "");
        assert_eq!(got, Resolution::Resolved { path: note("rust/learn.md"), fragment: None });
        assert_eq!(r.resolve("不存在", None, ""), Resolution::Unresolved);

        let r = resolver(&["a.md", "b.md"], &[("a.md", "同题"), ("b.md", "同题")]);
        assert_eq!(
            r.resolve("同题", None, ""),
            Resolution::Ambiguous { candidates: vec![note("a.md"), note("b.md")] }
        );
    }

    #[test]
    fn resolve_rejects_path_escape_and_absolute() {
        let r = resolver(&["secret.md"], &[]);
        assert_eq!(r.resolve("../secret", None, "a"), Resolution::Unresolved);
        assert_eq!(r.resolve("/abs", None, "a"), Resolution::Unresolved);
        assert_eq!(r.resolve("a/../../secret", None, ""), Resolution::Unresolved);
        assert_eq!(r.resolve("..\\secret", None, "a"), Resolution::Unresolved);
        assert_eq!(r.resolve("", None, ""), Resolution::Unresolved);
    }

    #[test]
    fn resolver_insert_remove_and_retitle() {
        let mut r = Resolver::new();
        r.insert(note("a.md"), Some("甲"));
        assert!(matches!(r.resolve("甲", None, ""), Resolution::Resolved { .. }));
        // 改标题：旧标题不再命中
        r.insert(note("a.md"), Some("乙"));
        assert_eq!(r.resolve("甲", None, ""), Resolution::Unresolved);
        assert!(matches!(r.resolve("乙", None, ""), Resolution::Resolved { .. }));
        assert_eq!(r.len(), 1);
        r.remove(&note("a.md"));
        assert!(r.is_empty());
        assert_eq!(r.resolve("a", None, ""), Resolution::Unresolved);
        assert_eq!(r.resolve("乙", None, ""), Resolution::Unresolved);
    }

    #[test]
    fn contested_lists_name_collisions_only() {
        let r = resolver(
            &["a/readme.md", "b/readme.md", "solo.md", "x.md", "y.md", "z.md"],
            &[("x.md", "y"), ("y.md", "Y"), ("z.md", "z")],
        );
        // a/b 同文件名；x 的标题撞上 y 的文件名（y 自己的标题也是 Y）；z 标题等于自己文件名不算；solo 无关
        assert_eq!(r.contested(), [note("a/readme.md"), note("b/readme.md"), note("x.md"), note("y.md")]);
        assert!(Resolver::from_notes([note("a.md"), note("b.md")]).contested().is_empty());

        // 单篇视角：x 的邻居是同标题组 / 文件名等于其标题的 y，与自己；再给个"改前标题"能把那一组也带上
        assert_eq!(r.neighbors(&note("x.md"), None), [note("x.md"), note("y.md")]);
        assert_eq!(r.neighbors(&note("x.md"), Some("solo")), [note("solo.md"), note("x.md"), note("y.md")]);
        assert_eq!(r.neighbors(&note("a/readme.md"), None), [note("a/readme.md"), note("b/readme.md")]);
        assert_eq!(r.neighbors(&note("gone.md"), None), [note("gone.md")]);
    }

    #[test]
    fn creation_path_appends_md_and_rejects_escapes() {
        assert_eq!(creation_path("", "a").unwrap().as_str(), "a.md");
        assert_eq!(creation_path("", "foo.md").unwrap().as_str(), "foo.md");
        assert_eq!(creation_path("rust", "cargo").unwrap().as_str(), "rust/cargo.md");
        assert_eq!(creation_path("", "rust/cargo").unwrap().as_str(), "rust/cargo.md");
        assert_eq!(creation_path("", "  a  ").unwrap().as_str(), "a.md");
        assert_eq!(creation_path("", ""), None);
        assert_eq!(creation_path("a", "../x"), None);
        assert_eq!(creation_path("", "/x"), None);
        assert_eq!(creation_path("", "a/../x"), None);
        assert_eq!(creation_path("", "..\\..\\x"), None);
        assert_eq!(creation_path("", "a\tb"), None);
    }

    #[test]
    fn resolve_keeps_fragment() {
        let r = resolver(&["rust/cargo.md"], &[]);
        let got = r.resolve("cargo", Some("标题"), "rust");
        assert_eq!(got, Resolution::Resolved { path: note("rust/cargo.md"), fragment: Some("标题".into()) });
    }

    #[test]
    fn url_roundtrip() {
        for (target, fragment) in [
            ("a", None),
            ("rust/cargo", Some("标题")),
            ("含空格 和#井号", Some("^block-id")),
            ("中文/路径", None),
            ("%25", Some("a b")),
            ("", Some("同篇")),
        ] {
            let url = encode_note_url(target, fragment);
            assert!(url.starts_with(NOTE_URL_PREFIX));
            assert_eq!(decode_note_url(&url), Some((target.to_owned(), fragment.map(str::to_owned))));
        }
        assert_eq!(decode_note_url("https://x/"), None);
        assert_eq!(decode_note_url("aero://note/%zz"), None);
        assert_eq!(decode_note_url("aero://note/%E4"), None);
    }

    #[test]
    fn to_markdown_converts_and_preserves() {
        assert_eq!(to_markdown("[[a]]"), "[a](aero://note/a)");
        assert_eq!(to_markdown("[[a|别名]]"), "[别名](aero://note/a)");
        assert_eq!(to_markdown("[[a#h]]"), "[a > h](aero://note/a#h)");
        assert_eq!(to_markdown("[[#h]]"), "[h](aero://note/#h)");
        assert_eq!(to_markdown("[[a#^bid]]"), "[a > ^bid](aero://note/a#%5Ebid)");
        assert_eq!(to_markdown("前 [[a]] 后"), "前 [a](aero://note/a) 后");
        assert_eq!(to_markdown("[[a]] [[b|c]]"), "[a](aero://note/a) [c](aero://note/b)");
        assert_eq!(to_markdown("**粗体 [[x]]**"), "**粗体 [x](aero://note/x)**");
        assert_eq!(
            to_markdown("`[[code]]` 和\n\n```\n[[fence]]\n```\n\n和 ![[embed]] 和 \\[[esc]]"),
            "`[[code]]` 和\n\n```\n[[fence]]\n```\n\n和 ![[embed]] 和 \\[[esc]]"
        );
        let plain = "没有链接的段落 [a](http://x) 原样";
        assert_eq!(to_markdown(plain), plain);
    }
}
