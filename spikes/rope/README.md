# spike-rope · 文本模型选型

M0 的第四个 spike，回答方案 7.2 / 7.4 的问题：Rope 选哪个、Anchor 怎么实现、10 MB 文档上编辑与解析各花多少。结论见 `docs/decisions/0004-rope-and-anchors.md`。

```sh
cargo run -p spike-rope --release            # 1 MB 与 10 MB，各回放 10k 次编辑
cargo run -p spike-rope --release -- --mb 10 --ops 50000
```

跑一次约 15 s。对每个候选（`String` 基线、`ropey 1.6`、`ropey 2.0.0-beta.1`、`crop 0.4`）输出：

- 构建时间与堆开销（用计数分配器精确测，不是 RSS）；
- `clone()` 快照、随机插入 / 删除、连续键入的每次耗时分布；
- byte→line、line→byte、byte→char 换算；
- 切一个 300 B 块到 `String`、物化整篇 `String`（喂 comrak）；
- marker 表每次编辑的平移成本，以及回放全部编辑后对未触碰块的文本一致性校验（必须 0 WRONG）。

四个候选回放同一份确定性编辑脚本，最后校验最终文本哈希一致，保证比较的是同一件事。

`MarkerTable` 就是 ADR 里 Anchor 的参考实现（排序 `Vec<(pos, Bias)>` + 二分定位 + 线性平移 + 删除区间内塌缩），可以直接搬进 `aero-core`。
