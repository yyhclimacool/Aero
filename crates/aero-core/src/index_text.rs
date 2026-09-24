//! Search text, not a Markdown formatter: walk the AST once and keep literal nodes opaque.

use comrak::nodes::{AstNode, ListType, NodeValue};
use comrak::{Arena, parse_document};

use crate::block::{Attrs, BlockKind};
use crate::inline_links::SourceMap;
use crate::parse::{comrak_options, markers};

pub(super) fn plain_text(src: &str, kind: &BlockKind) -> String {
    Extractor::default().plain_text(src, kind)
}

/// Leaves are visited in source order. Keep HTML state for the whole note, not
/// just a single leaf: blank lines can split inline script/style across blocks.
#[derive(Default)]
pub(super) struct Extractor {
    html: Html,
}

impl Extractor {
    pub(super) fn plain_text(&mut self, src: &str, kind: &BlockKind) -> String {
        if matches!(kind, BlockKind::FrontMatter | BlockKind::Marker | BlockKind::ThematicBreak) {
            return String::new();
        }
        // A leaf's source position may already omit the first line's indentation.
        // Its original kind, not an isolated reparse, determines whether this is literal code.
        if matches!(kind, BlockKind::CodeBlock) {
            return if self.html.hidden.is_some() {
                String::new()
            } else {
                src.lines()
                    .map(|line| line.strip_prefix("    ").or_else(|| line.strip_prefix('\t')).unwrap_or(line))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
        }
        let arena = Arena::new();
        let mut options = comrak_options();
        options.parse.escaped_char_spans = true;
        // A block slice is not a document preamble.
        options.extension.front_matter_delimiter = None;
        let root = parse_document(&arena, src, &options);
        let mut out = Text::default();
        let source = (src, SourceMap::new(src));
        walk(root, &mut out, &mut self.html, false, matches!(kind, BlockKind::Paragraph), &source);
        out.finish(src)
    }
}

/// Byte flags distinguish actual syntax from escaped characters and code/math literals.
/// Both passes are linear in the emitted text; no per-character scan over source ranges.
#[derive(Default)]
struct Text {
    value: String,
    literal: Vec<bool>,
    /// Source offsets of cleanup syntax, not offsets into entity-decoded text.
    origin: Vec<Option<usize>>,
    boundary: bool,
}

impl Text {
    fn push(&mut self, text: &str, literal: bool) {
        if text.is_empty() {
            return;
        }
        if self.boundary && !self.value.is_empty() && !self.value.ends_with('\n') {
            self.value.push('\n');
            self.literal.push(false);
            self.origin.push(None);
        }
        self.boundary = false;
        self.value.push_str(text);
        self.literal.resize(self.value.len(), literal);
        self.origin.resize(self.value.len(), None);
    }

    fn text(&mut self, text: &str, literal: bool, raw: Option<(&str, usize)>) {
        self.push(text, literal);
        if literal {
            return;
        }
        let start = self.value.len() - text.len();
        let mut syntax = raw.into_iter().flat_map(|(raw, base)| source_syntax(raw, base));
        for (i, c) in text.char_indices().filter(|(_, c)| cleanup_syntax(*c)) {
            let at = start + i;
            match syntax.next() {
                Some((original, Some(offset))) if original == c => self.origin[at] = Some(offset),
                // Entity-produced punctuation is visible text, never new markup.
                _ => self.literal[at] = true,
            }
        }
    }

    fn finish(self, src: &str) -> String {
        let value = self.value;
        let bytes = value.as_bytes();
        let mut omit = vec![false; value.len()];
        let mut brackets = Vec::new();
        let mut closed = None;
        let mut brace = None;
        let mut embed = None;
        // Track nesting once: a JSON object's inner braces are not attribute lists,
        // and unmatched braces never repeatedly search the remaining source.
        for (i, c) in value.char_indices() {
            if self.literal[i] {
                continue;
            }
            match c {
                '!' if bytes[i..].starts_with(b"![[") => embed = Some(i),
                '[' => brackets.push(i),
                ']' => {
                    closed = brackets.pop().map(|open| (open, i));
                    if bytes.get(i + 1) == Some(&b']')
                        && let Some(start) = embed.take()
                    {
                        let inner = &value[start + 3..i];
                        let (target, _) = crate::links::split_alias(inner);
                        if !target.is_empty() && !inner.contains(['[', ']']) {
                            omit[start..start + 3].fill(true);
                            omit[start + 3 + target.len()..i + 2].fill(true);
                        }
                    }
                }
                '{' => {
                    if let Some((_, depth, eligible)) = &mut brace {
                        *depth += 1;
                        *eligible = false;
                    } else {
                        brace = Some((i, 1, true));
                    }
                }
                '}' => {
                    if let Some((open, depth, eligible)) = &mut brace {
                        *depth -= 1;
                        if *depth == 0 {
                            let raw = self.origin[*open]
                                .zip(self.origin[i])
                                .and_then(|(start, end)| src.get(start + 1..end));
                            if *eligible && raw.and_then(Attrs::parse_inner).is_some() {
                                omit[*open..=i].fill(true);
                                if let Some((start, end)) = closed
                                    && end + 1 == *open
                                {
                                    omit[start] = true;
                                    omit[end] = true;
                                }
                            }
                            brace = None;
                        }
                    }
                }
                '\n' => {
                    brackets.clear();
                    closed = None;
                    if let Some((_, _, eligible)) = &mut brace {
                        *eligible = false;
                    }
                    embed = None;
                }
                _ => {}
            }
        }
        // Block IDs are only metadata at a line end; escaped carets and code stay literal.
        let mut offset = 0;
        for line in value.split_inclusive('\n') {
            let line = line.trim_end_matches('\n');
            let trailing = markers::strip_trailing(line);
            if trailing.id.is_some() {
                let end = offset + line.trim_end().len();
                if let Some(caret) = bytes[offset..end].iter().rposition(|&b| b == b'^')
                    && !self.literal[offset + caret]
                {
                    // strip_trailing also recognizes decoded attributes. Only remove
                    // preceding attributes that the source-based pass actually omitted.
                    let start = value[offset..offset + caret]
                        .char_indices()
                        .rev()
                        .take_while(|(i, c)| c.is_whitespace() || omit[offset + i])
                        .last()
                        .map_or(offset + caret, |(i, _)| offset + i);
                    omit[start..end].fill(true);
                }
            } else if markers::parse_marker_line(line).is_some()
                && let Some(first) = line.find(|c: char| !c.is_whitespace())
                && let Some(start) = self.origin[offset + first]
                && markers::parse_marker_line(src[start..].lines().next().unwrap_or("")).is_some()
                && !self.literal[offset + first]
            {
                omit[offset..offset + line.len()].fill(true);
            }
            offset += line.len() + 1;
        }
        value.char_indices().filter_map(|(i, c)| (!omit[i]).then_some(c)).collect()
    }
}

fn cleanup_syntax(c: char) -> bool {
    matches!(c, '!' | '[' | ']' | '{' | '}' | '^' | '\n')
}

/// Align only cleanup punctuation, not every decoded character. Other entities
/// (including multi-codepoint ones) cannot change this sequence. This avoids
/// duplicating Comrak's full entity decoder or reparsing individual text nodes.
fn source_syntax(src: &str, base: usize) -> impl Iterator<Item = (char, Option<usize>)> + '_ {
    let mut consumed = 0;
    src.char_indices().filter_map(move |(i, c)| {
        if i < consumed {
            return None;
        }
        if c == '&'
            && let Some((decoded, len)) = syntax_entity(&src[i + 1..])
        {
            consumed = i + 1 + len;
            return Some((decoded, None));
        }
        cleanup_syntax(c).then_some((c, Some(base + i)))
    })
}

fn syntax_entity(src: &str) -> Option<(char, usize)> {
    let end = src.bytes().take(32).position(|b| b == b';')?;
    let entity = &src[..end];
    let decoded = match entity {
        "excl" => '!',
        "lbrack" | "lsqb" => '[',
        "rbrack" | "rsqb" => ']',
        "lbrace" | "lcub" => '{',
        "rbrace" | "rcub" => '}',
        "Hat" => '^',
        "NewLine" => '\n',
        _ => {
            let number = entity.strip_prefix('#')?;
            let (digits, radix, max) = match number.strip_prefix('x').or_else(|| number.strip_prefix('X')) {
                Some(hex) => (hex, 16, 6),
                None => (number, 10, 7),
            };
            if digits.is_empty() || digits.len() > max || !digits.chars().all(|c| c.is_digit(radix)) {
                return None;
            }
            char::from_u32(u32::from_str_radix(digits, radix).ok()?)?
        }
    };
    cleanup_syntax(decoded).then_some((decoded, end + 1))
}

fn walk<'a>(
    node: &'a AstNode<'a>,
    out: &mut Text,
    html: &mut Html,
    literal: bool,
    preserve_numbers: bool,
    source: &(&str, SourceMap<'_>),
) {
    let data = node.data();
    match &data.value {
        NodeValue::FrontMatter(_) | NodeValue::ThematicBreak | NodeValue::FootnoteReference(_) => return,
        NodeValue::HtmlInline(text) => {
            html.append(text, out);
            return;
        }
        NodeValue::HtmlBlock(block) => {
            out.boundary = true;
            html.append(block.literal.trim_end_matches(['\n', '\r']), out);
            return;
        }
        _ if html.hidden.is_some() => {
            // Inline script/style contents can be parsed as emphasis, links, even code.
            // Still visit descendants so that a closing HTML node can end suppression.
            for child in node.children() {
                walk(child, out, html, literal, preserve_numbers, source);
            }
            return;
        }
        NodeValue::Text(text) => {
            let raw = source
                .1
                .range(node)
                .and_then(|range| source.0.get(range.clone()).map(|raw| (raw, range.start)));
            out.text(text, literal, raw);
            return;
        }
        NodeValue::Code(code) => {
            out.push(&code.literal, true);
            return;
        }
        NodeValue::CodeBlock(code) => {
            out.boundary = true;
            out.push(code.literal.strip_suffix('\n').unwrap_or(&code.literal).trim_end_matches('\r'), true);
            return;
        }
        NodeValue::Math(math) => {
            out.push(&math.literal, true);
            return;
        }
        NodeValue::SoftBreak | NodeValue::LineBreak => {
            out.push("\n", false);
            return;
        }
        NodeValue::WikiLink(_)
            if !out.boundary && out.value.ends_with('!') && out.literal.last() == Some(&true) =>
        {
            if let Some(raw) = source.1.range(node).and_then(|range| source.0.get(range)) {
                out.push(raw, true);
                return;
            }
        }
        NodeValue::Paragraph | NodeValue::Heading(_) | NodeValue::TableRow(_) => {
            out.boundary = true;
            if preserve_numbers
                && matches!(data.value, NodeValue::Paragraph)
                && node.previous_sibling().is_none()
                && let Some(parent) = node.parent()
                && let NodeValue::Item(list) = &parent.data().value
                && list.list_type == ListType::Ordered
            {
                out.push(&format!("{}. ", list.start), false);
            }
        }
        NodeValue::TableCell if node.previous_sibling().is_some() => out.push("  ", false),
        _ => {}
    }
    let start = out.value.len();
    let escaped = literal || matches!(data.value, NodeValue::Escaped | NodeValue::WikiLink(_));
    for child in node.children() {
        walk(child, out, html, escaped, preserve_numbers, source);
    }
    // Callouts are Aero syntax, deliberately not Comrak alerts. Only the first
    // paragraph of a quote can contain a callout header; ordinary [!tip] stays text.
    if matches!(data.value, NodeValue::Paragraph)
        && node.previous_sibling().is_none()
        && node.parent().is_some_and(|p| matches!(p.data().value, NodeValue::BlockQuote))
    {
        let header_start = start + usize::from(out.value[start..].starts_with('\n'));
        if !out.literal.get(header_start).copied().unwrap_or(false)
            && let Some((_, _, title, end)) = markers::callout_header(&out.value[header_start..])
        {
            let title_len = title.as_ref().map_or(0, String::len);
            let prefix_end = header_start + end - title_len;
            out.value.drain(header_start..prefix_end);
            out.literal.drain(header_start..prefix_end);
            out.origin.drain(header_start..prefix_end);
        }
    }
}

/// Small HTML text scanner, not a renderer: omit tags, comments and non-content
/// elements. Inline state persists across AST siblings (e.g. <script>*text*</script>).
#[derive(Default)]
struct Html {
    hidden: Option<(String, usize)>,
}

impl Html {
    fn append(&mut self, mut src: &str, out: &mut Text) {
        while !src.is_empty() {
            // Script/style are raw-text elements: a comparison's '<' must not
            // swallow their closing tag (or the visible HTML following it).
            if src.starts_with('<')
                && let Some((name, _)) = &self.hidden
                && matches!(name.as_str(), "script" | "style")
                && !src.get(2..2 + name.len()).is_some_and(|s| {
                    s.eq_ignore_ascii_case(name)
                        && src.starts_with("</")
                        && src
                            .as_bytes()
                            .get(2 + name.len())
                            .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>')
                })
            {
                src = &src[1..];
                continue;
            }
            if src.starts_with("<!--") {
                let Some(end) = src.find("-->") else { return };
                src = &src[end + 3..];
                continue;
            }
            if src.starts_with('<') {
                let after = src[1..].strip_prefix('/').unwrap_or(&src[1..]);
                if !after
                    .as_bytes()
                    .first()
                    .is_some_and(|b| b.is_ascii_alphabetic() || matches!(b, b'!' | b'?'))
                {
                    if self.hidden.is_none() {
                        out.push("<", true);
                    }
                    src = &src[1..];
                    continue;
                }
                let mut quote = None;
                let end = src.char_indices().skip(1).find_map(|(i, c)| match (quote, c) {
                    (Some(q), c) if c == q => {
                        quote = None;
                        None
                    }
                    (None, '\'' | '"') => {
                        quote = Some(c);
                        None
                    }
                    (None, '>') => Some(i),
                    _ => None,
                });
                let Some(end) = end else { return };
                let tag = src[1..end].trim();
                let closing = tag.starts_with('/');
                let tag = tag.trim_start_matches('/');
                let name = tag.split(|c: char| c.is_ascii_whitespace() || c == '/').next().unwrap_or("");
                let name = name.to_ascii_lowercase();
                if let Some((hidden, depth)) = &mut self.hidden {
                    if *hidden == name {
                        if closing {
                            *depth -= 1;
                            if *depth == 0 {
                                self.hidden = None;
                            }
                        } else if !matches!(name.as_str(), "script" | "style") {
                            *depth += 1;
                        }
                    }
                } else if !closing
                    && matches!(name.as_str(), "script" | "style" | "head" | "template" | "noscript")
                {
                    self.hidden = Some((name, 1));
                } else if name == "br" {
                    out.push("\n", true);
                } else if matches!(
                    name.as_str(),
                    "p" | "div"
                        | "section"
                        | "article"
                        | "li"
                        | "ul"
                        | "ol"
                        | "tr"
                        | "table"
                        | "hr"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                ) {
                    out.boundary = true;
                } else if matches!(name.as_str(), "td" | "th") && closing {
                    out.push("  ", true);
                }
                src = &src[end + 1..];
            } else {
                let end = src.find('<').unwrap_or(src.len());
                if self.hidden.is_none() {
                    html_text(&src[..end], out);
                }
                src = &src[end..];
            }
        }
    }
}

fn html_text(src: &str, out: &mut Text) {
    // Markdown text nodes are already entity-decoded. Raw HTML needs the common
    // named entities and numeric references; unknown names remain searchable as-is.
    let mut start = 0;
    for (i, _) in src.match_indices('&') {
        if i < start {
            continue;
        }
        let rest = &src[i + 1..];
        let Some(end) = rest.bytes().take(33).position(|b| b == b';') else { continue };
        let entity = &rest[..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|n| u32::from_str_radix(n, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|n| n.parse().ok()))
                .and_then(char::from_u32),
        };
        if let Some(c) = decoded {
            out.push(&src[start..i], true);
            out.push(c.encode_utf8(&mut [0; 4]), true);
            start = i + end + 2;
        }
    }
    out.push(&src[start..], true);
}
