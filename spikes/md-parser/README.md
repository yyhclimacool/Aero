# Spike · comrak vs markdown-rs

**已完成（2026-09-15）**，结论见 `docs/decisions/0002-markdown-parser.md`：采用 comrak。

保留原因：这是解析器的回归基准。comrak 升级、或 aero-core 后处理层改动时重跑一次，对比位置精度与速度是否退化。等 `aero-core` 有自己的往返测试框架（M0 产出）后删除。

```bash
# 全量报告（spec 用例 + aero-sample + 任意笔记目录），Markdown 输出
cargo run -p spike-md-parser --release -- ~/code/wlog

# 排查某个文件的节点与区间（第二个参数是最大深度）
cargo run -p spike-md-parser --release -- --dump path/to/note.md 3

# 列出更多不一致样例
SPIKE_SAMPLES=100 cargo run -p spike-md-parser --release
```

`corpus/commonmark-spec.txt` 是 CommonMark 0.31.2 的 spec 原文（用例从中提取）；`corpus/aero-sample.md` 覆盖 6.3 / 6.4 的全部 L1/L2 语法。
