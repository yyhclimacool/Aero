//! M0 spike：zvec-rust 能否当 Aero 唯一索引引擎（全文 + 向量）。
//!
//! 对照 project_plan.md 第 19 节预算：
//! - 全文搜索 bench-10k 上查询 p95（预算是 bench-100k < 50 ms，这里 10k 应远低于此）
//! - 增量索引：单篇保存后 < 300 ms 可搜索到 → 这里直接测"写后可见延迟"
//! - 常驻内存：bench-10k < 300 MB（这里只算索引进程部分）
//! - 冷启动：索引打开不阻塞 UI → 测 open + 首次查询
//!
//! 语料按 bench-10k 规格合成：1 万篇、平均 3 KB、中英混排、带 [[wikilink]]，
//! 以 Block 为索引文档（一篇 ≈ 12 个 block）。中文词从 jieba 词典按 Zipf 分布抽取，
//! 保证分词结果与词频分布接近真实文本；全部确定性生成，不依赖外部文件。
//!
//! 用法：spike-zvec-index [--notes N] [--dim D] [--vec-docs N] [--dir PATH] [--keep]
//!                        [--jieba-dict DIR] [--memory-limit MB]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use zvec_rust::*;

// ---------------------------------------------------------------------------
// 参数
// ---------------------------------------------------------------------------

struct Args {
    notes: usize,
    dim: usize,
    vec_docs: usize,
    dir: PathBuf,
    keep: bool,
    jieba_dict: Option<String>,
    memory_limit_mb: u64,
    tokenizer: String,
    tokenizer_params: Option<String>,
    /// 只打开 --dir 下已有的 fts collection 跑查询并报告内存（配合 --keep 的上一轮）
    open_only: bool,
    mmap: bool,
    buffer_mb: u64,
    /// 全量构建时每 N 个 doc flush 一次（0 = 只在末尾 flush），看能否压住峰值内存
    flush_every: usize,
}

fn parse_args() -> Args {
    let mut a = Args {
        notes: 10_000,
        dim: 384,
        vec_docs: 20_000,
        dir: std::env::temp_dir().join("aero-spike-zvec"),
        keep: false,
        jieba_dict: None,
        memory_limit_mb: 0,
        tokenizer: "jieba".into(),
        tokenizer_params: None,
        open_only: false,
        mmap: false,
        buffer_mb: 0,
        flush_every: 0,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--open-only" => a.open_only = true,
            "--flush-every" => a.flush_every = it.next().unwrap().parse().unwrap(),
            "--mmap" => a.mmap = true,
            "--buffer-mb" => a.buffer_mb = it.next().unwrap().parse().unwrap(),
            "--notes" => a.notes = it.next().unwrap().parse().unwrap(),
            "--dim" => a.dim = it.next().unwrap().parse().unwrap(),
            "--vec-docs" => a.vec_docs = it.next().unwrap().parse().unwrap(),
            "--dir" => a.dir = PathBuf::from(it.next().unwrap()),
            "--keep" => a.keep = true,
            "--jieba-dict" => a.jieba_dict = Some(it.next().unwrap()),
            "--memory-limit" => a.memory_limit_mb = it.next().unwrap().parse().unwrap(),
            "--tokenizer" => a.tokenizer = it.next().unwrap(),
            "--tokenizer-params" => a.tokenizer_params = Some(it.next().unwrap()),
            other => panic!("unknown arg {other}"),
        }
    }
    a
}

// ---------------------------------------------------------------------------
// 确定性语料
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
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
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
    fn unit_vec(&mut self, dim: usize) -> Vec<f32> {
        let mut v: Vec<f32> = (0..dim).map(|_| self.unit() as f32 - 0.5).collect();
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
        v.iter_mut().for_each(|x| *x /= n);
        v
    }
}

/// 按 Zipf(s) 分布抽取词表下标：rank 越小越常见。
struct Zipf {
    cdf: Vec<f64>,
}

impl Zipf {
    fn new(n: usize, s: f64) -> Self {
        let mut cdf = Vec::with_capacity(n);
        let mut acc = 0.0;
        for r in 0..n {
            acc += 1.0 / ((r + 1) as f64).powf(s);
            cdf.push(acc);
        }
        let total = acc;
        cdf.iter_mut().for_each(|x| *x /= total);
        Zipf { cdf }
    }
    fn sample(&self, rng: &mut Rng) -> usize {
        let u = rng.unit();
        self.cdf.partition_point(|&c| c < u).min(self.cdf.len() - 1)
    }
}

struct Vocab {
    zh: Vec<String>,
    en: Vec<String>,
    zh_zipf: Zipf,
    en_zipf: Zipf,
}

/// 领域词，保证查询里有可读的词，也放在英文词表最前面（最高频）。
const EN_SEED: &[&str] = &[
    "note", "editor", "vector", "index", "fulltext", "search", "database", "knowledge", "workspace", "file",
    "sync", "link", "backlink", "tag", "paragraph", "heading", "list", "code", "formula", "diagram", "render",
    "performance", "latency", "memory", "disk", "cache", "thread", "async", "network", "protocol", "model",
    "embedding", "semantic", "tokenizer", "layout", "font", "theme", "color", "shortcut", "keyboard", "mouse",
    "window", "sidebar", "panel", "command", "result", "cursor", "selection", "undo", "redo", "save",
    "conflict", "merge", "version", "history", "plan", "task", "project", "milestone", "risk", "decision",
    "architecture", "module", "interface", "test", "benchmark", "regression", "build", "release", "signing",
    "notarization", "package", "install", "update", "config", "rust", "gpui", "comrak", "zvec", "markdown",
    "wikilink", "callout", "directive", "attribute", "block", "the", "and", "with", "for", "from", "into",
    "about", "because", "should", "this", "that", "have", "will", "can", "not", "are", "was", "were",
];

impl Vocab {
    fn load(jieba_dict_dir: &str) -> Self {
        // jieba.dict.utf8 每行 "词 词频 词性"；取 2~4 字纯汉字词、按词频排序取前 30k
        let text = std::fs::read_to_string(Path::new(jieba_dict_dir).join("jieba.dict.utf8")).expect("read jieba dict");
        let mut words: Vec<(u64, &str)> = text
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                let w = it.next()?;
                let f: u64 = it.next()?.parse().ok()?;
                let n = w.chars().count();
                if (2..=4).contains(&n) && w.chars().all(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)) {
                    Some((f, w))
                } else {
                    None
                }
            })
            .collect();
        words.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        let zh: Vec<String> = words.iter().take(30_000).map(|(_, w)| w.to_string()).collect();

        // 英文：领域词在前，其后是音节拼出的伪词，共 8k
        let mut en: Vec<String> = EN_SEED.iter().map(|s| s.to_string()).collect();
        let cons = ["b", "c", "d", "f", "g", "h", "k", "l", "m", "n", "p", "r", "s", "t", "v", "w", "z", "st", "tr", "pl"];
        let vow = ["a", "e", "i", "o", "u", "ai", "ou", "ea"];
        let mut rng = Rng(7);
        while en.len() < 8_000 {
            let syl = 2 + rng.below(2);
            let mut w = String::new();
            for _ in 0..syl {
                w.push_str(cons[rng.below(cons.len())]);
                w.push_str(vow[rng.below(vow.len())]);
            }
            if !en.contains(&w) {
                en.push(w);
            }
        }
        Vocab {
            zh_zipf: Zipf::new(zh.len(), 1.05),
            en_zipf: Zipf::new(en.len(), 1.05),
            zh,
            en,
        }
    }
    fn zh(&self, rng: &mut Rng) -> &str {
        &self.zh[self.zh_zipf.sample(rng)]
    }
    fn en(&self, rng: &mut Rng) -> &str {
        &self.en[self.en_zipf.sample(rng)]
    }
    /// 中频词（rank 1k–10k）：更像用户真正会搜的具体词
    fn zh_mid(&self, rng: &mut Rng) -> &str {
        &self.zh[1_000 + rng.below(9_000)]
    }
}

#[derive(Clone)]
struct Block {
    pk: String,
    note_id: String,
    ord: i32,
    title: String,
    body: String,
}

fn note_id(i: usize) -> String {
    format!("note-{i:05}")
}

fn gen_sentence(v: &Vocab, rng: &mut Rng, notes: usize) -> String {
    let mut s = String::new();
    if rng.chance(70) {
        // 中文句：词直接相连，偶尔夹英文术语
        let n = 6 + rng.below(10);
        for _ in 0..n {
            if rng.chance(12) {
                s.push(' ');
                s.push_str(v.en(rng));
                s.push(' ');
            } else {
                s.push_str(v.zh(rng));
            }
        }
        if rng.chance(10) {
            s.push_str(&format!("[[{}]]", note_id(rng.below(notes))));
        }
        s.push('。');
    } else {
        let n = 8 + rng.below(12);
        for i in 0..n {
            if i > 0 {
                s.push(' ');
            }
            s.push_str(v.en(rng));
        }
        if rng.chance(10) {
            s.push_str(&format!(" [[{}]]", note_id(rng.below(notes))));
        }
        s.push_str(". ");
    }
    s
}

fn gen_block(v: &Vocab, rng: &mut Rng, notes: usize, min_bytes: usize) -> String {
    let mut body = String::new();
    while body.len() < min_bytes {
        body.push_str(&gen_sentence(v, rng, notes));
    }
    body
}

fn gen_note(v: &Vocab, rng: &mut Rng, notes: usize, i: usize) -> Vec<Block> {
    let id = note_id(i);
    let title = format!("{}{}", v.zh(rng), v.zh(rng));
    // 8..16 个 block，每个 ≥ 200 字节 → 一篇约 3 KB
    let n = 8 + rng.below(9);
    (0..n)
        .map(|ord| Block {
            pk: format!("{id}#{ord}"),
            note_id: id.clone(),
            ord: ord as i32,
            title: title.clone(),
            body: gen_block(v, rng, notes, 200),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn report(name: &str, samples: &[f64], extra: &str) {
    let mut v = samples.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
    println!(
        "  {name:<36} n={:<4} p50 {:>7.2} ms  p95 {:>7.2} ms  p99 {:>7.2} ms  max {:>7.2} ms  {extra}",
        v.len(),
        at(0.50),
        at(0.95),
        at(0.99),
        v.last().unwrap()
    );
}

fn rss_mb() -> f64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().unwrap_or(0.0) / 1024.0
}

fn dir_size_mb(p: &Path) -> f64 {
    fn walk(p: &Path) -> u64 {
        if p.is_file() {
            return p.metadata().map(|m| m.len()).unwrap_or(0);
        }
        std::fs::read_dir(p)
            .map(|rd| rd.flatten().map(|e| walk(&e.path())).sum())
            .unwrap_or(0)
    }
    walk(p) as f64 / 1024.0 / 1024.0
}

fn make_doc(b: &Block) -> Doc {
    let mut d = Doc::new().unwrap();
    d.set_pk(&b.pk);
    d.add_string("note_id", &b.note_id).unwrap();
    d.add_i32("block_ord", b.ord).unwrap();
    d.add_string("title", &b.title).unwrap();
    d.add_string("body", &b.body).unwrap();
    d
}

fn fts_query(field: &str, text: &str, topk: i32, filter: Option<&str>) -> SearchQuery {
    let mut fts = Fts::new().unwrap();
    fts.set_match_string(text).unwrap();
    let mut q = SearchQuery::fts(field, &fts, topk).unwrap();
    q.set_output_fields(&["note_id", "block_ord"]).unwrap();
    if let Some(f) = filter {
        q.set_filter(f).unwrap();
    }
    q
}

fn run_queries(
    col: &Collection,
    rng: &mut Rng,
    name: &str,
    n: usize,
    mk: &mut dyn FnMut(&mut Rng) -> (String, Option<String>),
) {
    let mut lat = Vec::with_capacity(n);
    let mut hits = 0usize;
    let mut empty = 0usize;
    for _ in 0..n {
        let (text, filter) = mk(rng);
        let q = fts_query("body", &text, 20, filter.as_deref());
        let t = Instant::now();
        let r = col.query(&q).unwrap();
        lat.push(ms(t.elapsed()));
        hits += r.len();
        if r.is_empty() {
            empty += 1;
        }
    }
    report(name, &lat, &format!("avg hits {:.1}, empty {empty}", hits as f64 / n as f64));
}

/// 轮询直到查询命中 pk 或超时，返回耗时。
fn wait_visible(col: &Collection, q: &SearchQuery, want_pk: &str, timeout: Duration) -> Option<Duration> {
    let t0 = Instant::now();
    loop {
        let hits = col.query(q).unwrap();
        if hits.iter().any(|d| d.get_pk() == Some(want_pk)) {
            return Some(t0.elapsed());
        }
        if t0.elapsed() > timeout {
            return None;
        }
        std::thread::sleep(Duration::from_micros(200));
    }
}

fn first_error(r: &WriteResult) -> String {
    r.results
        .iter()
        .find(|x| !x.success)
        .map(|x| format!("{:?}: {}", x.code, x.message))
        .unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum SaveStrategy {
    /// delete_by_filter("note_id = ...") + insert
    DeleteByFilter,
    /// delete(old pks) + insert
    DeleteByPk,
    /// upsert(new blocks) + delete(pks no longer present)
    Upsert,
}

struct SaveTiming {
    del: f64,
    write: f64,
    visible: f64,
    total: f64,
}

/// 模拟"保存一篇笔记"：把 note 的全部 block 换成 fresh，然后轮询直到新 block 可被搜到。
fn save_note(col: &Collection, strategy: SaveStrategy, old: &[Block], fresh: &[Block], probe_pk: &str, probe: &str) -> std::result::Result<SaveTiming, String> {
    let old_pks: Vec<&str> = old.iter().map(|b| b.pk.as_str()).collect();
    let docs: Vec<Doc> = fresh.iter().map(make_doc).collect();
    let refs: Vec<&Doc> = docs.iter().collect();
    let nid = &old[0].note_id;

    let t_all = Instant::now();
    let mut del = 0.0;
    let t = Instant::now();
    let write_r = match strategy {
        SaveStrategy::DeleteByFilter => {
            col.delete_by_filter(&format!("note_id = '{nid}'")).unwrap();
            del = ms(t.elapsed());
            let t = Instant::now();
            let r = col.insert(&refs).unwrap();
            (r, ms(t.elapsed()))
        }
        SaveStrategy::DeleteByPk => {
            let d = col.delete(&old_pks).unwrap();
            assert_eq!(d.error_count, 0, "delete: {}", first_error(&d));
            del = ms(t.elapsed());
            let t = Instant::now();
            let r = col.insert(&refs).unwrap();
            (r, ms(t.elapsed()))
        }
        SaveStrategy::Upsert => {
            let r = col.upsert(&refs).unwrap();
            let write = ms(t.elapsed());
            let fresh_pks: Vec<&str> = fresh.iter().map(|b| b.pk.as_str()).collect();
            let gone: Vec<&str> = old_pks.iter().copied().filter(|p| !fresh_pks.contains(p)).collect();
            if !gone.is_empty() {
                let t = Instant::now();
                let d = col.delete(&gone).unwrap();
                assert_eq!(d.error_count, 0, "delete: {}", first_error(&d));
                del = ms(t.elapsed());
            }
            (r, write)
        }
    };
    let (r, write) = write_r;
    if r.error_count > 0 {
        return Err(format!("write failed ({} of {}): {}", r.error_count, r.results.len(), first_error(&r)));
    }

    let q = fts_query("body", probe, 5, None);
    let visible = wait_visible(col, &q, probe_pk, Duration::from_secs(5)).ok_or_else(|| "never visible within 5 s".to_string())?;
    Ok(SaveTiming { del, write, visible: ms(visible), total: ms(t_all.elapsed()) })
}

fn main() {
    let args = parse_args();
    println!("== zvec-rust spike ==");
    println!("zvec C library version: {}", version());

    // 0. 初始化
    if args.memory_limit_mb > 0 {
        let cfg = ConfigBuilder::new().memory_limit(args.memory_limit_mb * 1024 * 1024).build();
        initialize(Some(&cfg)).expect("initialize");
        println!("memory_limit = {} MB", args.memory_limit_mb);
    } else {
        initialize(None).expect("initialize");
    }
    if let Some(dir) = &args.jieba_dict {
        set_default_jieba_dict_dir(dir).unwrap();
    }
    let jieba = get_default_jieba_dict_dir();
    println!("jieba dict dir: {}", if jieba.is_empty() { "<none>" } else { &jieba });
    assert!(!jieba.is_empty(), "jieba dict not discovered; pass --jieba-dict");
    println!("RSS after initialize: {:.1} MB", rss_mb());

    let mut opts = CollectionOptions::new().unwrap();
    opts.set_enable_mmap(args.mmap).unwrap();
    if args.buffer_mb > 0 {
        opts.set_max_buffer_size(args.buffer_mb * 1024 * 1024).unwrap();
    }
    println!("collection options: mmap={} max_buffer_size={} MB", opts.enable_mmap(), opts.max_buffer_size() / 1024 / 1024);
    let fts_dir = args.dir.join("fts");
    let vec_dir = args.dir.join("vec");

    if args.open_only {
        // 干净进程里的读路径内存：这才是常驻内存预算该对照的数字
        let vocab = Vocab::load(&jieba);
        let mut rng = Rng(42);
        println!("\n[open-only] {} (RSS before open {:.1} MB)", fts_dir.display(), rss_mb());
        let t = Instant::now();
        let col = Collection::open(fts_dir.to_str().unwrap(), Some(&opts)).unwrap();
        println!("  open {:.1} ms, doc_count={}, RSS {:.1} MB", ms(t.elapsed()), col.stats().unwrap().doc_count, rss_mb());
        let v = &vocab;
        run_queries(&col, &mut rng, "zh frequent word", 200, &mut |r| (v.zh(r).to_string(), None));
        println!("  RSS after 200 queries: {:.1} MB", rss_mb());
        run_queries(&col, &mut rng, "zh two words", 200, &mut |r| (format!("{} {}", v.zh(r), v.zh_mid(r)), None));
        run_queries(&col, &mut rng, "en word", 200, &mut |r| (v.en(r).to_string(), None));
        println!("  RSS after 600 queries: {:.1} MB", rss_mb());
        // 一次小写入 + 可见性，确认读写混合下的内存
        let mut b = Block { pk: "open-only#0".into(), note_id: "open-only".into(), ord: 0, title: "t".into(), body: String::new() };
        b.body = format!("{} openonlytoken", gen_block(v, &mut rng, 100, 200));
        let d = make_doc(&b);
        let t = Instant::now();
        col.upsert(&[&d]).unwrap();
        wait_visible(&col, &fts_query("body", "openonlytoken", 5, None), &b.pk, Duration::from_secs(5)).unwrap();
        println!("  upsert 1 doc → visible {:.2} ms, RSS {:.1} MB", ms(t.elapsed()), rss_mb());
        col.delete(&[b.pk.as_str()]).unwrap();
        col.close().unwrap();
        println!("  RSS after close: {:.1} MB", rss_mb());
        shutdown().unwrap();
        return;
    }

    if args.dir.exists() {
        std::fs::remove_dir_all(&args.dir).unwrap();
    }
    std::fs::create_dir_all(&args.dir).unwrap();

    // 1. 语料
    let t = Instant::now();
    let vocab = Vocab::load(&jieba);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let notes: Vec<Vec<Block>> = (0..args.notes).map(|i| gen_note(&vocab, &mut rng, args.notes, i)).collect();
    let total_blocks: usize = notes.iter().map(|n| n.len()).sum();
    let total_bytes: usize = notes.iter().flatten().map(|b| b.body.len()).sum();
    println!(
        "\n[corpus] {} notes, {} blocks, {:.1} MB body, avg {:.0} B/note, vocab zh {} / en {}, gen {:.0} ms",
        args.notes,
        total_blocks,
        total_bytes as f64 / 1e6,
        total_bytes as f64 / args.notes as f64,
        vocab.zh.len(),
        vocab.en.len(),
        ms(t.elapsed())
    );
    println!("  sample: {}", notes[7][2].body.chars().take(70).collect::<String>());
    let rss_corpus = rss_mb();
    println!("  RSS with corpus in memory: {rss_corpus:.1} MB");

    // 2. FTS collection 全量构建
    let fts_params = || IndexParams::fts(Some(&args.tokenizer), Some(&["lowercase"]), args.tokenizer_params.as_deref()).unwrap();
    println!("\n[schema] body: FTS tokenizer={} filters=[lowercase] extra={}", args.tokenizer, args.tokenizer_params.as_deref().unwrap_or("-"));
    let schema = CollectionSchema::builder("aero_fts")
        .add_field(FieldSchema::new("note_id", DataType::String, false, 0).unwrap())
        .add_field(FieldSchema::new("block_ord", DataType::Int32, false, 0).unwrap())
        .add_field(FieldSchema::new("title", DataType::String, false, 0).unwrap())
        .add_indexed_field("body", DataType::String, fts_params())
        .build()
        .expect("schema");
    let t = Instant::now();
    let col = Collection::create_and_open(fts_dir.to_str().unwrap(), &schema, Some(&opts)).expect("create");
    println!("  create_and_open {:.0} ms, RSS {:.1} MB", ms(t.elapsed()), rss_mb());

    println!("\n[build] FTS(jieba) full index");
    let t = Instant::now();
    let mut batch_ms = Vec::new();
    let mut pending: Vec<Doc> = Vec::with_capacity(512);
    let mut inserted = 0u64;
    let mut peak_rss = 0.0f64;
    for b in notes.iter().flatten() {
        pending.push(make_doc(b));
        if pending.len() == 512 {
            let refs: Vec<&Doc> = pending.iter().collect();
            let tb = Instant::now();
            let r = col.insert(&refs).unwrap();
            if batch_ms.is_empty() {
                println!("  first insert batch {:.0} ms (jieba dict load happens here?), RSS {:.1} MB", ms(tb.elapsed()), rss_mb());
            }
            batch_ms.push(ms(tb.elapsed()));
            inserted += r.success_count;
            assert_eq!(r.error_count, 0, "insert errors: {}", first_error(&r));
            pending.clear();
            if args.flush_every > 0 && inserted as usize % args.flush_every < 512 {
                col.flush().unwrap();
                peak_rss = peak_rss.max(rss_mb());
            }
        }
    }
    if args.flush_every > 0 {
        println!("  flush every {} docs: peak RSS seen {:.1} MB", args.flush_every, peak_rss);
    }
    if !pending.is_empty() {
        let refs: Vec<&Doc> = pending.iter().collect();
        inserted += col.insert(&refs).unwrap().success_count;
    }
    let build = t.elapsed();
    let rss_built = rss_mb();
    let t = Instant::now();
    col.flush().unwrap();
    let flush = t.elapsed();
    println!(
        "  inserted {inserted} docs in {:.2} s ({:.0} docs/s, {:.1} MB/s), flush {:.0} ms",
        build.as_secs_f64(),
        inserted as f64 / build.as_secs_f64(),
        total_bytes as f64 / 1e6 / build.as_secs_f64(),
        ms(flush)
    );
    report("insert batch(512)", &batch_ms, "");
    println!(
        "  doc_count={} on-disk {:.1} MB  RSS before flush {:.1} MB, after flush {:.1} MB (corpus itself {:.1} MB)",
        col.stats().unwrap().doc_count,
        dir_size_mb(&fts_dir),
        rss_built,
        rss_mb(),
        rss_corpus
    );
    let t = Instant::now();
    col.optimize().unwrap();
    println!("  optimize {:.0} ms → on-disk {:.1} MB, RSS {:.1} MB", ms(t.elapsed()), dir_size_mb(&fts_dir), rss_mb());

    // 3. 查询延迟
    println!("\n[query] FTS topk=20, output_fields=[note_id, block_ord]");
    let mut rng = Rng(42);
    let notes_n = args.notes;
    let v = &vocab;
    run_queries(&col, &mut rng, "zh frequent word (Zipf)", 200, &mut |r| (v.zh(r).to_string(), None));
    run_queries(&col, &mut rng, "zh mid-freq word (rank 1k-10k)", 200, &mut |r| (v.zh_mid(r).to_string(), None));
    run_queries(&col, &mut rng, "zh two words", 200, &mut |r| (format!("{} {}", v.zh(r), v.zh_mid(r)), None));
    run_queries(&col, &mut rng, "zh phrase concatenated", 100, &mut |r| (format!("{}{}", v.zh(r), v.zh(r)), None));
    run_queries(&col, &mut rng, "en word (Zipf)", 200, &mut |r| (v.en(r).to_string(), None));
    run_queries(&col, &mut rng, "mixed zh+en", 100, &mut |r| (format!("{} {}", v.zh(r), v.en(r)), None));
    run_queries(&col, &mut rng, "zh word + filter note_id", 100, &mut |r| {
        let nid = note_id(r.below(notes_n));
        (v.zh(r).to_string(), Some(format!("note_id = '{nid}'")))
    });
    run_queries(&col, &mut rng, "hottest words 我们 可以", 50, &mut |_| ("我们 可以".to_string(), None));

    // 相关性 sanity：top1 的 body 必须包含查询词
    {
        let (word, r) = loop {
            let word = vocab.zh_mid(&mut rng).to_string();
            let mut fts = Fts::new().unwrap();
            fts.set_match_string(&word).unwrap();
            let q = SearchQuery::fts("body", &fts, 5).unwrap();
            let r = col.query(&q).unwrap();
            if !r.is_empty() {
                break (word, r);
            }
        };
        let top = r.first().expect("expected hits");
        let body = top.get_string("body").unwrap().unwrap();
        println!("  relevance: '{word}' top1 pk={} score={:.3} contains={}", top.get_pk().unwrap(), top.get_score(), body.contains(&word));
        assert!(body.contains(&word));
    }
    // 大小写：语料里英文全小写，大写查询要靠 lowercase filter 命中
    {
        let lower = col.query(&fts_query("body", "gpui markdown", 20, None)).unwrap().len();
        let upper = col.query(&fts_query("body", "GPUI Markdown", 20, None)).unwrap().len();
        println!("  case-insensitive: 'gpui markdown' → {lower} hits, 'GPUI Markdown' → {upper} hits");
    }

    // 4. 写后可见延迟：三种"保存一篇笔记"策略
    println!("\n[visibility] save note (~12 blocks) then poll until the new block is searchable; budget < 300 ms");
    let mut current: Vec<Vec<Block>> = notes.clone();
    let rounds = 30;
    for strategy in [SaveStrategy::DeleteByFilter, SaveStrategy::DeleteByPk, SaveStrategy::Upsert] {
        let (mut del, mut write, mut vis, mut total) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut failed: Option<String> = None;
        for round in 0..rounds {
            let ni = rng.below(args.notes);
            let mut fresh = gen_note(&vocab, &mut rng, args.notes, ni);
            let probe = format!("visibletoken{strategy:?}{round}").to_lowercase();
            fresh[3].body.push_str(&format!(" {probe} "));
            let probe_pk = fresh[3].pk.clone();
            match save_note(&col, strategy, &current[ni], &fresh, &probe_pk, &probe) {
                Ok(st) => {
                    del.push(st.del);
                    write.push(st.write);
                    vis.push(st.visible);
                    total.push(st.total);
                    current[ni] = fresh;
                }
                Err(e) => {
                    failed = Some(e);
                    // 该笔记现在处于"部分写入"状态，用 upsert 拉回到 fresh，保持 current 一致
                    let docs: Vec<Doc> = fresh.iter().map(make_doc).collect();
                    let refs: Vec<&Doc> = docs.iter().collect();
                    col.upsert(&refs).unwrap();
                    let fresh_pks: Vec<&str> = fresh.iter().map(|b| b.pk.as_str()).collect();
                    let gone: Vec<&str> = current[ni].iter().map(|b| b.pk.as_str()).filter(|p| !fresh_pks.contains(p)).collect();
                    if !gone.is_empty() {
                        col.delete(&gone).unwrap();
                    }
                    current[ni] = fresh;
                    break;
                }
            }
        }
        println!("  -- {strategy:?}");
        if let Some(e) = failed {
            println!("  UNUSABLE: {e}");
        }
        if !total.is_empty() {
            report("delete part", &del, "");
            report("write part", &write, "");
            report("poll until visible", &vis, "");
            report("save→searchable total", &total, "");
        }
    }
    // 删除确实生效：旧 block 用 filter 查不到、fetch 不到
    {
        let ni = rng.below(args.notes);
        let old_pks: Vec<&str> = current[ni].iter().map(|b| b.pk.as_str()).collect();
        col.delete_by_filter(&format!("note_id = '{}'", note_id(ni))).unwrap();
        let fetched = col.fetch(&old_pks).unwrap().len();
        let q = fts_query("body", vocab.zh(&mut rng), 50, Some(&format!("note_id = '{}'", note_id(ni))));
        let hits = col.query(&q).unwrap().len();
        println!("  after delete_by_filter: fetch(old pks) -> {fetched}, filtered query hits -> {hits} (expect 0 / 0)");
        let docs: Vec<Doc> = current[ni].iter().map(make_doc).collect();
        let refs: Vec<&Doc> = docs.iter().collect();
        let r = col.insert(&refs).unwrap();
        println!("  re-insert same pks after delete_by_filter: ok {} / err {} {}", r.success_count, r.error_count, first_error(&r));
        if r.error_count > 0 {
            let d = col.delete(&old_pks).unwrap();
            println!("  delete(pks) on the zombies: ok {} / err {} {}", d.success_count, d.error_count, first_error(&d));
            let t = Instant::now();
            col.flush().unwrap();
            let r = col.insert(&refs).unwrap();
            println!("  after flush ({:.0} ms): insert ok {} / err {}", ms(t.elapsed()), r.success_count, r.error_count);
            if r.error_count > 0 {
                let t = Instant::now();
                col.optimize().unwrap();
                let r = col.insert(&refs).unwrap();
                println!("  after optimize ({:.0} ms): insert ok {} / err {}", ms(t.elapsed()), r.success_count, r.error_count);
                if r.error_count > 0 {
                    let r = col.upsert(&refs).unwrap();
                    println!("  upsert instead: ok {} / err {} {}", r.success_count, r.error_count, first_error(&r));
                    let got = col.fetch(&old_pks).unwrap().len();
                    println!("  fetch after upsert: {got} of {}", old_pks.len());
                }
            }
        }
    }
    // zh unseen token recall（jieba 会把生词切碎）
    {
        let ni = rng.below(args.notes);
        let mut fresh = current[ni].clone();
        let token = "可见令牌零号";
        fresh[0].body.push_str(&format!("{token}。"));
        let docs: Vec<Doc> = fresh.iter().map(make_doc).collect();
        let refs: Vec<&Doc> = docs.iter().collect();
        col.upsert(&refs).unwrap();
        let q = fts_query("body", token, 5, None);
        let hit = wait_visible(&col, &q, &fresh[0].pk, Duration::from_secs(2)).is_some();
        println!("  zh unseen token '{token}' recalled via match_string: {hit}");
    }

    // 高频小写入：模拟连续编辑，200 次保存不 flush，看查询延迟是否退化
    println!("\n[churn] 200 x save (delete_by_pk + insert) without flush, then query latency again");
    let t = Instant::now();
    for _ in 0..200 {
        let ni = rng.below(args.notes);
        let old_pks: Vec<&str> = current[ni].iter().map(|b| b.pk.as_str()).collect();
        col.delete(&old_pks).unwrap();
        let fresh = gen_note(&vocab, &mut rng, args.notes, ni);
        let docs: Vec<Doc> = fresh.iter().map(make_doc).collect();
        let refs: Vec<&Doc> = docs.iter().collect();
        let r = col.insert(&refs).unwrap();
        assert_eq!(r.error_count, 0, "{}", first_error(&r));
        current[ni] = fresh;
    }
    println!("  churn took {:.0} ms total, doc_count={} (expect ≈ {})", ms(t.elapsed()), col.stats().unwrap().doc_count, current.iter().map(|n| n.len()).sum::<usize>());
    run_queries(&col, &mut rng, "zh frequent word (after churn)", 200, &mut |r| (v.zh(r).to_string(), None));
    run_queries(&col, &mut rng, "zh mid-freq word (after churn)", 200, &mut |r| (v.zh_mid(r).to_string(), None));
    let t = Instant::now();
    col.flush().unwrap();
    println!("  flush after churn {:.0} ms, RSS {:.1} MB, on-disk {:.1} MB", ms(t.elapsed()), rss_mb(), dir_size_mb(&fts_dir));

    // 5. 冷打开
    println!("\n[cold open] close, reopen, first query");
    col.close().unwrap();
    drop(notes);
    println!("  RSS after close (corpus copy still held): {:.1} MB", rss_mb());
    let t = Instant::now();
    let col = Collection::open(fts_dir.to_str().unwrap(), Some(&opts)).unwrap();
    let open = t.elapsed();
    let q = fts_query("body", "架构 决策", 20, None);
    let t = Instant::now();
    let r = col.query(&q).unwrap();
    println!("  open {:.1} ms, first query {:.2} ms ({} hits), doc_count={}, RSS {:.1} MB", ms(open), ms(t.elapsed()), r.len(), col.stats().unwrap().doc_count, rss_mb());
    run_queries(&col, &mut rng, "zh frequent word (reopened)", 200, &mut |r| (v.zh(r).to_string(), None));
    println!("  RSS after 200 queries: {:.1} MB", rss_mb());
    col.close().unwrap();

    // 6. 向量 + FTS 混合（M3 才用，这里只验证 API、写后可见与量级）
    println!("\n[vector] dim={} docs={} HNSW(cosine, m=16, ef_c=200) + FTS body, MultiQuery RRF", args.dim, args.vec_docs);
    let schema = CollectionSchema::builder("aero_hybrid")
        .add_field(FieldSchema::new("note_id", DataType::String, false, 0).unwrap())
        .add_indexed_field("body", DataType::String, fts_params())
        .add_vector_field("embedding", DataType::VectorFp32, args.dim as u32, IndexParams::hnsw(MetricType::Cosine, 16, 200).unwrap())
        .build()
        .unwrap();
    let vcol = Collection::create_and_open(vec_dir.to_str().unwrap(), &schema, Some(&opts)).unwrap();
    let rss0 = rss_mb();
    let t = Instant::now();
    let mut vecs: Vec<(String, Vec<f32>)> = Vec::new();
    let mut pending: Vec<Doc> = Vec::new();
    for b in current.iter().flatten().take(args.vec_docs) {
        let v = rng.unit_vec(args.dim);
        let mut d = Doc::new().unwrap();
        d.set_pk(&b.pk);
        d.add_string("note_id", &b.note_id).unwrap();
        d.add_string("body", &b.body).unwrap();
        d.add_vector_f32("embedding", &v).unwrap();
        if vecs.len() < 200 {
            vecs.push((b.pk.clone(), v));
        }
        pending.push(d);
        if pending.len() == 256 {
            let refs: Vec<&Doc> = pending.iter().collect();
            vcol.insert(&refs).unwrap();
            pending.clear();
        }
    }
    if !pending.is_empty() {
        let refs: Vec<&Doc> = pending.iter().collect();
        vcol.insert(&refs).unwrap();
    }
    let build = t.elapsed();
    let t = Instant::now();
    vcol.flush().unwrap();
    let flush = t.elapsed();
    let t = Instant::now();
    vcol.optimize().unwrap();
    println!(
        "  inserted {} docs in {:.2} s ({:.0} docs/s), flush {:.0} ms, optimize {:.0} ms, RSS +{:.1} MB, on-disk {:.1} MB",
        args.vec_docs,
        build.as_secs_f64(),
        args.vec_docs as f64 / build.as_secs_f64(),
        ms(flush),
        ms(t.elapsed()),
        rss_mb() - rss0,
        dir_size_mb(&vec_dir)
    );

    // 向量 topk=10；自查：查自己的向量应排第一
    let mut lat = Vec::new();
    let mut self_top1 = 0;
    for (pk, v) in &vecs {
        let mut q = SearchQuery::new("embedding", v, 10).unwrap();
        q.set_output_fields(&["note_id"]).unwrap();
        let t = Instant::now();
        let r = vcol.query(&q).unwrap();
        lat.push(ms(t.elapsed()));
        if r.first().and_then(|d| d.get_pk()) == Some(pk.as_str()) {
            self_top1 += 1;
        }
    }
    report("vector topk=10", &lat, &format!("self@1 {self_top1}/{}", vecs.len()));

    // 混合：向量子查询 + FTS 子查询，RRF
    let mut lat = Vec::new();
    let mut hits = 0;
    for (_, v) in vecs.iter().take(100) {
        let mut mq = MultiQuery::new().unwrap();
        let mut s1 = SubQuery::new().unwrap();
        s1.set_field_name("embedding").unwrap();
        s1.set_query_vector(v).unwrap();
        s1.set_num_candidates(50).unwrap();
        let mut fts = Fts::new().unwrap();
        fts.set_match_string(&format!("{} {}", vocab.zh(&mut rng), vocab.en(&mut rng))).unwrap();
        let mut s2 = SubQuery::new().unwrap();
        s2.set_field_name("body").unwrap();
        s2.set_fts(&fts).unwrap();
        s2.set_num_candidates(50).unwrap();
        mq.add_sub_query(&s1).unwrap();
        mq.add_sub_query(&s2).unwrap();
        mq.set_topk(10).unwrap();
        mq.set_rerank_rrf(60).unwrap();
        mq.set_output_fields(&["note_id"]).unwrap();
        let t = Instant::now();
        let r = vcol.multi_query(&mq).unwrap();
        lat.push(ms(t.elapsed()));
        hits += r.len();
    }
    report("hybrid MultiQuery RRF topk=10", &lat, &format!("avg hits {:.1}", hits as f64 / 100.0));

    // 向量写后可见：upsert 单 doc 新向量，立刻自查
    let mut vis = Vec::new();
    for (pk, _) in vecs.iter().take(20) {
        let v = rng.unit_vec(args.dim);
        let mut d = Doc::new().unwrap();
        d.set_pk(pk);
        d.add_string("note_id", "note-upd").unwrap();
        d.add_string("body", "vector upsert visibility").unwrap();
        d.add_vector_f32("embedding", &v).unwrap();
        let t = Instant::now();
        let r = vcol.upsert(&[&d]).unwrap();
        assert_eq!(r.error_count, 0, "{}", first_error(&r));
        let mut q = SearchQuery::new("embedding", &v, 1).unwrap();
        q.set_output_fields(&["note_id"]).unwrap();
        loop {
            let r = vcol.query(&q).unwrap();
            if r.first().and_then(|d| d.get_pk()) == Some(pk.as_str()) && r[0].get_string("note_id").unwrap().as_deref() == Some("note-upd") {
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(5), "vector upsert never visible");
            std::thread::sleep(Duration::from_micros(200));
        }
        vis.push(ms(t.elapsed()));
    }
    report("vector upsert 1 doc → visible", &vis, "");
    vcol.close().unwrap();

    println!("\nfinal RSS {:.1} MB", rss_mb());
    shutdown().unwrap();
    if !args.keep {
        std::fs::remove_dir_all(&args.dir).ok();
    } else {
        println!("kept data in {}", args.dir.display());
    }
}
