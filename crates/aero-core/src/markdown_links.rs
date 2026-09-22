//! 普通 Markdown 链接目标的原始字节位置（不重排或规范化源码）。

use std::ops::Range;

use comrak::{
    Arena,
    nodes::{AstNode, NodeValue},
    parse_document,
};

use crate::{inline_links::SourceMap, parse};

#[derive(Debug)]
pub(crate) struct Destination {
    pub range: Range<usize>,
    pub url: String,
}

/// 只返回实际生效的链接目标；未使用、重复的引用定义不返回。
/// comrak 0.55 的 ResolvedReference 只有 URL/title，refmap 和 URL scanner
/// 都不公开，也没有定义的 source span。候选地址必须经过整篇内存重解析：
/// 替换它只能改变对应 AST 链接的 URL，不能改变标题、节点位置或其他链接。
/// 已确认的链接无法定位时失败关闭，不能让移动事务漏改。
pub(crate) fn destinations(src: &str) -> Result<Vec<Destination>, String> {
    // 仅剥离解析视图的 BOM，返回时补回字节偏移，不规范化换行或源码。
    let text = src.strip_prefix('\u{feff}').unwrap_or(src);
    let bom = src.len() - text.len();
    let arena = Arena::new();
    let root = parse_document(&arena, text, &parse::comrak_options());
    let links = collect_links(root, text)?;
    let positions = SourceMap::new(text);
    let protected: Vec<_> = root
        .descendants()
        .filter(|n| {
            matches!(
                n.data().value,
                NodeValue::Code(_)
                    | NodeValue::CodeBlock(_)
                    | NodeValue::Math(_)
                    | NodeValue::HtmlInline(_)
                    | NodeValue::HtmlBlock(_)
                    | NodeValue::FrontMatter(_)
            )
        })
        .filter_map(|n| positions.range(n))
        .collect();
    let mut covered: Vec<_> = links.iter().map(|l| !l.bracketed).collect();
    // 短探针避免触发 comrak 按文档长度限制的引用展开预算。
    let mut marker = "x".to_owned();
    while links.iter().any(|l| l.url == marker) {
        marker.push('x');
    }
    let mut result = Vec::new();

    for (index, link) in links.iter().enumerate().filter(|(_, l)| l.bracketed) {
        // 扫描范围来自真实 Link/Image 节点；代码 label、嵌套图片等的假候选
        // 不会通过对应节点 URL 的重解析校验，不需要重新实现 label 语法。
        for (at, _) in text[link.range.clone()].match_indices("](") {
            for range in candidates(text, link.range.start + at + 2) {
                if let Some(changed) = verify(text, &links, &range, &marker, Some(index)) {
                    for i in changed {
                        covered[i] = true;
                    }
                    result.push(Destination { range, url: link.url.clone() });
                    break;
                }
            }
            if covered[index] {
                break;
            }
        }
    }

    if covered.iter().any(|c| !c) {
        // 定义在 AST 中已被移除。这里只找语法分隔符而非全文 URL，且排除
        // 链接用法和保护节点；最终仍由 comrak 证明是实际生效的定义。
        for (at, _) in text.match_indices("]:") {
            if protected.iter().any(|r| r.contains(&at)) || links.iter().any(|l| l.range.contains(&at)) {
                continue;
            }
            for range in candidates(text, at + 2) {
                if protected.iter().any(|r| overlaps(r, &range))
                    || links.iter().any(|l| overlaps(&l.range, &range))
                {
                    continue;
                }
                if let Some(changed) = verify(text, &links, &range, &marker, None) {
                    let url = links[changed[0]].url.clone();
                    if changed.iter().any(|&i| links[i].url != url) {
                        return Err("引用目标对应了不一致的 URL".into());
                    }
                    for i in changed {
                        covered[i] = true;
                    }
                    result.push(Destination { range, url });
                    break;
                }
            }
        }
    }
    if let Some(i) = covered.iter().position(|c| !c) {
        return Err(format!("无法定位 Markdown 链接目标（字节 {}）", links[i].range.start + bom));
    }
    result.sort_by_key(|d| (d.range.start, d.range.end));
    result.dedup_by(|a, b| a.range == b.range && a.url == b.url);
    if result.windows(2).any(|p| overlaps(&p[0].range, &p[1].range)) {
        return Err("Markdown 链接目标区间重叠".into());
    }
    for d in &mut result {
        d.range.start += bom;
        d.range.end += bom;
    }
    Ok(result)
}

#[derive(Debug)]
struct Link {
    range: Range<usize>,
    url: String,
    title: String,
    image: bool,
    bracketed: bool,
}

fn collect_links<'a>(root: &'a AstNode<'a>, src: &str) -> Result<Vec<Link>, String> {
    let positions = SourceMap::new(src);
    root.descendants()
        .filter_map(|node| {
            let data = node.data();
            let (link, image) = match &data.value {
                NodeValue::Link(link) => (link, false),
                NodeValue::Image(link) => (link, true),
                _ => return None,
            };
            Some(positions.range(node).ok_or_else(|| "无法映射 Markdown 链接源码位置".into()).map(|range| {
                let raw = &src[range.clone()];
                // comrak 的跨行 title 会令后续节点列号偏移；不能因源码不以 `[` 开头
                // 就默认为自动链接而漏改。只排除能用原字节正面确认的自动链接。
                let auto = raw.strip_prefix('<').and_then(|s| s.strip_suffix('>')).unwrap_or(raw);
                let autolink = (auto == link.url && auto.contains(':'))
                    || (auto.starts_with("www.") && link.url == format!("http://{auto}"))
                    || (auto.contains('@') && link.url == format!("mailto:{auto}"));
                Link {
                    range,
                    url: link.url.clone(),
                    title: link.title.clone(),
                    image,
                    bracketed: image || !autolink,
                }
            }))
        })
        .collect()
}

/// 证明替换的是完整 URL（而非标签、标题、定义用法或 URL 的某一部分）。
fn verify(
    src: &str,
    before: &[Link],
    range: &Range<usize>,
    marker: &str,
    inline: Option<usize>,
) -> Option<Vec<usize>> {
    src.get(range.clone())?;
    let mut probe = src.to_owned();
    probe.replace_range(range.clone(), marker);
    let arena = Arena::new();
    let root = parse_document(&arena, &probe, &parse::comrak_options());
    let after = collect_links(root, &probe).ok()?;
    if before.len() != after.len() {
        return None;
    }
    let offset = |at: usize| {
        if at >= range.end { at - range.len() + marker.len() } else { at }
    };
    let mut changed = Vec::new();
    for (i, (old, new)) in before.iter().zip(&after).enumerate() {
        if old.image != new.image
            || old.title != new.title
            || offset(old.range.start) != new.range.start
            || offset(old.range.end) != new.range.end
        {
            return None;
        }
        if old.url != new.url {
            if new.url != marker || !old.bracketed || inline.is_some_and(|index| index != i) {
                return None;
            }
            changed.push(i);
        }
    }
    if changed.is_empty() || inline.is_some_and(|i| !changed.contains(&i)) { None } else { Some(changed) }
}

fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

/// 容器中的换行先尝试原字节，再尝试跳过引用前缀；是否真的是前缀由
/// AST 校验决定（不能把以 `>` 开头的合法裸 URL 当成容器标记）。
fn candidates(src: &str, mut at: usize) -> Vec<Range<usize>> {
    let b = src.as_bytes();
    let mut newline = false;
    while b.get(at).is_some_and(u8::is_ascii_whitespace) {
        newline |= matches!(b[at], b'\r' | b'\n');
        at += 1;
    }
    let mut result = Vec::new();
    loop {
        if let Some(range) = url_range(src, at) {
            result.push(range);
        }
        if !newline || b.get(at) != Some(&b'>') {
            break;
        }
        at += 1;
        while b.get(at).is_some_and(|c| matches!(c, b' ' | b'\t')) {
            at += 1;
        }
    }
    result
}

/// 与 comrak 的目标词法一致：角括号、标点转义、至多 32 层括号。
/// 不负责确认这是链接，调用方必须通过 AST 验证。
fn url_range(src: &str, mut at: usize) -> Option<Range<usize>> {
    let b = src.as_bytes();
    if b.get(at) == Some(&b'<') {
        at += 1;
        let start = at;
        while let Some(&c) = b.get(at) {
            match c {
                b'>' => return Some(start..at),
                b'\r' | b'\n' | b'<' => return None,
                b'\\' => at += 1,
                _ => {}
            }
            at += 1;
        }
        return None;
    }
    let start = at;
    let mut depth = 0;
    while let Some(&c) = b.get(at) {
        match c {
            b'\\' if b.get(at + 1).is_some_and(u8::is_ascii_punctuation) => at += 1,
            b'(' => {
                depth += 1;
                if depth > 32 {
                    return None;
                }
            }
            b')' if depth == 0 => break,
            b')' => depth -= 1,
            c if c.is_ascii_whitespace() || (c.is_ascii_control() && c != 0) => break,
            _ => {}
        }
        at += 1;
    }
    (depth == 0 && src.get(start..at).is_some()).then_some(start..at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use comrak::{Arena, nodes::NodeValue, parse_document};

    fn urls(src: &str) -> Vec<String> {
        let arena = Arena::new();
        let root = parse_document(&arena, src, &crate::parse::comrak_options());
        root.descendants()
            .filter_map(|n| match &n.data().value {
                NodeValue::Link(link) | NodeValue::Image(link) => Some(link.url.clone()),
                _ => None,
            })
            .collect()
    }

    // Each expected raw destination is unique. Replace only its returned range, prove
    // the complete document stays byte-identical elsewhere, and reparse the new URL.
    fn check(src: &str, expected: &[(&str, &str, usize)]) {
        let found = destinations(src).unwrap_or_else(|e| panic!("{e}\n{src}"));
        assert_eq!(found.len(), expected.len(), "{found:?}\n{src}");
        assert!(found.windows(2).all(|p| p[0].range.end <= p[1].range.start));
        let before = urls(src);
        for (i, (actual, &(raw, url, uses))) in found.iter().zip(expected).enumerate() {
            let start = src.find(raw).unwrap();
            assert_eq!(actual.range, start..start + raw.len(), "{src}");
            assert_eq!(&src[actual.range.clone()], raw);
            assert_eq!(actual.url, url);
            let replacement = format!("moved-{i}.md");
            let mut edited = src.to_owned();
            edited.replace_range(actual.range.clone(), &replacement);
            assert_eq!(edited, format!("{}{}{}", &src[..start], replacement, &src[start + raw.len()..]));
            let after = urls(&edited);
            assert_eq!(after.len(), before.len(), "{edited}");
            let changed: Vec<_> = before.iter().zip(&after).filter(|(a, b)| a != b).collect();
            assert_eq!(changed.len(), uses, "{src}");
            assert!(changed.iter().all(|(a, b)| a.as_str() == url && b.as_str() == replacement));
        }
    }

    #[test]
    fn markdown_links_inline_destinations_decode_without_changing_other_bytes() {
        check(
            "前😀 [**中 `a[b]` 文**](目录/a\\(b\\)&amp;x%20y.md#节 \"标题\") 后\n\
             ![图 *像*](<图 片&amp;一.png> '图片标题')\n\
             [嵌套](one(two(three)).md) [外部](https://host/a?q=1&amp;b=2)",
            &[
                ("目录/a\\(b\\)&amp;x%20y.md#节", "目录/a(b)&x%20y.md#节", 1),
                ("图 片&amp;一.png", "图 片&一.png", 1),
                ("one(two(three)).md", "one(two(three)).md", 1),
                ("https://host/a?q=1&amp;b=2", "https://host/a?q=1&b=2", 1),
            ],
        );
    }

    #[test]
    fn markdown_links_references_use_only_the_effective_definition_once() {
        check(
            "[显式][ID] ![图][id] [id][] [ID]\n\n\
             [id]: <目录/引用&amp;一.md> \"保留标题\"\n\
             [ID]: ignored.md '无效重复'\n\
             [unused]: unused.md\n",
            &[("目录/引用&amp;一.md", "目录/引用&一.md", 4)],
        );
        // A shortcut use immediately followed by a colon is not a definition.
        check("正文 [id]: ordinary.md\n\n[id]: real.md\n", &[("real.md", "real.md", 1)]);
        check(
            "[straße] [STRASSE][] [文字][Strasse]\n\n[Straße]: &#x4E2D;文%20档.md\n",
            &[("&#x4E2D;文%20档.md", "中文%20档.md", 3)],
        );
        check("[a\\]b] ![图][a\\]b]\n\n[a\\]b]: <a\\>b.md>\n", &[("a\\>b.md", "a>b.md", 2)]);
    }

    #[test]
    fn markdown_links_nested_reference_containers_and_multiline_titles() {
        check(
            "> - [**文字**][a] [b][]\n>\n>   [a]:\n>     <A B.md>\n>     '多行\n>     标题'\n>   - [b]: B(c).md (标题)\n",
            &[("A B.md", "A B.md", 1), ("B(c).md", "B(c).md", 1)],
        );
    }

    #[test]
    fn markdown_links_bom_mixed_line_endings_and_multiline_labels() {
        check(
            "\u{feff}[首](首.md)\r\n前\r\n[跨\r\n行](<跨 行.md>\n 'title')\r\n\
             [ref]\n\r\n[ref]: 引用.md\r\n",
            &[("首.md", "首.md", 1), ("跨 行.md", "跨 行.md", 1), ("引用.md", "引用.md", 1)],
        );
        check("\u{feff}---\r\nx: '[no](front.md)'\n---\r\n[yes](body.md)\n", &[("body.md", "body.md", 1)]);
    }

    #[test]
    fn markdown_links_table_escaped_pipes_map_back_to_source() {
        check(
            "| 左 | 右 |\r\n| --- | --- |\n\
             | 文\\|字 [中\\|文](<a\\|b&amp;c.md> 'T') | ![图](image.png) [r] |\r\n\
             \n[r]: table-ref.md\n",
            &[
                ("a\\|b&amp;c.md", "a|b&c.md", 1),
                ("image.png", "image.png", 1),
                ("table-ref.md", "table-ref.md", 1),
            ],
        );
    }

    #[test]
    fn markdown_links_protected_and_escaped_contexts_are_not_destinations() {
        check(
            "---\nx: '[f]: front.md'\n---\n\
             ```md\n[code]: fake.md\n[x](fake-inline.md)\n```\n\
             ` [inline]: fake.md `\n\n\
             \x20   [indent]: fake.md\n\n\
             $[math](fake.md)$\n\n$$\n[mathdef]: fake.md\n$$\n\n\
             <div>\n[html]: fake.md\n[x](fake.md)\n</div>\n\n\
             <span title='[attr](fake.md)'>text</span>\n\n\
             \\[escaped](fake.md)\n\n\\[escapeddef]: fake.md\n\n\
             [code] [inline] [indent] [mathdef] [html] [escapeddef]\n\n\
             [real](real.md)\n",
            &[("real.md", "real.md", 1)],
        );
    }

    #[test]
    fn markdown_links_unmappable_positions_fail_closed_instead_of_skipping_links() {
        // SourceMap supports LF/CRLF, not lone CR. A confirmed link on its
        // second logical line must stop the transaction, never return Ok([]).
        let src = "before\r[label](target.md)";
        assert_eq!(urls(src), ["target.md"]);
        assert!(destinations(src).is_err());
        assert!(destinations("plain text `code` $math$").unwrap().is_empty());
        assert!(destinations("<https://host/a> www.example.com a@example.com").unwrap().is_empty());
    }

    #[test]
    fn markdown_links_empty_destinations_and_nested_link_image_labels() {
        for (src, at) in [("[x]()", 4), ("![x](<> 'title')", 6)] {
            let found = destinations(src).unwrap();
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].range, at..at);
            assert_eq!(found[0].url, "");
            let mut edited = src.to_owned();
            edited.replace_range(found[0].range.clone(), "new.md");
            assert_eq!(urls(&edited), ["new.md"]);
        }
        check(
            "[![内图](inside.png) **`a]b`**](outer.md) ![a [b](child.md)](parent.png)",
            &[
                ("inside.png", "inside.png", 1),
                ("outer.md", "outer.md", 1),
                ("child.md", "child.md", 1),
                ("parent.png", "parent.png", 1),
            ],
        );
    }
}
