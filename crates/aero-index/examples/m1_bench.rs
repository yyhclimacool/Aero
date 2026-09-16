//! M1 的真实 core/index 管线基准；不把无头耗时冒充窗口首帧或输入到绘制耗时。
//! cargo run --release -p aero-index --example m1_bench -- 10000
use std::{error::Error, fs, sync::Arc, time::Instant};

use aero_core::{Document, Edit, Vault, VaultPath};
use aero_index::Indexer;

fn percentile(values: &mut [f64], percent: usize) -> f64 {
    values.sort_by(f64::total_cmp);
    values[(values.len() * percent).div_ceil(100).saturating_sub(1)]
}

fn note(n: usize) -> String {
    let mut body = format!("---\ntags: [rust, 技术]\n---\n# Knowledge {n}\n\n");
    let prose = "本地知识管理、中文检索与性能测试。Rust memory ownership and concurrent indexing. ";
    for section in 0..12 {
        body.push_str(&format!(
            "Section {section}: [[note-{}]] **Markdown** {}\n\n",
            (n + 1) % 1000,
            prose.repeat(2)
        ));
    }
    body
}

fn main() -> Result<(), Box<dyn Error>> {
    let count: usize = std::env::args().nth(1).unwrap_or_else(|| "10000".into()).parse()?;
    if ![1000, 10000, 100000].contains(&count) {
        return Err("note count must be 1000, 10000 or 100000".into());
    }
    // 每次全新临时 vault/index；不触碰个人笔记。生成时间不计入重建时间。
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("vault");
    fs::create_dir(&root)?;
    let mut bytes = 0usize;
    for n in 0..count {
        let body = note(n);
        bytes += body.len();
        fs::write(root.join(format!("note-{n}.md")), body)?;
    }
    eprintln!("generated {count} notes, {} bytes/note", bytes / count);
    let vault = Arc::new(Vault::open(&root)?);
    let start = Instant::now();
    let mut index = Indexer::open(vault.clone(), &temp.path().join("index"))?;
    let report = index.sync_all(|done, total| {
        if done % 10000 == 0 {
            eprintln!("indexed {done}/{total}");
        }
    });
    if !report.errors.is_empty() || report.indexed != count {
        return Err(format!("incomplete rebuild: {report:?}").into());
    }
    index.optimize()?;
    let rebuild_ms = start.elapsed().as_secs_f64() * 1000.;
    let queries = ["中文", "Rust memory", "ownership", "Markdown", "#rust", "#技术", "\"local first\""];
    for query in queries {
        index.search(query, 50)?;
    }
    let mut search_ms = Vec::with_capacity(210);
    for n in 0..210 {
        let start = Instant::now();
        let hits = index.search(queries[n % queries.len()], 50)?;
        if n % queries.len() != queries.len() - 1 && hits.is_empty() {
            return Err(format!("expected hits for {}", queries[n % queries.len()]).into());
        }
        search_ms.push(start.elapsed().as_secs_f64() * 1000.);
    }
    let mut update_ms = Vec::with_capacity(30);
    for n in 0..30 {
        let path = VaultPath::new(&format!("note-{n}.md"))?;
        let token = format!("freshneedle{n:04}");
        let body = format!("{}\n{token}\n", note(n));
        let stamp = vault.write_bytes(&path, body.as_bytes())?;
        // 从保存完成起计时，包含解析、元数据库、FTS 写入和一次检索验证。
        let start = Instant::now();
        index.index_text(&path, &body, stamp)?;
        let hits = index.search(&token, 50)?;
        if !hits.iter().any(|hit| hit.path == path) {
            return Err(format!("saved note not searchable: {path}").into());
        }
        update_ms.push(start.elapsed().as_secs_f64() * 1000.);
    }
    let search_p95 = percentile(&mut search_ms, 95);
    let update_max = update_ms.iter().copied().fold(0., f64::max);
    println!(
        "{{\"notes\":{count},\"bytes_per_note\":{},\"rebuild_ms\":{rebuild_ms:.3},\"search_p95_ms\":{search_p95:.3},\"saved_to_searchable_max_ms\":{update_max:.3}}}",
        bytes / count
    );
    drop(index);
    // 仅测 Document 构造和编辑+重解析，GUI 首帧/绘制/滚动仍需实际应用测量。
    for mb in [1usize, 10] {
        let unit = note(1);
        let source = unit.repeat((mb * 1024 * 1024).div_ceil(unit.len()));
        let start = Instant::now();
        let mut document = Document::from_text(&source);
        let parse_ms = start.elapsed().as_secs_f64() * 1000.;
        let mut edit_ms = Vec::new();
        for _ in 0..20 {
            let start = Instant::now();
            document.edit(Edit::insert(0, "a"))?;
            document.reparse();
            edit_ms.push(start.elapsed().as_secs_f64() * 1000.);
        }
        let edit_p99 = percentile(&mut edit_ms, 99);
        println!(
            "{{\"document_bytes\":{},\"parse_ms\":{parse_ms:.3},\"edit_reparse_p99_ms\":{edit_p99:.3},\"includes_rendering\":false}}",
            source.len()
        );
    }
    // 只有 100k 运行才具有计划中的索引规模；指标超标让验收命令失败。
    if count == 100000 && (search_p95 >= 50. || rebuild_ms >= 180000. || update_max >= 300.) {
        return Err("M1 index performance budget exceeded (see JSON results)".into());
    }
    Ok(())
}
