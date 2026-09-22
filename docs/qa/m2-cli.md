# M2 CLI 验收

更新日期：2026-09-22。对应 [方案 13.1](../project_plan.md)。

已交付：`search`、`read`、`backlinks`、`append`、`new`、`index`。无参数或 `aero <库目录>` 仍启动图形界面。
还没做：`export`，以及图形界面正占用索引时的并发访问。

## 用法

库目录按这个顺序找：`--vault`、环境变量 `AERO_VAULT`、最近打开且目录还在的库。索引与图形界面共用应用数据目录下的 `index/<vault-id>/`。`--json` 把结果写到标准输出；错误写到标准错误。

```text
aero search "查询" [--tag rust] [--limit 20]
aero read <笔记> [--block ^id]
aero backlinks <笔记> [--block ^id] [--limit 50]
aero append <笔记> --text "..."
aero new <路径> [--template 名称]
aero index status
aero index rebuild
```

- `search` 的查询语法与搜索框相同（`#tag`、`tag:tag`、`-#tag`）。`--tag` 再追加一个必须命中的标签。结果是块，带路径、标题和正文摘要。
- `read` 接受库内路径、唯一文件名、标题、别名，或索引里的 note id。`--block` 只输出那一块的源码。
- `backlinks` 只统计已经解析的入链。`--block` 再按 `^id` 筛选。
- `append` 在原文后空一行接上新段落，并更新索引。
- `new` 不带 `.md` 时补上。已存在则失败，不覆盖。`--template` 读取 `.aero/templates/<名称>.md`，模板名不能是路径。没有模板时写入一级标题。
- `index status` 对比已索引笔记数和磁盘上的笔记数。`rebuild` 清空后按磁盘重建。

自己的 `new` / `append` 会立刻写入索引。图形界面之外改过的文件，要 `index rebuild` 之后搜索和反链才看得到。

## 自动化

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

## 限制

- Aero 正在使用同一个库时，索引文件被锁住，命令打不开索引，需要先退出应用。
- `export` 不在这六个子命令里。
- 无参数启动的仍是窗口，不是帮助。帮助用 `aero --help`。
