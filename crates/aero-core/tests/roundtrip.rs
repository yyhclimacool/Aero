//! 往返测试框架（方案 8.3 / 20）。
//!
//! 1. 语料上 `Document(x).to_string() == x`，块树满足结构不变量，后处理无异常；
//! 2. 随机编辑序列（proptest）：未触及的块文本不变、与 String 参考实现一致、
//!    重解析后文本不变且不变量成立、全部撤销后 BlockId 全部恢复。
//!
//! 设置 `AERO_CORPUS_DIR=~/notes` 可额外跑一个真实笔记目录。

mod common;

use aero_core::{BlockId, Document, Edit, ParseReport};
use common::*;
use proptest::prelude::*;

fn check_doc(d: &Doc) -> Result<ParseReport, String> {
    let doc = Document::from_text(&d.text);
    if doc.to_string() != d.text {
        return Err(format!("{}: serialize(parse(x)) != x", d.name));
    }
    check_invariants(&doc).map_err(|e| format!("{}: {e}", d.name))?;
    Ok(doc.report.clone())
}

#[test]
fn corpus_files_roundtrip_with_clean_report() {
    for d in corpus_files() {
        let report = check_doc(&d).unwrap();
        assert_eq!(report.dropped, 0, "{}: {report:?}", d.name);
        assert_eq!(report.clamped, 0, "{}: {report:?}", d.name);
    }
}

#[test]
fn commonmark_spec_examples_roundtrip() {
    let docs = spec_examples();
    let mut clamped = Vec::new();
    let mut raw_gaps = 0;
    for d in &docs {
        let report = check_doc(d).unwrap();
        assert_eq!(report.dropped, 0, "{}: {report:?}", d.name);
        if report.clamped > 0 {
            clamped.push(d.name.clone());
        }
        raw_gaps += report.raw_gaps;
    }
    eprintln!(
        "spec: {} examples, {} with clamped ranges, {raw_gaps} raw gap blocks",
        docs.len(),
        clamped.len()
    );
    // ADR 0002 记录的 comrak 位置缺陷都在行内层，块级区间不应需要修正
    assert!(clamped.is_empty(), "block ranges needed clamping in: {clamped:?}");
}

#[test]
fn synthetic_documents_roundtrip() {
    for (seed, size) in [(1u64, 4 * 1024), (2, 64 * 1024), (3, 512 * 1024)] {
        let d = synthetic(seed, size);
        if let Ok(dir) = std::env::var("AERO_DUMP_SYNTHETIC") {
            std::fs::write(format!("{dir}/synthetic-{seed}.md"), &d.text).unwrap();
        }
        let report = check_doc(&d).unwrap();
        let detail: Vec<String> = report
            .clamped_nodes
            .iter()
            .map(|(what, raw, fixed)| {
                let s = floor_boundary(&d.text, raw.start.saturating_sub(20));
                let e = floor_boundary(&d.text, raw.end.min(d.text.len()));
                format!("{what} {raw:?}->{fixed:?}: {:?}", &d.text[s..e])
            })
            .collect();
        assert_eq!(report, ParseReport::default(), "{}: {detail:#?}", d.name);
    }
}

#[test]
fn external_corpus_roundtrip() {
    let docs = external_corpus();
    if docs.is_empty() {
        return;
    }
    let mut failures = Vec::new();
    let mut clamped = 0;
    for d in &docs {
        match check_doc(d) {
            Ok(r) => {
                clamped += r.clamped;
                for (what, raw, fixed) in &r.clamped_nodes {
                    eprintln!("  {}: clamped {what} {raw:?} -> {fixed:?}", d.name);
                }
            }
            Err(e) => failures.push(e),
        }
    }
    eprintln!("external corpus: {} docs, {clamped} clamped ranges", docs.len());
    assert!(failures.is_empty(), "{failures:#?}");
}

// ---------------------------------------------------------------------------
// 随机编辑
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Op {
    Insert { frac: f64, text: String },
    Delete { frac: f64, chars: usize },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0.0f64..1.0, prop_oneof![
                Just("a".to_string()), Just("中".to_string()), Just(" ".to_string()), Just("\n".to_string()),
                Just("\n\n".to_string()), Just("- ".to_string()), Just("# ".to_string()), Just("`".to_string()),
                Just("[[x]]".to_string()), Just("> ".to_string()), Just("**".to_string()), Just("^id".to_string()),
            ])
            .prop_map(|(frac, text)| Op::Insert { frac, text }),
        2 => (0.0f64..1.0, 1usize..4).prop_map(|(frac, chars)| Op::Delete { frac, chars }),
    ]
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn resolve(text: &str, op: &Op) -> Edit {
    match op {
        Op::Insert { frac, text: t } => {
            Edit::insert(floor_boundary(text, (frac * text.len() as f64) as usize), t.clone())
        }
        Op::Delete { frac, chars } => {
            let s = floor_boundary(text, (frac * text.len() as f64) as usize);
            let e = text[s..].char_indices().nth(*chars).map_or(text.len(), |(i, _)| s + i);
            Edit::delete(s..e)
        }
    }
}

fn base_documents() -> Vec<String> {
    let mut v: Vec<String> = corpus_files().into_iter().map(|d| d.text).collect();
    v.push(synthetic(7, 3000).text);
    v.push(synthetic(8, 12000).text);
    if let Ok(dir) = std::env::var("AERO_DUMP_SYNTHETIC") {
        for (i, t) in v.iter().enumerate() {
            std::fs::write(format!("{dir}/base-{i}.md"), t).unwrap();
        }
    }
    v
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, .. ProptestConfig::default() })]

    #[test]
    fn random_edits_keep_untouched_blocks_and_ids(base_idx in 0usize..4, ops in prop::collection::vec(op_strategy(), 1..25)) {
        let bases = base_documents();
        let base = &bases[base_idx % bases.len()];
        let mut doc = Document::from_text(base);
        let mut reference = base.clone();

        // 原始块：文本 + 当前区间（跟着编辑同步平移，用来判断是否被触碰）
        let original: Vec<(BlockId, &'static str, String)> =
            doc.blocks().iter().map(|(_, b)| (b.id, b.kind.tag(), doc.block_text(b))).collect();
        let mut cur: Vec<(usize, usize)> = doc.blocks().iter().map(|(_, b)| { let r = doc.range_of(b); (r.start, r.end) }).collect();
        // 文本层面的触碰：编辑落在块的开区间内（边界插入不算，块只会"长大"）
        let mut touched = vec![false; original.len()];
        // 结构层面的触碰：编辑碰到块区间外扩一字节的闭区间（边界处的编辑可能改变块类型或合并相邻块）
        let mut touched_struct = vec![false; original.len()];

        for op in &ops {
            let e = resolve(&reference, op);
            let (s, en) = (e.range.start, e.range.end);
            for (i, &(bs, be)) in cur.iter().enumerate() {
                let hits = if s == en { s > bs && s < be } else { s < be && en > bs };
                if hits { touched[i] = true; }
                let (lo, hi) = (bs.saturating_sub(1), be + 1);
                let near = if s == en { s >= lo && s <= hi } else { s <= hi && en >= lo };
                if near { touched_struct[i] = true; }
            }
            reference.replace_range(e.range.clone(), &e.text);
            doc.edit(e.clone()).unwrap();
            prop_assert_eq!(doc.word_count(), aero_core::text::word_count(&reference));
            // 同步 cur：与 anchor 相同的规则
            let delta = e.text.len() as isize - (en - s) as isize;
            for c in cur.iter_mut() {
                let mv = |p: usize, left: bool| -> usize {
                    if p < s { p }
                    else if p >= en && !(p == s && left) { (p as isize + delta) as usize }
                    else if p == s { if left { p } else { s + e.text.len() } }
                    else if left { s } else { s + e.text.len() }
                };
                *c = (mv(c.0, true), mv(c.1, false));
            }
        }

        // 父容器被碰到（如 `[^1]:` / `> ` / `- ` 前缀被改）时子块也算被碰到：沿先序遍历的深度栈向下传播
        let depths: Vec<usize> = Document::from_text(base).blocks().iter().map(|(d, _)| d).collect();
        let mut stack: Vec<bool> = Vec::new();
        for i in 0..depths.len() {
            stack.truncate(depths[i]);
            let inherited = stack.last().copied().unwrap_or(false);
            touched_struct[i] |= inherited;
            stack.push(touched_struct[i]);
        }

        // (a) 文本与参考一致
        prop_assert_eq!(doc.to_string(), reference.clone());
        prop_assert_eq!(doc.word_count(), aero_core::text::word_count(&reference));
        // (b) 未触碰块的文本不变（边界插入允许原文为子串）
        for (i, (_, _, orig)) in original.iter().enumerate() {
            if touched[i] { continue; }
            let now = doc.slice(cur[i].0..cur[i].1).to_string();
            prop_assert!(now.contains(orig.as_str()), "untouched block changed: {:?} -> {:?}", orig, now);
        }
        // (c) 重解析后文本不变、不变量成立；结构上未被触碰的块 id 不变
        doc.reparse();
        prop_assert_eq!(doc.to_string(), reference.clone());
        prop_assert!(check_invariants(&doc).is_ok(), "{:?}", check_invariants(&doc));
        //     "未被触碰"是文本层面的判断，编辑仍可能改变别的块的范围（例如在闭合围栏后插入字符让代码块
        //     吞掉后文），所以只要求：未触碰、且重解析后在原（漂移后的）位置仍以同类型同文本存在的块，沿用原 id。
        let ids_now: Vec<(BlockId, &'static str, (usize, usize), String)> = doc
            .blocks()
            .iter()
            .map(|(_, b)| { let r = doc.range_of(b); (b.id, b.kind.tag(), (r.start, r.end), doc.block_text(b)) })
            .collect();
        let mut survivors: Vec<BlockId> = Vec::new();
        for (i, (id, tag, orig)) in original.iter().enumerate() {
            if touched_struct[i] { continue; }
            let Some(now) = ids_now.iter().find(|(_, t, r, s)| t == tag && *r == cur[i] && s == orig) else { continue };
            prop_assert_eq!(now.0, *id, "untouched block at {:?} lost its id after reparse: {} {:?}; ops={:?}", cur[i], tag, orig, ops);
            survivors.push(*id);
        }
        // (d) 全部撤销：文本恢复；重解析后，(c) 中保住 id 的块 id 仍是原来的
        while doc.undo().is_some() {}
        prop_assert_eq!(doc.to_string(), base.clone());
        prop_assert_eq!(doc.word_count(), aero_core::text::word_count(base));
        doc.reparse();
        let ids_after: Vec<BlockId> = doc.blocks().iter().map(|(_, b)| b.id).collect();
        prop_assert_eq!(ids_after.len(), original.len());
        for (i, (id, _, _)) in original.iter().enumerate() {
            if !survivors.contains(id) { continue; }
            prop_assert_eq!(ids_after[i], *id, "block {} lost its id across edit+undo", i);
        }
    }
}
