//! CPU matching only, excluding GPUI, filesystem and catalog construction.
//! cargo run --locked -p aero-app --example matching_bench
#[allow(dead_code)]
#[path = "../src/matching.rs"]
mod matching;

use aero_core::{VaultPath, links::NoteSummary};
use std::{hint::black_box, sync::atomic::AtomicBool, time::Instant};

fn baseline(notes: &[NoteSummary], query: &str) -> Vec<usize> {
    let mut scores: Vec<_> = notes
        .iter()
        .enumerate()
        .filter_map(|(i, note)| {
            let score = std::iter::once(note.title.as_str())
                .chain(std::iter::once(note.path.as_str()))
                .chain(note.aliases.iter().map(String::as_str))
                .filter_map(|target| matching::score(&matching::prepare(query), &matching::prepare(target)))
                .max()?;
            Some((i, score))
        })
        .collect();
    scores.sort_by(|(a, sa), (b, sb)| sb.cmp(sa).then(notes[*a].path.cmp(&notes[*b].path)));
    scores.into_iter().take(8).map(|(i, _)| i).collect()
}

fn main() {
    println!("candidates,prepare_ms,old_p50_ms,old_p95_ms,prepared_p50_ms,prepared_p95_ms");
    for count in [10_000, 100_000] {
        let notes: Vec<_> = (0..count)
            .map(|i| NoteSummary {
                path: VaultPath::new(&format!("projects/note-{i:06}.md")).unwrap(),
                title: format!("项目规划 Rust 编辑器 {i}"),
                aliases: vec![format!("plan-{i}"), format!("Notes {i}")],
            })
            .collect();
        let started = Instant::now();
        let prepared = matching::Notes::new(&notes);
        let prepare_ms = started.elapsed().as_secs_f64() * 1000.;
        let query = matching::prepare("plan");
        let mut old = Vec::new();
        let mut new = Vec::new();
        for iteration in 0..23 {
            let started = Instant::now();
            let expected = baseline(&notes, "plan");
            let before = started.elapsed().as_secs_f64() * 1000.;
            let started = Instant::now();
            let actual = matching::top_k(notes.len(), 8, &AtomicBool::new(false), |i| {
                prepared.score(i, &query).map(|score| (score, notes[i].path.as_str()))
            });
            let after = started.elapsed().as_secs_f64() * 1000.;
            assert_eq!(actual, expected);
            black_box(actual);
            if iteration >= 3 {
                old.push(before);
                new.push(after);
            }
        }
        old.sort_by(f64::total_cmp);
        new.sort_by(f64::total_cmp);
        println!("{count},{prepare_ms:.2},{:.2},{:.2},{:.2},{:.2}", old[10], old[18], new[10], new[18]);
    }
}
