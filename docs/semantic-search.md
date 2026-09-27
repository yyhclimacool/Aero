# 本地语义搜索

按 2026-09-26 源码核对。本文是使用与实现说明；既有测试结果见 [性能续作验收](qa/performance-followup-2026-09-26.md)，本次文档更新未重新下载或运行真实模型。

## 开启与使用

1. 打开一个 vault，按 `⌘,`，选择「搜索与隐私」。也可从「视图 → 搜索与隐私…」或搜索栏的「设置」进入。
2. 开启「允许模型下载」，再点击「下载模型」。下载与启用语义搜索是两个独立操作；仅打开语义搜索不会自动下载。
3. 开启「启用本地语义搜索」。模型就绪后，后台在输入空闲时为笔记建立索引；面板显示进度，可暂停或恢复。
4. 按 `⌘⇧F`，选择「混合」，输入自然语言查询。全文结果先显示，语义结果随后融合；选择「全文」只查关键词。

下载许可和语义搜索默认均关闭。模型缺失、加载中或出错时，全文搜索仍可使用；「混合」会显示相应提示。仅按标签查询时不需要语义推理。

## 模型与隐私

| 项目 | 当前实现 |
| --- | --- |
| 模型 | `Xenova/multilingual-e5-small`，固定修订 `761b726dd34fb83930e26aab4e9ac3899aa1fa78`，int8 ONNX |
| 下载 | 权重与 tokenizer 共 135,137,323 字节，约 135 MB；校验大小与 SHA-256 后安装 |
| 推理 | 本地 CPU 单线程，384 维；每批最多 4 段，每段最多 256 tokens（含特殊 token） |
| 网络 | 仅显式下载固定模型文件时访问 Hugging Face 及许可的分发域名；对方可见 IP 与模型请求，笔记、查询不上传 |
| 离线 | 模型下载后可离线使用；关闭下载许可不影响已有本地模型 |
| 后台调度 | 输入空闲至少 800 ms 后分批索引；查询优先，可手动暂停；模型空闲 60 秒后卸载 |

运行时模型需要数百 MB 内存；卸载模型并不表示整个进程的内存会归零。当前没有远程 Embedding、GPU 推理或模型切换界面。

## 数据位置与结果边界

macOS 应用数据根目录是 `~/Library/Application Support/Aero/`：

- `models/<固定模型版本>/`：模型文件、校验清单与许可。
- `index/<vault-id>/semantic/`：向量索引与 `metadata.sqlite`，和全文的 `fts/` 分开。
- `search.toml`：应用级下载许可、语义搜索与暂停偏好。

语义索引可在退出应用后删除重建；不要把 `search.toml` 或可编辑的 `demo-vault/` 当缓存清掉。其他平台目录见 [方案 5.3](project_plan.md#53-索引与缓存放在应用数据目录)。

语义索引读取磁盘上的 Markdown 正文。保存、外部修改、删除及忽略规则变化会触发同步；未保存正文没有实时语义覆盖层。长块按预算切片并保留原块位置，结果按笔记路径和块去重；全文与语义在应用侧用 RRF 融合，语义回复迟到时保留用户当前选中的结果。

超过 2 MiB 的笔记、标题与标签总计超过 16 KiB、或片段数超过 8,192 的输入会被跳过或报告错误。只有标题 / 别名而没有正文的笔记不生成语义片段，仍可通过全文搜索查找。PDF 正文不进入索引。CLI 的 `search` 目前也只查全文。

## 开发验证

普通测试不需要下载语义模型；首次构建所需的 Cargo / ONNX Runtime 依赖需另行准备：

```sh
cargo test --locked -p aero-embedding
cargo test --locked -p aero-index semantic
cargo test --locked -p aero-app --bin aero semantic
cargo test --locked -p aero-app --bin aero search_view
```

真实模型测试默认 `ignore`。下列第一条会联网下载约 135 MB，测试目录必须在仓库的 `target/` 内；第二条复用该模型验证后台索引、查询、更新与退出，不能指向用户的正式模型缓存：

```sh
AERO_EMBEDDING_TEST_ROOT="$PWD/target/embedding-model-test" \
  cargo test --locked -p aero-embedding --test real_model download_and_rank_real_model -- --ignored --nocapture
AERO_EMBEDDING_TEST_ROOT="$PWD/target/embedding-model-test" \
  cargo test --locked -p aero-app --bin aero real_worker_indexes_queries_updates_and_stops_offline -- --ignored --nocapture
```

实现入口：[模型与下载](../crates/aero-embedding/src/lib.rs)、[后台服务](../crates/aero-app/src/semantic.rs)、[索引与融合](../crates/aero-index/src/semantic.rs)、[搜索视图](../crates/aero-app/src/search_view.rs)。
