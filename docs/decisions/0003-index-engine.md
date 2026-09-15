# ADR 0003 · 索引引擎：zvec-rust（全文 + 向量合一）

- 状态：**已采纳**（2026-09-15，spike 半天完成）
- 关联：`project_plan.md` 10.3 / 10.4 / 16 / 19；`spikes/zvec-index/`（可重跑：`scripts/fetch-zvec.sh && cargo run -p spike-zvec-index --release`）

## 问题

方案把全文检索与向量检索押在一个引擎上，M0 只验证 zvec 一项（10.3 的决策方式）。要回答：

1. `bench-10k` 上的查询延迟能否远低于 19 节 `bench-100k` p95 < 50 ms 的预算；
2. **写后可见延迟**：保存一篇笔记（十几个 block 替换）后多久能搜到，预算 < 300 ms；
3. 常驻内存、全量重建峰值、冷打开时间；
4. 分发链路：dylib 进 `Contents/Frameworks` 并签名、`ZVEC_LIB_DIR` 离线构建、能否静态链接。

## 被测对象

- zvec-rust **v0.7.1**（git tag，2026-09-14）：crates.io 上还是 0.7.0，而 jieba 词典随预编译包分发、以及 `set_default_jieba_dict_dir` 这组 C API 都是 0.7.1 才有。
- 预编译 `zvec-prebuilt-aarch64-apple-darwin.tar.gz`（10 MB）：`libzvec_c_api.dylib` 23 MB（install name `@rpath/…`，minos macOS 14.0，只依赖 libc++ / libSystem / CoreFoundation，ad-hoc 签名）+ `data/jieba_dict/`（5.6 MB）。**没有静态库**。
- 语料：1 万篇、120k block、29 MB 正文、中英混排带 `[[wikilink]]`。中文词从 jieba 词典前 3 万个 2–4 字词按 Zipf(1.05) 抽取，英文 8k 词同分布——保证分词与词频分布接近真实文本（第一版用 240 个词的词表，每个词命中 20% 的 block，p95 被夸大 10 倍，作废）。
- 机器：Apple M5 Pro，release 构建。

## 结果

### 查询（FTS jieba + lowercase，topk 20，只取 `note_id` / `block_ord`）

| 查询 | p50 | p95 | p99 |
| --- | --- | --- | --- |
| 中文高频词 | 0.14 ms | 0.92 ms | 1.5 ms |
| 中文中频词（rank 1k–10k） | 0.07 ms | 0.11 ms | 0.35 ms |
| 中文双词 / 中英混合 | 0.45 ms | 2.5–2.9 ms | 3.4 ms |
| 中文词 + `note_id = '…'` 过滤 | 0.31 ms | 1.2 ms | 1.8 ms |
| 200 次保存未 optimize 后，中文高频词 | 0.83 ms | 8.3 ms | 8.6 ms |

10k 上 p95 ≤ 3 ms，离 100k 上 50 ms 的预算有一个数量级余量。相关性正常（top1 含查询词），大小写不敏感由 `lowercase` filter 保证。未 optimize 的碎片段让 p95 涨到 8–9 ms，`optimize()` 在 10k 上 0.5 s，适合空闲时做。

### 写后可见延迟（保存一篇 ≈ 12 block）

| 策略 | 写入 p95 | 轮询到可搜 p95 | 保存→可搜 总 p95 |
| --- | --- | --- | --- |
| `delete(旧 pk) + insert` | 1.6 ms | 0.55 ms | **2.2 ms** |
| `upsert(新 block) + delete(消失的 pk)` | 1.3 ms | 0.94 ms | **2.0 ms** |
| `delete_by_filter("note_id = …") + insert` | — | — | **不可用**，见下 |

写入即可见，不需要 `flush()`；预算 300 ms，实测 2 ms。这意味着 10.3 里"给正在编辑的文档维护内存 overlay"可以简化：直接把编辑中的 block 以几百毫秒节流 upsert 进引擎即可。

### zvec 缺陷：`delete_by_filter` 留下僵尸主键

`delete_by_filter` 之后，被删 doc 在 `fetch` / 查询里都消失了，但同一 pk **`insert` 报 `AlreadyExists`，`delete` 报 `NotFound: … already deleted`**，`flush()` 与 `optimize()` 都不能恢复，只有 `upsert` 能把 pk 救回来。方案原本打算用 `delete_by_filter` 做"文件变更后整篇重建"，**这条路不能走**。Aero 的保存路径定为：`upsert(新 block)`，再 `delete(旧有而新无的 pk)`；旧 pk 列表来自 SQLite 的 block 表（10.5 说 SQLite 是权威），不依赖引擎查询。向上游提 issue。

### 内存（干净进程 `--open-only` 打开已建好的索引，120k block）

| 配置 | 打开后 RSS（减去进程基线） | 打开耗时 |
| --- | --- | --- |
| jieba 全词典（349k 词） | **≈ 300 MB** | 225 ms |
| jieba 前 150k 词 | ≈ 190 MB | 116 ms |
| jieba 前 100k 词 | **≈ 145 MB** | 97 ms |
| jieba 前 50k 词 | ≈ 130 MB | 81 ms |
| ngram 分词（无词典） | ≈ 105 MB | 62 ms |

结论：FTS 索引本体约 100 MB 常驻（不随 `mmap` / `memory_limit` / `max_buffer_size` 变化，三者对 RSS 均无影响），**jieba 词典在全量时占 ≈ 200 MB，是最大单项**，且随词条数近似线性。cppjieba 的 trie 按词条分配，裁掉词频 < 11 的长尾（349k → 100k，1.4 MB）即可省 150 MB，OOV 交给 HMM 与 `cut_mode=search` 的子词切分。词典是进程级共享的：第二个带 jieba 字段的 Collection 只多 7 MB。

ngram 不是替代方案：构建慢 2 倍、磁盘 2.5 倍（268 MB vs 108 MB），英文按字母二元组匹配、precision 差到探针词在 top-5 都找不到。

全量重建峰值：一次性写 120k block 再 flush，RSS 冲到 ≈ 900 MB；**每 1 万 doc `flush()` 一次，峰值 ≈ 280 MB**（不含 spike 自己持有的语料），吞吐不变（120k block 17 s，含 flush）。

### 其他

- 全量构建 120k block：insert 12.5 s + flush 4.5 s，optimize 0.5 s 后磁盘 108 MB（正文 29 MB，3.7 倍）。外推 `bench-100k` ≈ 3 min，刚好在 19 节预算边缘，M3 时按真实数据复测。
- 冷打开 100 ms，首次查询 5 ms。
- 向量：20k × 384 维 HNSW（cosine, m16, ef200）写入 2.5 s，查询 p95 1.7 ms，自查 top1 200/200；`MultiQuery` 向量 + FTS 的 RRF p95 1.7 ms；单 doc upsert 后立即可查 p95 1.9 ms。API 与方案 10.4 的描述一致。
- 未见词（`可见令牌零号`）用 `match_string` 能召回。

### 分发链路

- **rpath 是下游的事**：zvec-sys 的 build.rs 用 `cargo:rustc-link-arg` 加 rpath，但该指令只作用于它自己的包，链接 zvec 的二进制拿不到任何 `LC_RPATH`（上游 bug，其 examples 能跑是碰巧）。`aero-app` 要自带 build.rs：开发期加 `ZVEC_LIB_DIR` 绝对路径，另加 `@executable_path/../Frameworks`。
- `.app` 链路已跑通（`spikes/zvec-index/bundle.sh`）：删掉绝对 rpath → dylib 进 `Contents/Frameworks`、词典进 `Resources/jieba_dict` → 先签 dylib 再签 app → `codesign --verify --deep --strict` 通过 → 从 bundle 启动，`DYLD_PRINT_LIBRARIES` 确认加载的是 bundle 内的 dylib。词典目录运行时用 `ConfigBuilder::jieba_dict_dir` / `set_default_jieba_dict_dir` 指定。公证本机无 Developer ID 无法实测，命令序列写在脚本末尾；dylib minos 14.0 与 Aero 的 macOS 15 底线兼容。
- 离线构建：`scripts/fetch-zvec.sh` 下载预编译包、比对 `scripts/zvec-prebuilt.sha256`、解到 `vendor/zvec/lib`（gitignore）；`.cargo/config.toml` 设 `ZVEC_LIB_DIR` + `ZVEC_AUTO_BUILD=0`，zvec-rust 关掉 `bundled`。`cargo build --offline` 通过。
- **git 依赖的代价**：zvec-rust 仓库把整个上游 C++ 树作为 submodule（`vendor/zvec` → arrow、rocksdb、antlr、googletest…），cargo 对 git 依赖会递归拉全部 submodule：首次解析 **~10 分钟、~730 MB**，只为 10k 行绑定代码。crates.io 版本没有这个问题。
- 静态链接：预编译包只有 dylib；上游 CMake 只提供 shared 目标，静态需自己编译整个 C++ 树（含 arrow / rocksdb）。**放弃**，dylib 链路已验证可用。

## 决定

**采用 zvec-rust 作为唯一索引引擎**，不再评估 Tantivy 与 `usearch` + `sqlite-vec`。四项验收：查询延迟、写后可见延迟、打包链路、离线构建全部通过；常驻内存在裁词典后达标。

带上以下约束：

1. **保存路径**：`upsert` + `delete(pk)`，禁用 `delete_by_filter`（除非上游修复）。
2. **词典**：随应用分发裁到 ~100k 词的 `jieba.dict.utf8`（按词频截断），加 Aero 自己的 `user.dict.utf8` 放领域词。裁剪脚本进 `scripts/`，M2 做。
3. **内存预算**（改 19 节）：索引进程部分在 `bench-10k` 上 < 150 MB；全量重建峰值 < 300 MB，靠每 1 万 block flush 一次。
4. **维护**：每篇保存后不需要 flush；空闲时（如 30 s 无编辑）`flush()`，每日或碎片段超阈值时 `optimize()`。
5. **依赖形态**：等上游把 0.7.1 发到 crates.io 后立刻从 git tag 切回版本号；CI 在此之前缓存 `~/.cargo/git`。
6. `aero-app` 自带 build.rs 声明 rpath；打包脚本删绝对 rpath、inside-out 签名。

## 对方案的修正

- 10.3：决策完成；"内存 overlay" 降级为"编辑中 block 节流 upsert"；引擎维护策略按上文。
- 10.4：删去"`delete_by_filter` 适合整篇重建"；`memory_limit` 不影响 FTS 常驻，只对向量侧有意义。
- 16：预编译包只有 dylib，静态链接放弃；rpath 由 `aero-app` 声明；词典进 `Resources`。
- 19：内存预算拆出索引份额；`bench-100k` 全量重建预算保留，M3 复测。
- 21：风险表更新 zvec 一行。
