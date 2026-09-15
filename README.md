# Aero
A blazing fast, AI Native, local-first knowledge IDE.

## 运行（M1a 开发版）

```sh
# 打开一个文件夹作为 vault；不带参数则进入首次运行页 / 最近打开的 vault
cargo run -p aero-app -- ~/Knowledge

# 日志（默认只输出 warn）
AERO_LOG=debug cargo run -p aero-app -- ~/Knowledge

# 测试：aero-core 往返测试 + aero-app 的纯函数与无头 GPUI 交互测试
cargo test -p aero-core -p aero-app
```

- 设计文档：`docs/project_plan.md`（技术方案）、`docs/ui_design.md`（UI 与交互）。
- 代码分层：`crates/aero-core`（文本 / 块 / 解析 / vault，无 UI）→ `crates/aero-app`（GPUI 窗口、文件树、块编辑器、自动保存）。
- 快捷键（M1a）：`⌘O` 打开 vault，`⌘N` 新建笔记，`⌘S` 保存，`⌘\` / `⌘⇧\` 折叠两侧面板，`⏎` 拆块 / 续列表，`⇧⏎` 软换行，`Esc` 回到浏览态，`⌘Z` / `⌘⇧Z` 文档级撤销重做。

---

tar zcvf aero.m0.aero-core.tar.gz --exclude=Aero/target Aero/

docker run -d --name aero --hostname aero -v "$HOME":"/workspace" -w "/workspace" ubuntu:24.04 sleep infinity
