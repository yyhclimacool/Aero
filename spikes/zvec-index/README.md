# Spike · zvec-rust 索引引擎

**已完成（2026-09-15）**，结论见 `docs/decisions/0003-index-engine.md`：zvec 定为唯一索引引擎。

保留原因：这是索引层的性能与行为基准。升级 zvec-rust、改保存路径、换词典时重跑一次，盯四个数：查询 p95、保存→可搜延迟、`--open-only` 的 RSS、`delete_by_filter` 是否仍留僵尸主键。等 `aero-index` 有自己的基准后删除。

```bash
# 一次性：下载并校验预编译 C 库到 vendor/zvec/lib（.cargo/config.toml 把 ZVEC_LIB_DIR 指到这里）
scripts/fetch-zvec.sh

# 全量：合成 bench-10k（1 万篇、120k block），FTS 构建 / 查询 / 三种保存策略的写后可见 / 200 次连续保存 / 冷打开 / 向量 + 混合
cargo run -p spike-zvec-index --release

# 常驻内存要在干净进程里量：先 --keep 建好，再 --open-only 打开
cargo run -p spike-zvec-index --release -- --keep --dir /tmp/zvec-keep
cargo run -p spike-zvec-index --release -- --open-only --dir /tmp/zvec-keep
cargo run -p spike-zvec-index --release -- --open-only --dir /tmp/zvec-keep --jieba-dict /path/to/trimmed_dict

# 其他开关
#   --notes N / --vec-docs N / --dim D    语料规模、向量数、维度
#   --tokenizer ngram                     换分词器（对照用；结论是不可用）
#   --flush-every 10000                   全量构建分批 flush，压峰值内存
#   --mmap / --buffer-mb N / --memory-limit MB   zvec 的三个内存相关开关（实测对 FTS 常驻无影响）

# macOS 打包链路：dylib 进 Contents/Frameworks、删绝对 rpath、inside-out 签名、从 bundle 启动
spikes/zvec-index/bundle.sh            # ad-hoc
SIGN_ID="Developer ID Application: …" spikes/zvec-index/bundle.sh
```

`build.rs` 演示了下游二进制必须自己声明 rpath（zvec-sys 的 `rustc-link-arg` 不会传下来），`aero-app` 照抄。
