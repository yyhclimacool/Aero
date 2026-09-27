# M2 CLI 验收

使用说明按 2026-09-26 源码更新。2026-09-22 的测试数字保留为历史记录，不代表本次复跑。对应 [方案 13.1](../project_plan.md)。

已交付：`search`、`read`、`backlinks`、`append`、`new`、`export`、`index`。无参数或 `aero <库目录>` 仍启动图形界面。开发时用 `cargo run -p aero-app -- <参数>`，打包后 CLI 与 GUI 共用应用包里的 `aero` 可执行文件。

## 用法

库目录按这个顺序找：`--vault`、环境变量 `AERO_VAULT`、最近打开且目录还在的库。索引与图形界面共用应用数据目录下的 `index/<vault-id>/`。`--json` 把结果写到标准输出；错误写到标准错误。

```text
aero search "查询" [--tag rust] [--limit 20]
aero read <笔记> [--block ^id]
aero backlinks <笔记> [--block ^id] [--limit 50]
aero append <笔记> --text "..."
aero new <路径> [--template 名称]
aero export <笔记> --format md|html|json
aero index status
aero index rebuild
```

- `search` 的查询语法与搜索框相同（`#tag`、`tag:tag`、`-#tag`）。`--tag` 再追加一个必须命中的标签。结果是块，带路径、标题和正文摘要。
- `read` 接受库内路径、唯一文件名、标题、别名，或索引里的 note id。`--block` 只输出那一块的源码。
- `backlinks` 只统计已经解析的入链。`--block` 再按 `^id` 筛选。
- `append` 在原文后空一行接上新段落，并更新索引。
- `new` 不带 `.md` 时补上。已存在则失败，不覆盖。`--template` 读取 `.aero/templates/<名称>.md`，模板名不能是路径。没有模板时写入一级标题。
- `export` 输出到标准输出：`md` 是 Markdown 源码，`html` 是渲染内容，`json` 包含路径、标题、源码与块列表。它不是附件打包器；`md` 输出会整理尾部空白，不用于字节级备份。可用 shell 重定向保存。
- `index status` 对比已索引笔记数和磁盘上的笔记数。`rebuild` 清空后按磁盘重建。
- `search` 目前只有全文检索，不调用图形界面的本地语义模型。

索引可用时，`new` / `append` 同步更新索引；GUI 正占用索引时先写文件，由 GUI 文件监听接续更新。应用未运行时，外部编辑不会由 CLI 持续监听，需要重开应用或 `index rebuild` 才能确保搜索与磁盘一致。

## 应用打开时使用 CLI

- `read`、`export`、`append` 指定实际存在的库内路径时直接读写文件，`new` 也不要求先拿到索引锁；通过标题、别名或 note id 解析目标时仍可能需要索引。并发使用时优先传 `note.md` 这样的明确路径。
- `search`、`backlinks`、`index status` 遇到引擎锁冲突时，复制全文索引到临时目录后查询，并读取原元数据库。这不是与 GUI 写入一致的事务快照，可能缺少尚未刷盘的内容；复制或打开副本也可能失败，不能当作最新结果保证。
- `index rebuild` 不使用临时副本，需要先退出占用该库索引的应用。
- CLI 写入不会修改 GUI 的未保存内存；GUI 按外部修改规则重载或提示冲突。

## 自动化（2026-09-22 历史记录）

新增 6 项回归，全量 **512 项通过**（app 283、链接绘制集成 17、core 156、往返 5、index 50、反链计数集成 1）。

- `gui_launch_is_not_a_subcommand`：无参数和库路径不进入命令行；子命令和 `--help` 进入。
- `help_lists_the_six_commands`：帮助里能看到六个子命令。
- `new_append_read_search_and_backlinks_share_one_index`：新建、追加 `[[链接]]`、重建后能读到、能搜到、反链能指回来；缺失块 id 报错。
- `new_refuses_to_overwrite_and_template_is_copied`：模板正文原样写入；再次新建失败；模板名不能带路径。
- `recent_vault_is_used_when_no_flag_is_given`：不给 `--vault` 时用最近打开的库。
- `append_keeps_a_blank_line_and_rejects_empty_text`：追加前留一个空行；空文字拒绝。

```sh
cargo fmt -p aero-core -p aero-index -p aero-app -- --check
cargo test --locked --offline -p aero-app --bin aero cli::
cargo test --locked --offline -p aero-core -p aero-index -p aero-app
```

## 后续代码覆盖与限制

当前源码另含 `export_writes_markdown_html_and_json` 与 `file_commands_work_while_the_index_lock_is_held`，分别覆盖三种导出和索引被占用时的文件操作 / 搜索回退。上面的 512 项仅为 2026-09-22 的结果；最近全工作区记录见 [2026-09-26 性能验收](performance-followup-2026-09-26.md)。

无参数启动仍是窗口，帮助用 `aero --help`；CLI 尚无语义搜索或 MCP 服务。索引副本不能保证与正在写入的 GUI 完全同步。
