//! 回放 proptest 的最小失败输入：`replay <base.md> d:<frac>:<chars> i:<frac>:<text> ...`，输出编辑后的文本到 stdout。
fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
fn main() {
    let mut args = std::env::args().skip(1);
    let mut text = std::fs::read_to_string(args.next().unwrap()).unwrap();
    for op in args {
        let parts: Vec<&str> = op.splitn(3, ':').collect();
        let frac: f64 = parts[1].parse().unwrap();
        let s = floor_boundary(&text, (frac * text.len() as f64) as usize);
        match parts[0] {
            "d" => {
                let n: usize = parts[2].parse().unwrap();
                let e = text[s..].char_indices().nth(n).map_or(text.len(), |(i, _)| s + i);
                eprintln!("delete {s}..{e} {:?}", &text[s..e]);
                text.replace_range(s..e, "");
            }
            "i" => {
                let t = parts[2].replace("\\n", "\n");
                eprintln!("insert {s} {:?}", t);
                text.insert_str(s, &t);
            }
            _ => panic!(),
        }
    }
    print!("{text}");
}
