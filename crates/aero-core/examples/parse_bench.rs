//! Snapshot parsing and bounded local parsing only; excludes apply/layout/GUI.
//! cargo run --locked -p aero-core --example parse_bench
use aero_core::{Document, Edit};
use std::{hint::black_box, time::Instant};

fn main() {
    println!("bytes,full_parse_p50_ms,full_parse_p95_ms,local_p50_us,local_p95_us");
    for mib in [1, 10] {
        let paragraph = "中文 rust hello ".repeat(40) + "\n\n";
        let source = paragraph.repeat(mib * 1024 * 1024 / paragraph.len());
        let mut doc = Document::from_text(&source);
        let mut full = Vec::new();
        let mut local = Vec::new();
        for iteration in 0..12 {
            let snapshot = doc.snapshot();
            let started = Instant::now();
            black_box(snapshot.parse());
            let before = started.elapsed().as_secs_f64() * 1000.;
            doc.edit(Edit::insert(10, "中")).unwrap();
            let started = Instant::now();
            assert!(doc.try_reparse_local(64 * 1024));
            let after = started.elapsed().as_secs_f64() * 1e6;
            doc.undo();
            doc.reparse();
            if iteration >= 2 {
                full.push(before);
                local.push(after);
            }
        }
        full.sort_by(f64::total_cmp);
        local.sort_by(f64::total_cmp);
        println!("{},{:.2},{:.2},{:.2},{:.2}", source.len(), full[5], full[9], local[5], local[9]);
    }
}
