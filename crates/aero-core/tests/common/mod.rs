//! 往返测试的公共部分：语料加载与块树不变量（方案 8.3 / 20）。

use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use aero_core::{Block, BlockKind, Document};

pub struct Doc {
    pub name: String,
    pub text: String,
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

/// `tests/corpus/*.md`
pub fn corpus_files() -> Vec<Doc> {
    let mut out = Vec::new();
    let mut entries: Vec<_> = fs::read_dir(corpus_dir()).unwrap().flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.extension().is_some_and(|x| x == "md") {
            out.push(Doc {
                name: p.file_name().unwrap().to_string_lossy().into_owned(),
                text: fs::read_to_string(&p).unwrap(),
            });
        }
    }
    out
}

/// CommonMark spec 的全部 example（`→` 还原为 tab）。
pub fn spec_examples() -> Vec<Doc> {
    let spec = fs::read_to_string(corpus_dir().join("commonmark-spec.txt")).unwrap();
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
        docs.push(Doc { name: format!("spec#{}(L{})", docs.len() + 1, ln + 1), text: md });
    }
    assert!(docs.len() > 600, "spec examples not found");
    docs
}

/// 环境变量 `AERO_CORPUS_DIR` 指向的真实笔记目录（可选，本地跑）。
pub fn external_corpus() -> Vec<Doc> {
    let Ok(dir) = std::env::var("AERO_CORPUS_DIR") else { return Vec::new() };
    let mut out = Vec::new();
    collect_md(Path::new(&dir), &mut out);
    out
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
        } else if p.extension().is_some_and(|x| x == "md")
            && let Ok(text) = fs::read_to_string(&p)
        {
            out.push(Doc { name: p.display().to_string(), text });
        }
    }
}

/// 一个小的确定性 Markdown 生成器，覆盖常见块类型与中英混排。
pub fn synthetic(seed: u64, target_bytes: usize) -> Doc {
    let mut x = seed | 1;
    let mut rng = move |n: usize| -> usize {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) % n as u64) as usize
    };
    const ZH: &[&str] =
        &["笔记", "编辑器", "向量索引", "全文检索", "反向链接", "标签", "段落", "标题", "渲染", "性能"];
    const EN: &[&str] =
        &["rope", "anchor", "block", "tree", "parse", "render", "gpui", "comrak", "zvec", "note"];
    let mut s = String::new();
    let mut sec = 0;
    let sentence = |rng: &mut dyn FnMut(usize) -> usize, s: &mut String| {
        for i in 0..(4 + rng(6)) {
            if rng(3) == 0 {
                if i > 0 {
                    s.push(' ');
                }
                s.push_str(EN[rng(EN.len())]);
                s.push(' ');
            } else {
                s.push_str(ZH[rng(ZH.len())]);
            }
        }
        match rng(8) {
            0 => s.push_str(" [[链接]]"),
            1 => s.push_str(" **粗体**"),
            2 => s.push_str(" `code`"),
            3 => s.push_str(" #标签"),
            _ => {}
        }
        s.push('。');
    };
    while s.len() < target_bytes {
        sec += 1;
        s.push_str(&format!("\n## 第 {sec} 节\n\n"));
        for _ in 0..(2 + rng(4)) {
            match rng(9) {
                0 => {
                    s.push_str("```rust\n    let x = 1; // ");
                    s.push_str(EN[rng(EN.len())]);
                    s.push_str("\n```\n\n");
                }
                1 => {
                    for _ in 0..(2 + rng(4)) {
                        s.push_str(if rng(3) == 0 { "- [ ] " } else { "- " });
                        sentence(&mut rng, &mut s);
                        s.push('\n');
                    }
                    s.push('\n');
                }
                2 => {
                    s.push_str("> [!note] 标题\n> ");
                    sentence(&mut rng, &mut s);
                    s.push_str("\n\n");
                }
                3 => {
                    s.push_str("| a | b |\n|---|---|\n| 1 | 2 |\n\n");
                }
                4 => {
                    sentence(&mut rng, &mut s);
                    s.push_str(" ^blk-");
                    s.push_str(&format!("{}", rng(100000)));
                    s.push_str("\n\n");
                }
                _ => {
                    sentence(&mut rng, &mut s);
                    sentence(&mut rng, &mut s);
                    s.push_str("\n\n");
                }
            }
        }
    }
    Doc { name: format!("synthetic({seed},{target_bytes})"), text: s }
}

/// 块树的结构不变量。违反时返回描述。
pub fn check_invariants(doc: &Document) -> Result<(), String> {
    let text = doc.to_string();
    let len = text.len();
    fn walk(
        doc: &Document,
        text: &str,
        blocks: &[Block],
        parent: Range<usize>,
        depth: usize,
    ) -> Result<(), String> {
        let mut prev_end = parent.start;
        for b in blocks {
            let r = doc.range_of(b);
            if r.start > r.end || r.end > text.len() {
                return Err(format!("{:?} range {r:?} out of bounds (len {})", b.kind, text.len()));
            }
            if !text.is_char_boundary(r.start) || !text.is_char_boundary(r.end) {
                return Err(format!("{:?} range {r:?} not on char boundary", b.kind));
            }
            if r.start < parent.start || r.end > parent.end {
                return Err(format!("{:?} range {r:?} escapes parent {parent:?}", b.kind));
            }
            if r.start < prev_end {
                return Err(format!(
                    "{:?} range {r:?} overlaps previous sibling ending at {prev_end}",
                    b.kind
                ));
            }
            if b.suffix_len as usize > r.len() {
                return Err(format!("{:?} suffix_len {} > range {r:?}", b.kind, b.suffix_len));
            }
            if !b.kind.is_container() && !b.children.is_empty() {
                return Err(format!("leaf {:?} has children", b.kind));
            }
            if depth == 0 {
                // 顶层间隙必须是空白（否则有内容被解析器吞掉而没有变成 Raw）
                let gap = &text[prev_end..r.start];
                if !gap.trim().is_empty() {
                    return Err(format!("non-blank gap before {:?} at {}: {gap:?}", b.kind, r.start));
                }
            }
            walk(doc, text, &b.children, r.clone(), depth + 1)?;
            prev_end = r.end;
        }
        if depth == 0 {
            let tail = &text[prev_end..];
            if !tail.trim().is_empty() {
                return Err(format!("non-blank tail after last block: {tail:?}"));
            }
        }
        Ok(())
    }
    walk(doc, &text, &doc.blocks().roots, 0..len, 0)?;

    // Marker 块必须有 id 或属性，且不能是第一个兄弟（没有可挂靠的块时仍允许存在，但要有内容）
    for (_, b) in doc.blocks().iter() {
        if b.kind == BlockKind::Marker && b.persisted_id.is_none() && b.attrs.is_empty() {
            // 属性已经 merge 给前一个兄弟，这里只检查 id 或原文形态
            let t = doc.block_text(b);
            if !(t.trim_start().starts_with('^') || t.trim_start().starts_with('{')) {
                return Err(format!("marker block without marker text: {t:?}"));
            }
        }
    }
    Ok(())
}
