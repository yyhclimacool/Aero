//! 调试用：打印一篇文档的块树与后处理报告。`cargo run -p aero-core --example dump -- file.md`
fn main() {
    let text = std::fs::read_to_string(std::env::args().nth(1).expect("usage: dump <file.md>")).unwrap();
    let out = aero_core::parse(&text);
    println!(
        "report: clamped={} dropped={} raw_gaps={}",
        out.report.clamped, out.report.dropped, out.report.raw_gaps
    );
    for (what, raw, fixed) in &out.report.clamped_nodes {
        println!(
            "  clamped {what} {raw:?} -> {fixed:?}: {:?}",
            text.get(raw.clone()).map(|s| s.chars().take(60).collect::<String>())
        );
    }
    for (d, b) in aero_core::ParsedBlock::iter(&out.blocks) {
        let preview: String = text[b.range.clone()].chars().take(40).collect();
        println!(
            "{}{:?} {:?} suffix={} id={:?} {preview:?}",
            "  ".repeat(d),
            b.kind,
            b.range,
            b.suffix_len,
            b.persisted_id
        );
    }
}
