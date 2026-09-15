//! M0 spike：文档文本模型选型。方案 7.2 是"一份 Rope + Block 树持有 Range<Anchor>"，
//! 要回答：
//! 1. 1 MB / 10 MB 笔记上的编辑延迟（随机插入、连续键入、删除）；
//! 2. 快照是否 O(1)（整篇重解析要在后台线程对快照做，见 ADR 0002 的解析耗时）；
//! 3. byte / char / line 换算的成本（编辑器光标要用）；
//! 4. 给解析器物化整篇 String 的成本（comrak 只吃 &str）；
//! 5. anchor 漂移：用"排序 marker 表 + 每次编辑平移"实现 Range<Anchor>，验证正确性与每次编辑的成本。
//!
//! 候选：ropey 1.6（char 索引，需要 byte→char 换算）、crop 0.4（byte 索引，与 comrak 一致）、
//! 以及 String 基线（说明 rope 到底买到了什么）。Zed 的 rope 不在 crates.io，且 anchor 其实在
//! text::Buffer（CRDT）里而不在 rope crate，这里不测，理由写在 ADR。
//!
//! 用法：spike-rope [--mb 1,10] [--ops 10000]

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use comrak::nodes::NodeValue;
use comrak::{Arena, Options, parse_document};

// ---------------------------------------------------------------------------
// 计数分配器：精确看 rope 的堆占用
// ---------------------------------------------------------------------------

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size(), Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn live_mb() -> f64 {
    LIVE.load(Ordering::Relaxed) as f64 / 1024.0 / 1024.0
}

// ---------------------------------------------------------------------------
// 统一接口
// ---------------------------------------------------------------------------

trait TextBuf: Clone {
    const NAME: &'static str;
    /// 换算是否 O(log n)；String 基线是 O(n)，只跑少量次数
    const FAST_CONVERSIONS: bool;
    /// 是否有 O(log n) 的 char 索引
    const CHAR_INDEX: bool;
    fn from_text(s: &str) -> Self;
    fn len(&self) -> usize;
    fn is_char_boundary(&self, b: usize) -> bool;
    fn insert(&mut self, at: usize, s: &str);
    fn delete(&mut self, r: Range<usize>);
    fn to_text(&self) -> String;
    fn slice_to_string(&self, r: Range<usize>) -> String;
    fn byte_to_line(&self, b: usize) -> usize;
    fn line_to_byte(&self, l: usize) -> usize;
    fn line_count(&self) -> usize;
    fn byte_to_char(&self, b: usize) -> usize;

    fn floor_boundary(&self, mut b: usize) -> usize {
        b = b.min(self.len());
        while b > 0 && !self.is_char_boundary(b) {
            b -= 1;
        }
        b
    }
    fn next_boundary(&self, mut b: usize) -> usize {
        let n = self.len();
        if b >= n {
            return n;
        }
        b += 1;
        while b < n && !self.is_char_boundary(b) {
            b += 1;
        }
        b
    }
}

impl TextBuf for String {
    const NAME: &'static str = "String";
    const FAST_CONVERSIONS: bool = false;
    const CHAR_INDEX: bool = false;
    fn from_text(s: &str) -> Self {
        s.to_string()
    }
    fn len(&self) -> usize {
        String::len(self)
    }
    fn is_char_boundary(&self, b: usize) -> bool {
        str::is_char_boundary(self, b)
    }
    fn insert(&mut self, at: usize, s: &str) {
        self.insert_str(at, s)
    }
    fn delete(&mut self, r: Range<usize>) {
        self.replace_range(r, "")
    }
    fn to_text(&self) -> String {
        self.clone()
    }
    fn slice_to_string(&self, r: Range<usize>) -> String {
        self[r].to_string()
    }
    fn byte_to_line(&self, b: usize) -> usize {
        self.as_bytes()[..b].iter().filter(|&&c| c == b'\n').count()
    }
    fn line_to_byte(&self, l: usize) -> usize {
        if l == 0 {
            return 0;
        }
        let mut seen = 0;
        for (i, &c) in self.as_bytes().iter().enumerate() {
            if c == b'\n' {
                seen += 1;
                if seen == l {
                    return i + 1;
                }
            }
        }
        self.len()
    }
    fn line_count(&self) -> usize {
        self.as_bytes().iter().filter(|&&c| c == b'\n').count() + 1
    }
    fn byte_to_char(&self, b: usize) -> usize {
        self[..b].chars().count()
    }
}

impl TextBuf for ropey::Rope {
    const NAME: &'static str = "ropey 1.6";
    const FAST_CONVERSIONS: bool = true;
    const CHAR_INDEX: bool = true;
    fn from_text(s: &str) -> Self {
        ropey::Rope::from_str(s)
    }
    fn len(&self) -> usize {
        self.len_bytes()
    }
    fn is_char_boundary(&self, b: usize) -> bool {
        // ropey 没有直接的 API：byte→char→byte 回到自身即在边界上
        self.char_to_byte(self.byte_to_char(b)) == b
    }
    fn insert(&mut self, at: usize, s: &str) {
        let c = self.byte_to_char(at);
        ropey::Rope::insert(self, c, s)
    }
    fn delete(&mut self, r: Range<usize>) {
        let cs = self.byte_to_char(r.start);
        let ce = self.byte_to_char(r.end);
        self.remove(cs..ce)
    }
    fn to_text(&self) -> String {
        String::from(self)
    }
    fn slice_to_string(&self, r: Range<usize>) -> String {
        self.byte_slice(r).to_string()
    }
    fn byte_to_line(&self, b: usize) -> usize {
        ropey::Rope::byte_to_line(self, b)
    }
    fn line_to_byte(&self, l: usize) -> usize {
        ropey::Rope::line_to_byte(self, l)
    }
    fn line_count(&self) -> usize {
        self.len_lines()
    }
    fn byte_to_char(&self, b: usize) -> usize {
        ropey::Rope::byte_to_char(self, b)
    }
}

impl TextBuf for ropey2::Rope {
    const NAME: &'static str = "ropey 2.0.0-beta.1 (gpui-base 同款)";
    const FAST_CONVERSIONS: bool = true;
    const CHAR_INDEX: bool = true;
    fn from_text(s: &str) -> Self {
        ropey2::Rope::from_str(s)
    }
    fn len(&self) -> usize {
        ropey2::Rope::len(self)
    }
    fn is_char_boundary(&self, b: usize) -> bool {
        ropey2::Rope::is_char_boundary(self, b)
    }
    fn insert(&mut self, at: usize, s: &str) {
        ropey2::Rope::insert(self, at, s)
    }
    fn delete(&mut self, r: Range<usize>) {
        self.remove(r)
    }
    fn to_text(&self) -> String {
        String::from(self)
    }
    fn slice_to_string(&self, r: Range<usize>) -> String {
        self.slice(r).to_string()
    }
    fn byte_to_line(&self, b: usize) -> usize {
        self.byte_to_line_idx(b, ropey2::LineType::LF)
    }
    fn line_to_byte(&self, l: usize) -> usize {
        self.line_to_byte_idx(l, ropey2::LineType::LF)
    }
    fn line_count(&self) -> usize {
        self.len_lines(ropey2::LineType::LF)
    }
    fn byte_to_char(&self, b: usize) -> usize {
        self.byte_to_char_idx(b)
    }
}

impl TextBuf for crop::Rope {
    const NAME: &'static str = "crop 0.4";
    const FAST_CONVERSIONS: bool = true;
    const CHAR_INDEX: bool = false;
    fn from_text(s: &str) -> Self {
        crop::Rope::from(s)
    }
    fn len(&self) -> usize {
        self.byte_len()
    }
    fn is_char_boundary(&self, b: usize) -> bool {
        crop::Rope::is_char_boundary(self, b)
    }
    fn insert(&mut self, at: usize, s: &str) {
        crop::Rope::insert(self, at, s)
    }
    fn delete(&mut self, r: Range<usize>) {
        crop::Rope::delete(self, r)
    }
    fn to_text(&self) -> String {
        self.to_string()
    }
    fn slice_to_string(&self, r: Range<usize>) -> String {
        self.byte_slice(r).to_string()
    }
    fn byte_to_line(&self, b: usize) -> usize {
        self.line_of_byte(b)
    }
    fn line_to_byte(&self, l: usize) -> usize {
        self.byte_of_line(l)
    }
    fn line_count(&self) -> usize {
        self.line_len()
    }
    fn byte_to_char(&self, b: usize) -> usize {
        // crop 没有 char 索引（只有 byte / line / utf16 / grapheme），按字节切片数 char
        self.byte_slice(..b).chars().count()
    }
}

// ---------------------------------------------------------------------------
// 语料与编辑脚本（确定性）
// ---------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

const ZH: &[&str] = &[
    "笔记", "编辑器", "向量索引", "全文检索", "知识工作区", "同步", "反向链接", "标签", "段落", "标题",
    "代码块", "公式", "渲染", "性能", "延迟", "内存", "线程", "模型", "语义", "分词", "排版", "快捷键",
    "窗口", "侧栏", "命令面板", "光标", "选区", "撤销", "保存", "冲突合并", "版本历史", "里程碑", "架构",
    "接口", "测试", "构建", "发布", "签名", "配置", "用户体验", "交互设计", "需求", "方案", "工具", "平台",
];
const EN: &[&str] = &[
    "the", "rope", "anchor", "block", "tree", "parse", "render", "gpui", "comrak", "zvec", "markdown", "note",
    "vault", "index", "search", "latency", "memory", "snapshot", "undo", "cursor", "selection", "line", "byte",
    "char", "grapheme", "editor", "keystroke", "frame", "budget", "background", "thread", "diff", "id",
];

fn gen_markdown(rng: &mut Rng, target_bytes: usize) -> String {
    let mut s = String::with_capacity(target_bytes + 4096);
    let mut sec = 0;
    let sentence = |rng: &mut Rng, s: &mut String| {
        if rng.below(100) < 60 {
            for _ in 0..(5 + rng.below(8)) {
                if rng.below(100) < 15 {
                    s.push(' ');
                    s.push_str(EN[rng.below(EN.len())]);
                    s.push(' ');
                } else {
                    s.push_str(ZH[rng.below(ZH.len())]);
                }
            }
            if rng.below(100) < 8 {
                s.push_str("[[笔记链接]]");
            }
            s.push('。');
        } else {
            for i in 0..(6 + rng.below(10)) {
                if i > 0 {
                    s.push(' ');
                }
                s.push_str(EN[rng.below(EN.len())]);
            }
            if rng.below(100) < 8 {
                s.push_str(" **strong** and `code`");
            }
            s.push_str(". ");
        }
    };
    while s.len() < target_bytes {
        sec += 1;
        s.push_str(&format!("\n## 第 {sec} 节 {}\n\n", ZH[rng.below(ZH.len())]));
        for _ in 0..(3 + rng.below(6)) {
            match rng.below(10) {
                0 => {
                    s.push_str("```rust\n");
                    for _ in 0..(3 + rng.below(8)) {
                        s.push_str("    let x = rope.insert(at, text); // ");
                        s.push_str(EN[rng.below(EN.len())]);
                        s.push('\n');
                    }
                    s.push_str("```\n\n");
                }
                1 => {
                    for _ in 0..(3 + rng.below(6)) {
                        s.push_str("- ");
                        sentence(rng, &mut s);
                        s.push('\n');
                    }
                    s.push('\n');
                }
                2 => {
                    s.push_str("> [!note]\n> ");
                    sentence(rng, &mut s);
                    s.push_str("\n\n");
                }
                _ => {
                    for _ in 0..(2 + rng.below(4)) {
                        sentence(rng, &mut s);
                    }
                    s.push_str("\n\n");
                }
            }
        }
    }
    s
}

#[derive(Clone)]
enum Op {
    /// 在 frac × len 处插入
    Insert { frac: f64, text: &'static str },
    /// 在 frac × len 处删掉 n 个 char
    Delete { frac: f64, chars: usize },
}

const INSERTS: &[&str] = &["a", "中", " ", "\n", "的", "x", "，", "\n\n", "文档", "rope"];

fn gen_ops(rng: &mut Rng, n: usize) -> Vec<Op> {
    (0..n)
        .map(|_| {
            if rng.below(100) < 65 {
                Op::Insert { frac: rng.unit(), text: INSERTS[rng.below(INSERTS.len())] }
            } else {
                Op::Delete { frac: rng.unit(), chars: 1 + rng.below(3) }
            }
        })
        .collect()
}

/// 把 Op 落到具体字节区间：(start, end, text)。所有候选用同一规则 → 最终文本必须一致。
fn resolve<B: TextBuf>(buf: &B, op: &Op) -> (usize, usize, &'static str) {
    match op {
        Op::Insert { frac, text } => {
            let p = buf.floor_boundary((frac * buf.len() as f64) as usize);
            (p, p, text)
        }
        Op::Delete { frac, chars } => {
            let s = buf.floor_boundary((frac * buf.len() as f64) as usize);
            let mut e = s;
            for _ in 0..*chars {
                e = buf.next_boundary(e);
            }
            (s, e, "")
        }
    }
}

// ---------------------------------------------------------------------------
// Marker 表 = Range<Anchor> 的实现
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bias {
    /// 在锚点位置插入时锚点不动（留在插入文本之前）
    Left,
    /// 在锚点位置插入时锚点跟着挪到插入文本之后
    Right,
}

#[derive(Clone)]
struct MarkerTable {
    pos: Vec<usize>,
    bias: Vec<Bias>,
}

impl MarkerTable {
    /// 输入已按 pos 升序（同 pos 时 Left 在前）
    fn new(items: &[(usize, Bias)]) -> Self {
        let mut items = items.to_vec();
        items.sort_by_key(|&(p, b)| (p, b == Bias::Right));
        MarkerTable { pos: items.iter().map(|x| x.0).collect(), bias: items.iter().map(|x| x.1).collect() }
    }

    /// 文本 [start, end) 被替换成长度 new_len 的内容
    fn apply(&mut self, start: usize, end: usize, new_len: usize) {
        let delta = new_len as isize - (end - start) as isize;
        // 第一个可能受影响的 marker：pos >= start
        let first = self.pos.partition_point(|&p| p < start);
        for i in first..self.pos.len() {
            let p = self.pos[i];
            if p >= end && !(p == start && self.bias[i] == Bias::Left) {
                // 完全在编辑之后（含 Right 偏置的 start 位置）：整体平移
                self.pos[i] = (p as isize + delta) as usize;
            } else if p == start {
                // Left 偏置停在 start；Right 偏置到替换文本之后
                if self.bias[i] == Bias::Right {
                    self.pos[i] = start + new_len;
                }
            } else {
                // 落在被删除的区间内部：按偏置塌缩到替换区间的一端
                self.pos[i] = if self.bias[i] == Bias::Left { start } else { start + new_len };
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

fn pct(v: &mut [f64], q: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * q).round() as usize]
}

fn row(label: &str, samples: &mut Vec<f64>, unit: &str, extra: &str) {
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let p50 = pct(samples, 0.5);
    let p99 = pct(samples, 0.99);
    let max = *samples.last().unwrap();
    println!("  {label:<40} mean {mean:>9.2} {unit}  p50 {p50:>9.2}  p99 {p99:>9.2}  max {max:>10.2}  {extra}");
}

fn hash_text(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
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

/// comrak 解析并收集全部块级节点的字节区间（含容器），作为 anchor 的输入
fn parse_blocks(text: &str) -> (Vec<Range<usize>>, Duration) {
    let mut o = Options::default();
    o.extension.table = true;
    o.extension.strikethrough = true;
    o.extension.tasklist = true;
    o.extension.footnotes = true;
    o.extension.front_matter_delimiter = Some("---".into());
    o.extension.math_dollars = true;
    o.extension.wikilinks_title_after_pipe = true;
    o.extension.alerts = true;
    o.render.sourcepos = true;
    let t = Instant::now();
    let arena = Arena::new();
    let root = parse_document(&arena, text, &o);
    let ls = line_starts(text);
    let mut out = Vec::new();
    for node in root.descendants() {
        let ast = node.data();
        let inline = matches!(
            ast.value,
            NodeValue::Text(_)
                | NodeValue::SoftBreak
                | NodeValue::LineBreak
                | NodeValue::Code(_)
                | NodeValue::Emph
                | NodeValue::Strong
                | NodeValue::Strikethrough
                | NodeValue::Link(_)
                | NodeValue::Image(_)
                | NodeValue::WikiLink(_)
                | NodeValue::Document
                | NodeValue::Highlight
                | NodeValue::Escaped
                | NodeValue::EscapedTag(_)
                | NodeValue::FootnoteReference(_)
                | NodeValue::HtmlInline(_)
                | NodeValue::Raw(_)
        ) || matches!(&ast.value, NodeValue::Math(m) if !m.display_math);
        if inline {
            continue;
        }
        let sp = ast.sourcepos;
        if sp.start.line == 0 || sp.end.line == 0 {
            continue;
        }
        let sl = (sp.start.line - 1).min(ls.len() - 1);
        let el = (sp.end.line - 1).min(ls.len() - 1);
        let s = ls[sl] + sp.start.column.saturating_sub(1);
        let e = (ls[el] + sp.end.column).min(text.len());
        if s < e && text.is_char_boundary(s) && text.is_char_boundary(e) {
            out.push(s..e);
        }
    }
    (out, t.elapsed())
}

// ---------------------------------------------------------------------------
// 基准
// ---------------------------------------------------------------------------

struct Summary {
    final_hash: u64,
}

fn bench<B: TextBuf>(text: &str, ops: &[Op], blocks: &[Range<usize>]) -> Summary {
    println!("\n--- {} ---", B::NAME);
    let mut rng = Rng(99);

    // 构建 + 堆占用
    let before = live_mb();
    let t = Instant::now();
    let mut buf = B::from_text(text);
    let build = t.elapsed();
    println!(
        "  build {:>8.2} ms    heap +{:.2} MB for {:.2} MB text ({:.0}% overhead)    lines {}",
        build.as_secs_f64() * 1e3,
        live_mb() - before,
        text.len() as f64 / 1e6,
        ((live_mb() - before) / (text.len() as f64 / 1024.0 / 1024.0) - 1.0) * 100.0,
        buf.line_count()
    );

    // 快照
    let mut v = Vec::new();
    for _ in 0..1000 {
        let t = Instant::now();
        let snap = buf.clone();
        v.push(us(t.elapsed()));
        std::hint::black_box(&snap);
    }
    row("clone (snapshot for background parse)", &mut v, "µs", "");

    // 随机编辑（同一脚本）
    let mut ins = Vec::new();
    let mut del = Vec::new();
    for op in ops {
        let (s, e, txt) = resolve(&buf, op);
        let t = Instant::now();
        if s == e {
            buf.insert(s, txt);
            ins.push(us(t.elapsed()));
        } else {
            buf.delete(s..e);
            del.push(us(t.elapsed()));
        }
    }
    row("random insert (incl. byte→pos lookup)", &mut ins, "µs", "");
    row("random delete 1–3 chars", &mut del, "µs", "");

    // 连续键入：在文档中部逐字敲 2000 个字
    let mut p = buf.floor_boundary(buf.len() / 2);
    let mut v = Vec::new();
    for i in 0..2000 {
        let ch = if i % 3 == 0 { "中" } else { "a" };
        let t = Instant::now();
        buf.insert(p, ch);
        v.push(us(t.elapsed()));
        p += ch.len();
    }
    row("typing burst (sequential inserts)", &mut v, "µs", "");

    // 换算
    let n = if B::FAST_CONVERSIONS { 100_000 } else { 200 };
    let len = buf.len();
    let lines = buf.line_count();
    let mut b2l = Vec::with_capacity(n);
    let mut l2b = Vec::with_capacity(n);
    let mut b2c = Vec::with_capacity(n);
    for i in 0..n {
        let b = buf.floor_boundary(rng.below(len));
        let l = rng.below(lines);
        let t = Instant::now();
        std::hint::black_box(buf.byte_to_line(b));
        b2l.push(us(t.elapsed()));
        let t = Instant::now();
        std::hint::black_box(buf.line_to_byte(l));
        l2b.push(us(t.elapsed()));
        if B::CHAR_INDEX || i < 200 {
            let t = Instant::now();
            std::hint::black_box(buf.byte_to_char(b));
            b2c.push(us(t.elapsed()));
        }
    }
    let note = if B::FAST_CONVERSIONS { "" } else { "(O(n) scan, n=200)" };
    row("byte→line", &mut b2l, "µs", note);
    row("line→byte", &mut l2b, "µs", note);
    row("byte→char", &mut b2c, "µs", if B::CHAR_INDEX { "" } else { "(无 char 索引，O(n) 数 char，n=200)" });

    // 取一个块的文本（渲染每个块都要）
    let mut v = Vec::new();
    for _ in 0..20_000 {
        let s = buf.floor_boundary(rng.below(len.saturating_sub(400)));
        let e = buf.floor_boundary(s + 300);
        let t = Instant::now();
        std::hint::black_box(buf.slice_to_string(s..e));
        v.push(us(t.elapsed()));
    }
    row("slice 300 B block → String", &mut v, "µs", "");

    // 物化整篇（喂 comrak）
    let mut v = Vec::new();
    let mut last = String::new();
    for _ in 0..20 {
        let t = Instant::now();
        last = buf.to_text();
        v.push(us(t.elapsed()) / 1000.0);
    }
    row("materialize full String (for parser)", &mut v, "ms", "");
    let final_hash = hash_text(&last);

    // anchor 漂移：从原文的块区间建 marker 表，重放同一脚本，验证未被触碰的块文本不变
    {
        let mut fresh = B::from_text(text);
        let mut items = Vec::with_capacity(blocks.len() * 2);
        for r in blocks {
            items.push((r.start, Bias::Left));
            items.push((r.end, Bias::Right));
        }
        let mut markers = MarkerTable::new(&items);
        // 记录每个块对应 marker 下标（按排序后的位置找）
        let mut touched = vec![false; blocks.len()];
        let mut shift = Vec::with_capacity(ops.len());
        // 为了判断"是否触碰"，需要块的当前区间；用一个跟 markers 同步的 (s,e) 表
        let mut cur: Vec<(usize, usize)> = blocks.iter().map(|r| (r.start, r.end)).collect();
        for op in ops {
            let (s, e, txt) = resolve(&fresh, op);
            // 触碰判定：编辑区间与块的开区间 (bs, be) 相交，或删除跨过边界
            for (i, &(bs, be)) in cur.iter().enumerate() {
                let hits = if s == e { s > bs && s < be } else { s < be && e > bs };
                if hits {
                    touched[i] = true;
                }
            }
            if s == e {
                fresh.insert(s, txt);
            } else {
                fresh.delete(s..e);
            }
            let t = Instant::now();
            markers.apply(s, e, txt.len());
            shift.push(us(t.elapsed()));
            // 同步 cur（与 marker 同一规则）
            let delta = txt.len() as isize - (e - s) as isize;
            for c in cur.iter_mut() {
                let mv = |p: usize, bias: Bias| -> usize {
                    if p < s {
                        p
                    } else if p >= e && !(p == s && bias == Bias::Left) {
                        (p as isize + delta) as usize
                    } else if p == s {
                        if bias == Bias::Right { s + txt.len() } else { p }
                    } else if bias == Bias::Left {
                        s
                    } else {
                        s + txt.len()
                    }
                };
                *c = (mv(c.0, Bias::Left), mv(c.1, Bias::Right));
            }
        }
        row(&format!("marker shift per edit ({} markers)", markers.pos.len()), &mut shift, "µs", "");

        // 校验：marker 表与 cur 一致；未触碰块的文本与原文一致（边界插入允许原文为子串）
        let mut ok = 0;
        let mut boundary = 0;
        let mut bad = 0;
        let mut checked = 0;
        for (i, r) in blocks.iter().enumerate() {
            if touched[i] {
                continue;
            }
            checked += 1;
            let (cs, ce) = cur[i];
            let orig = &text[r.clone()];
            let now = fresh.slice_to_string(cs..ce);
            if now == orig {
                ok += 1;
            } else if now.contains(orig) {
                boundary += 1;
            } else {
                bad += 1;
                if bad <= 3 {
                    println!("  MISMATCH block {i}: orig {:?} now {:?}", orig.chars().take(30).collect::<String>(), now.chars().take(30).collect::<String>());
                }
            }
        }
        // marker 表本身和 cur 的对照
        let mut table_ok = true;
        let mut sorted: Vec<(usize, Bias)> = cur.iter().flat_map(|&(s, e)| [(s, Bias::Left), (e, Bias::Right)]).collect();
        sorted.sort_by_key(|&(p, b)| (p, b == Bias::Right));
        for (i, (p, _)) in sorted.iter().enumerate() {
            if markers.pos[i] != *p {
                table_ok = false;
                break;
            }
        }
        println!(
            "  anchor check: {checked} untouched blocks → {ok} identical, {boundary} grown at boundary, {bad} WRONG; marker table == reference: {table_ok}"
        );
        assert_eq!(bad, 0);
        assert!(table_ok);
    }

    Summary { final_hash }
}

fn main() {
    let mut mbs = vec![1usize, 10];
    let mut nops = 10_000usize;
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--mb" => mbs = it.next().unwrap().split(',').map(|x| x.parse().unwrap()).collect(),
            "--ops" => nops = it.next().unwrap().parse().unwrap(),
            other => panic!("unknown arg {other}"),
        }
    }
    println!("== rope spike ==  ops per size: {nops}");

    for mb in mbs {
        let mut rng = Rng(0x1234_5678_9ABC_DEF0 ^ mb as u64);
        let text = gen_markdown(&mut rng, mb * 1024 * 1024);
        let ops = gen_ops(&mut rng, nops);
        let (blocks, parse) = parse_blocks(&text);
        println!(
            "\n===== {mb} MB: {} bytes, {} chars, {} lines, {} block nodes; comrak full parse {:.1} ms =====",
            text.len(),
            text.chars().count(),
            text.bytes().filter(|&b| b == b'\n').count() + 1,
            blocks.len(),
            parse.as_secs_f64() * 1e3
        );

        let a = bench::<String>(&text, &ops, &blocks);
        let b = bench::<ropey::Rope>(&text, &ops, &blocks);
        let b2 = bench::<ropey2::Rope>(&text, &ops, &blocks);
        let c = bench::<crop::Rope>(&text, &ops, &blocks);
        println!(
            "\n  final text identical across candidates: {}",
            [b.final_hash, b2.final_hash, c.final_hash].iter().all(|h| *h == a.final_hash)
        );
    }
}
