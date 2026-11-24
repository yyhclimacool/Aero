//! M0 spike #2：comrak vs markdown-rs。
//!
//! 回答 project_plan.md 8.2 提出的问题：哪一个解析器能给 Aero 的
//! "Rope + Block 树持有字节区间" 模型提供足够精确的源位置，扩展成本多大，速度如何。
//!
//! 用法：
//!   cargo run -p spike-md-parser --release -- [额外的笔记目录...]
//! 默认语料：本 crate 的 corpus/（CommonMark spec 用例 + aero-sample.md），
//! 额外目录会递归收集 *.md 作为"真实笔记"组。输出是一份 Markdown 报告，直接贴进 ADR。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use comrak::nodes::NodeValue;
use comrak::{Arena, Options, parse_document};
use markdown::mdast::Node;
use markdown::{ParseOptions, to_mdast};

// ───────────────────────── 语料 ─────────────────────────

struct Doc {
    name: String,
    text: String,
}

fn load_spec_examples(path: &Path) -> Vec<Doc> {
    let Ok(spec) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let fence = "`".repeat(32);
    let mut docs = Vec::new();
    let mut lines = spec.lines().enumerate().peekable();
    while let Some((ln, line)) = lines.next() {
        if !(line.starts_with(&fence) && line.contains("example")) {
            continue;
        }
        let mut md = String::new();
        for (_, l) in lines.by_ref() {
            if l == "." {
                break;
            }
            md.push_str(&l.replace('→', "\t"));
            md.push('\n');
        }
        for (_, l) in lines.by_ref() {
            if l.starts_with(&fence) {
                break;
            }
        }
        docs.push(Doc {
            name: format!("spec#{}(L{})", docs.len() + 1, ln + 1),
            text: md,
        });
    }
    docs
}

fn collect_md(dir: &Path, out: &mut Vec<Doc>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        if p.is_dir() {
            collect_md(&p, out);
        } else if p.extension().is_some_and(|e| e == "md")
            && let Ok(text) = fs::read_to_string(&p)
            && text.len() <= 5 * 1024 * 1024
        {
            out.push(Doc { name: p.to_string_lossy().into_owned(), text });
        }
    }
}

// ───────────────────────── 度量 ─────────────────────────

#[derive(Default, Clone)]
struct Stats {
    docs: usize,
    bytes: usize,
    nodes: usize,
    inline_nodes: usize,
    /// 有非零位置的节点数
    positioned: usize,
    /// 位置能安全切片（char 边界、start <= end <= len）
    sliceable: usize,
    /// Text / Code 类节点：切片与 literal 完全一致
    text_exact: usize,
    /// Text / Code 类节点：切片与 literal 不一致且不能用转义/实体解释
    text_mismatch: usize,
    text_checked: usize,
    /// 顶层块区间乱序或重叠的文档数
    toplevel_disorder_docs: usize,
    /// 顶层块之间的空隙里出现非空白字节的文档数（说明有内容没被任何块覆盖）
    toplevel_gap_docs: usize,
    parse_failures: usize,
    parse_time: Duration,
    kinds: BTreeMap<String, usize>,
    unpositioned_kinds: BTreeMap<String, usize>,
    mismatch_samples: Vec<String>,
}

impl Stats {
    fn merge(&mut self, o: &Stats) {
        self.docs += o.docs;
        self.bytes += o.bytes;
        self.nodes += o.nodes;
        self.inline_nodes += o.inline_nodes;
        self.positioned += o.positioned;
        self.sliceable += o.sliceable;
        self.text_exact += o.text_exact;
        self.text_mismatch += o.text_mismatch;
        self.text_checked += o.text_checked;
        self.toplevel_disorder_docs += o.toplevel_disorder_docs;
        self.toplevel_gap_docs += o.toplevel_gap_docs;
        self.parse_failures += o.parse_failures;
        self.parse_time += o.parse_time;
        for (k, v) in &o.kinds {
            *self.kinds.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &o.unpositioned_kinds {
            *self.unpositioned_kinds.entry(k.clone()).or_default() += v;
        }
        if self.mismatch_samples.len() < sample_cap() {
            self.mismatch_samples.extend(o.mismatch_samples.iter().cloned());
            self.mismatch_samples.truncate(sample_cap());
        }
    }
}

struct NodeInfo {
    kind: String,
    inline: bool,
    range: Option<(usize, usize)>,
    /// 需要与切片核对的字面文本
    literal: Option<String>,
    depth: usize,
}

fn kind_name(debug: &str) -> String {
    debug.split(['(', ' ', '{']).next().unwrap_or(debug).to_string()
}

fn slice_ok(text: &str, (s, e): (usize, usize)) -> bool {
    s <= e && e <= text.len() && text.is_char_boundary(s) && text.is_char_boundary(e)
}

/// 由节点列表统计，与解析器无关。
fn tally(doc: &Doc, nodes: &[NodeInfo], parser: &str, stats: &mut Stats) {
    let text = &doc.text;
    let mut toplevel: Vec<(usize, usize)> = Vec::new();
    for n in nodes {
        stats.nodes += 1;
        if n.inline {
            stats.inline_nodes += 1;
        }
        *stats.kinds.entry(n.kind.clone()).or_default() += 1;
        let Some(r) = n.range else {
            *stats.unpositioned_kinds.entry(n.kind.clone()).or_default() += 1;
            continue;
        };
        stats.positioned += 1;
        if !slice_ok(text, r) {
            if stats.mismatch_samples.len() < sample_cap() {
                stats.mismatch_samples.push(format!(
                    "[{parser}] {} {} 位置越界/非 char 边界 {:?} (len {})",
                    doc.name, n.kind, r, text.len()
                ));
            }
            continue;
        }
        stats.sliceable += 1;
        if n.depth == 1 {
            toplevel.push(r);
        }
        if let Some(lit) = &n.literal {
            stats.text_checked += 1;
            let slice = &text[r.0..r.1];
            if slice == lit || slice.trim() == lit.trim() {
                stats.text_exact += 1;
            } else if slice.contains(lit.trim()) {
                // 例如 Code 节点区间含反引号；算一致
                stats.text_exact += 1;
            } else if slice.contains('\\') || slice.contains('&') || lit.contains('\n') {
                // 转义 / 实体 / 软换行合并造成的差异，可解释
            } else {
                stats.text_mismatch += 1;
                if stats.mismatch_samples.len() < sample_cap() {
                    stats.mismatch_samples.push(format!(
                        "[{parser}] {} {} 切片 {:?} ≠ literal {:?}",
                        doc.name,
                        n.kind,
                        truncate(slice, 40),
                        truncate(lit, 40)
                    ));
                }
            }
        }
    }
    // 顶层块覆盖检查
    let mut disorder = false;
    let mut gap = false;
    for w in toplevel.windows(2) {
        let (a, b) = (w[0], w[1]);
        if b.0 < a.1 {
            disorder = true;
        } else if !text[a.1..b.0].trim().is_empty() {
            gap = true;
        }
    }
    if let (Some(first), Some(last)) = (toplevel.first(), toplevel.last()) {
        if !text[..first.0].trim().is_empty() || !text[last.1..].trim().is_empty() {
            gap = true;
        }
    } else if !text.trim().is_empty() {
        gap = true;
    }
    if disorder {
        stats.toplevel_disorder_docs += 1;
    }
    if gap {
        stats.toplevel_gap_docs += 1;
        if stats.mismatch_samples.len() < sample_cap() {
            stats.mismatch_samples.push(format!(
                "[{parser}] {} 顶层块未覆盖全部非空白内容",
                doc.name
            ));
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

// ───────────────────────── comrak ─────────────────────────

fn comrak_options() -> Options<'static> {
    let mut o = Options::default();
    let e = &mut o.extension;
    e.strikethrough = true;
    e.table = true;
    e.autolink = true;
    e.tasklist = true;
    e.footnotes = true;
    e.front_matter_delimiter = Some("---".to_owned());
    e.math_dollars = true;
    e.wikilinks_title_after_pipe = true;
    e.alerts = true;
    e.highlight = true;
    e.block_directive = true;
    e.cjk_friendly_emphasis = true;
    e.header_attributes = true;
    e.inline_code_attributes = true;
    e.link_attributes = true;
    e.fenced_code_attributes = true;
    o.render.sourcepos = true;
    o
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut v = vec![0];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}

fn comrak_is_inline(v: &NodeValue) -> bool {
    matches!(
        v,
        NodeValue::Text(_)
            | NodeValue::SoftBreak
            | NodeValue::LineBreak
            | NodeValue::Code(_)
            | NodeValue::HtmlInline(_)
            | NodeValue::Raw(_)
            | NodeValue::Emph
            | NodeValue::Strong
            | NodeValue::Strikethrough
            | NodeValue::Highlight
            | NodeValue::Insert
            | NodeValue::Superscript
            | NodeValue::Subscript
            | NodeValue::Underline
            | NodeValue::Link(_)
            | NodeValue::Image(_)
            | NodeValue::FootnoteReference(_)
            | NodeValue::Escaped
            | NodeValue::EscapedTag(_)
            | NodeValue::WikiLink(_)
            | NodeValue::SpoileredText
    ) || matches!(v, NodeValue::Math(m) if !m.display_math)
}

fn comrak_nodes(doc: &Doc, opts: &Options) -> Vec<NodeInfo> {
    let arena = Arena::new();
    let root = parse_document(&arena, &doc.text, opts);
    let ls = line_starts(&doc.text);
    let mut out = Vec::new();
    for node in root.descendants() {
        let ast = node.data();
        if matches!(ast.value, NodeValue::Document) {
            continue;
        }
        let depth = node.ancestors().count() - 1; // Document 的直接子节点 depth = 1
        let sp = ast.sourcepos;
        let range = if sp.start.line == 0 || sp.end.line == 0 {
            None
        } else {
            let sl = (sp.start.line - 1).min(ls.len() - 1);
            let el = (sp.end.line - 1).min(ls.len() - 1);
            let s = ls[sl] + sp.start.column.saturating_sub(1);
            let e = ls[el] + sp.end.column; // end.column 为闭区间
            Some((s, e.min(doc.text.len())))
        };
        let literal = match &ast.value {
            NodeValue::Text(t) => Some(t.to_string()),
            NodeValue::Code(c) => Some(c.literal.clone()),
            _ => None,
        };
        out.push(NodeInfo {
            kind: kind_name(&format!("{:?}", ast.value)),
            inline: comrak_is_inline(&ast.value),
            range,
            literal,
            depth,
        });
    }
    out
}

fn comrak_time(doc: &Doc, opts: &Options, iters: usize) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..iters {
        let arena = Arena::new();
        let t = Instant::now();
        let root = parse_document(&arena, &doc.text, opts);
        let n = root.descendants().count();
        let d = t.elapsed();
        std::hint::black_box(n);
        best = best.min(d);
    }
    best
}

// ───────────────────────── markdown-rs ─────────────────────────

fn mdrs_options() -> ParseOptions {
    let mut o = ParseOptions::gfm();
    o.constructs.frontmatter = true;
    o.constructs.math_flow = true;
    o.constructs.math_text = true;
    o
}

fn mdrs_is_inline(n: &Node) -> bool {
    matches!(
        n,
        Node::Text(_)
            | Node::Break(_)
            | Node::InlineCode(_)
            | Node::InlineMath(_)
            | Node::Delete(_)
            | Node::Emphasis(_)
            | Node::Strong(_)
            | Node::FootnoteReference(_)
            | Node::Image(_)
            | Node::ImageReference(_)
            | Node::Link(_)
            | Node::LinkReference(_)
    )
}

fn mdrs_walk(n: &Node, depth: usize, out: &mut Vec<NodeInfo>) {
    if depth > 0 {
        let literal = match n {
            Node::Text(t) => Some(t.value.clone()),
            Node::InlineCode(c) => Some(c.value.clone()),
            _ => None,
        };
        out.push(NodeInfo {
            kind: kind_name(&format!("{:?}", n)),
            inline: mdrs_is_inline(n),
            range: n.position().map(|p| (p.start.offset, p.end.offset)),
            literal,
            depth,
        });
    }
    if let Some(children) = n.children() {
        for c in children {
            mdrs_walk(c, depth + 1, out);
        }
    }
}

fn mdrs_nodes(doc: &Doc, opts: &ParseOptions) -> Option<Vec<NodeInfo>> {
    let root = to_mdast(&doc.text, opts).ok()?;
    let mut out = Vec::new();
    mdrs_walk(&root, 0, &mut out);
    Some(out)
}

fn mdrs_time(doc: &Doc, opts: &ParseOptions, iters: usize) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..iters {
        let t = Instant::now();
        let r = to_mdast(&doc.text, opts);
        let d = t.elapsed();
        std::hint::black_box(r.is_ok());
        best = best.min(d);
    }
    best
}

// ───────────────────────── 跑一组语料 ─────────────────────────

fn run_group(docs: &[Doc], iters: usize) -> (Stats, Stats) {
    let copts = comrak_options();
    let mopts = mdrs_options();
    let mut cs = Stats::default();
    let mut ms = Stats::default();
    for doc in docs {
        let mut c = Stats {
            docs: 1,
            bytes: doc.text.len(),
            ..Default::default()
        };
        let nodes = comrak_nodes(doc, &copts);
        tally(doc, &nodes, "comrak", &mut c);
        c.parse_time = comrak_time(doc, &copts, iters);
        cs.merge(&c);

        let mut m = Stats {
            docs: 1,
            bytes: doc.text.len(),
            ..Default::default()
        };
        match mdrs_nodes(doc, &mopts) {
            Some(nodes) => tally(doc, &nodes, "markdown-rs", &mut m),
            None => m.parse_failures = 1,
        }
        m.parse_time = mdrs_time(doc, &mopts, iters);
        ms.merge(&m);
    }
    (cs, ms)
}

fn pct(a: usize, b: usize) -> String {
    if b == 0 {
        "-".into()
    } else {
        format!("{:.2}%", a as f64 * 100.0 / b as f64)
    }
}

fn mbps(bytes: usize, d: Duration) -> String {
    if d.is_zero() {
        return "-".into();
    }
    format!("{:.1} MB/s", bytes as f64 / d.as_secs_f64() / 1e6)
}

fn print_group(title: &str, c: &Stats, m: &Stats) {
    println!("### {title}\n");
    println!("| 指标 | comrak | markdown-rs |");
    println!("| --- | --- | --- |");
    println!("| 文档数 / 字节 | {} / {} | {} / {} |", c.docs, c.bytes, m.docs, m.bytes);
    println!("| 解析失败 | {} | {} |", c.parse_failures, m.parse_failures);
    println!("| 节点数（行内） | {} ({}) | {} ({}) |", c.nodes, c.inline_nodes, m.nodes, m.inline_nodes);
    println!(
        "| 有位置的节点 | {} | {} |",
        pct(c.positioned, c.nodes),
        pct(m.positioned, m.nodes)
    );
    println!(
        "| 位置可安全切片 | {} | {} |",
        pct(c.sliceable, c.positioned),
        pct(m.sliceable, m.positioned)
    );
    println!(
        "| Text/Code 切片 = literal | {} / {} 不一致 | {} / {} 不一致 |",
        pct(c.text_exact, c.text_checked),
        c.text_mismatch,
        pct(m.text_exact, m.text_checked),
        m.text_mismatch
    );
    println!(
        "| 顶层块乱序 / 未覆盖内容（文档数） | {} / {} | {} / {} |",
        c.toplevel_disorder_docs, c.toplevel_gap_docs, m.toplevel_disorder_docs, m.toplevel_gap_docs
    );
    println!(
        "| 解析耗时（每文档取最小，累计） | {:.2?} ({}) | {:.2?} ({}) |",
        c.parse_time,
        mbps(c.bytes, c.parse_time),
        m.parse_time,
        mbps(m.bytes, m.parse_time)
    );
    println!();
    for (name, s) in [("comrak", c), ("markdown-rs", m)] {
        if !s.unpositioned_kinds.is_empty() {
            let list: Vec<String> = s
                .unpositioned_kinds
                .iter()
                .map(|(k, v)| format!("{k}×{v}"))
                .collect();
            println!("- {name} 无位置节点：{}", list.join(", "));
        }
        for smp in &s.mismatch_samples {
            println!("- 样例：{smp}");
        }
    }
    println!();
}

fn print_feature_matrix(sample: &Doc) {
    let c = comrak_nodes(sample, &comrak_options());
    let m = mdrs_nodes(sample, &mdrs_options()).unwrap_or_default();
    let has = |nodes: &[NodeInfo], kinds: &[&str]| nodes.iter().any(|n| kinds.contains(&n.kind.as_str()));
    let rows: &[(&str, &[&str], &[&str])] = &[
        ("front matter", &["FrontMatter"], &["Yaml"]),
        ("表格", &["Table"], &["Table"]),
        ("任务列表", &["TaskItem"], &["ListItem"]), // markdown-rs 用 ListItem.checked 表示
        ("删除线", &["Strikethrough"], &["Delete"]),
        ("脚注", &["FootnoteDefinition"], &["FootnoteDefinition"]),
        ("行内公式 $", &["Math"], &["InlineMath"]),
        ("块公式 $$", &["Math"], &["Math"]),
        ("自动链接", &["Link"], &["Link"]),
        ("wikilink [[ ]]", &["WikiLink"], &[]),
        ("高亮 ==", &["Highlight"], &[]),
        ("callout [!note]", &["Alert"], &[]),
        ("::: 容器", &["BlockDirective"], &[]),
        ("标题属性 {.red}", &["Heading"], &[]), // 需再看 attrs，此处只标记语法可被接受
    ];
    println!("### 扩展语法覆盖（aero-sample.md）\n");
    println!("| 语法 | comrak | markdown-rs |");
    println!("| --- | --- | --- |");
    for (name, ck, mk) in rows {
        let cy = if ck.is_empty() { "—" } else if has(&c, ck) { "原生" } else { "无" };
        let my = if mk.is_empty() { "无" } else if has(&m, mk) { "原生" } else { "无" };
        println!("| {name} | {cy} | {my} |");
    }
    println!("| 段尾 ^id / #tag / %% / [text]{{.red}} / :inline[] | 两者都无，走 aero-core 后处理 | |");
    println!();
    println!("comrak 节点种类：{}", c.iter().map(|n| n.kind.clone()).collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>().join(", "));
    println!();
    println!("markdown-rs 节点种类：{}", m.iter().map(|n| n.kind.clone()).collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>().join(", "));
    println!();
}

fn print_big_doc(sample: &Doc) {
    let mut big = String::new();
    while big.len() < 1024 * 1024 {
        big.push_str(&sample.text);
        big.push('\n');
    }
    let big = Doc {
        name: "synthetic-1MiB".into(),
        text: big,
    };
    let mid = Doc {
        name: "synthetic-100KiB".into(),
        text: big.text[..big.text.len().min(100 * 1024)].to_string(),
    };
    let copts = comrak_options();
    let mopts = mdrs_options();
    println!("### 大文件解析耗时（最小值 / 5 次）\n");
    println!("| 文档 | 字节 | comrak | markdown-rs |");
    println!("| --- | --- | --- | --- |");
    for d in [&mid, &big] {
        let ct = comrak_time(d, &copts, 5);
        let mt = mdrs_time(d, &mopts, 5);
        println!(
            "| {} | {} | {:.2?} ({}) | {:.2?} ({}) |",
            d.name,
            d.text.len(),
            ct,
            mbps(d.text.len(), ct),
            mt,
            mbps(d.text.len(), mt)
        );
    }
    println!();
}

/// `--dump <file> [depth]`：打印两种解析器的节点、区间与切片，用于排查位置问题。
fn dump(path: &Path, max_depth: usize) {
    let doc = Doc {
        name: path.display().to_string(),
        text: fs::read_to_string(path).expect("read file"),
    };
    let show = |title: &str, nodes: &[NodeInfo]| {
        println!("## {title}");
        for n in nodes.iter().filter(|n| n.depth <= max_depth) {
            let indent = "  ".repeat(n.depth.saturating_sub(1));
            match n.range {
                Some(r) if slice_ok(&doc.text, r) => println!(
                    "{indent}{} {:?} {:?}",
                    n.kind,
                    r,
                    truncate(&doc.text[r.0..r.1].replace('\n', "⏎"), 60)
                ),
                Some(r) => println!("{indent}{} {:?} <invalid>", n.kind, r),
                None => println!("{indent}{} <no position>", n.kind),
            }
        }
        println!();
    };
    show("comrak", &comrak_nodes(&doc, &comrak_options()));
    show("markdown-rs", &mdrs_nodes(&doc, &mdrs_options()).unwrap_or_default());
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--dump") {
        let depth = args.get(2).and_then(|d| d.parse().ok()).unwrap_or(1);
        dump(Path::new(&args[1]), depth);
        return;
    }
    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus");
    let extra_dirs: Vec<PathBuf> = args.iter().map(PathBuf::from).collect();

    let spec = load_spec_examples(&corpus_dir.join("commonmark-spec.txt"));
    let sample_path = corpus_dir.join("aero-sample.md");
    let sample = Doc {
        name: "aero-sample.md".into(),
        text: fs::read_to_string(&sample_path).expect("corpus/aero-sample.md"),
    };
    let mut notes = Vec::new();
    for d in &extra_dirs {
        collect_md(d, &mut notes);
    }

    println!("# comrak 0.55 vs markdown-rs 1.0 · {}\n", chrono_free_now());
    println!(
        "profile: {} · spec 用例 {} · 真实笔记 {}（{}）\n",
        if cfg!(debug_assertions) { "debug" } else { "release" },
        spec.len(),
        notes.len(),
        extra_dirs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
    );

    let (c, m) = run_group(&spec, 3);
    print_group("CommonMark spec 用例", &c, &m);

    let (c, m) = run_group(std::slice::from_ref(&sample), 20);
    print_group("aero-sample.md（L1/L2 语法样本）", &c, &m);

    if !notes.is_empty() {
        let (c, m) = run_group(&notes, 5);
        print_group("真实笔记", &c, &m);
    }

    print_feature_matrix(&sample);
    print_big_doc(&sample);
}

/// 不引入 chrono：用系统时间秒数做个标记就够了。
fn chrono_free_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix {secs}")
}

/// 报告里每组最多列出的样例条数，`SPIKE_SAMPLES=50` 可调大用于排查。
fn sample_cap() -> usize {
    std::env::var("SPIKE_SAMPLES").ok().and_then(|v| v.parse().ok()).unwrap_or(6)
}
