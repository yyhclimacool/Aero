//! Word-count maintenance only; excludes reparsing, layout and presentation.
//! Run with: cargo run --locked -p aero-core --example word_count_bench
use std::hint::black_box;
use std::time::{Duration, Instant};

use aero_core::{Document, Edit, text::word_count};

fn samples(document: &mut Document, full_scan: bool) -> Vec<Duration> {
    let expected = document.word_count() + 1;
    (0..110)
        .filter_map(|iteration| {
            let started = Instant::now();
            document.edit(Edit::insert(0, "x ")).unwrap();
            let count = if full_scan { word_count(&document.to_string()) } else { document.word_count() };
            black_box(count);
            let elapsed = started.elapsed();
            assert_eq!(count, expected);
            document.undo();
            (iteration >= 10).then_some(elapsed)
        })
        .collect()
}

fn percentiles(mut samples: Vec<Duration>) -> (f64, f64) {
    samples.sort_unstable();
    (samples[50].as_secs_f64() * 1e6, samples[94].as_secs_f64() * 1e6)
}

fn main() {
    println!("bytes,full_scan_p50_us,full_scan_p95_us,incremental_p50_us,incremental_p95_us");
    for mib in [1, 10] {
        let source = "中文 rust hello ".repeat(mib * 1024 * 1024 / "中文 rust hello ".len());
        let mut document = Document::from_text(&source);
        let before = percentiles(samples(&mut document, true));
        let after = percentiles(samples(&mut document, false));
        println!("{},{:.2},{:.2},{:.2},{:.2}", source.len(), before.0, before.1, after.0, after.1);
    }
}
