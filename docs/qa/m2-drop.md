# M2 拖放导入验收

更新日期：2026-09-22。对应 [UI 设计 7.6](../ui_design.md)。粘贴导入见 [图片验收](m2-images.md)。

已交付：把文件拖进编辑区。Finder 文件按粘贴规则收进当前笔记的 `.assets/`；文件树里的笔记插入链接。
还没做：把 Finder 文件拖到文件树上（`.md` 复制进库，其他文件进目标笔记的 `.assets/`）。

## 范围

- 从 Finder 拖到编辑区，和 `⌘V` 同一套规则：图片 / PDF 复制后插入 `![[文件名]]`，其他文件插入普通链接。重名追加 `-2`，不覆盖。文件夹跳过，不弹错误。
- 从文件树把一篇笔记拖到编辑区，插入 `[[路径]]`（去掉 `.md`）。按住 ⌥ 插入 `![[路径]]`。文件夹、别的库的条目不接收。
- 拖过编辑区时有高亮。插入位置与粘贴相同：正在编辑就放在光标处，否则接到文末。一次插入是一个撤销组；撤销只回退源码，不删已经落盘的附件。
- 预览里点普通文件链接时，打开 `.assets/` 里的真实文件（`file://` 且做百分号编码）。相对路径不再直接交给系统，避免 macOS 报「应用程序无法打开 -50」。网页链接仍用系统浏览器打开。

## 自动化

新增 2 项回归，全量 **514 项通过**（app 285、链接绘制集成 17、core 156、往返 5、index 50、反链计数集成 1）。

- `dropping_files_onto_the_editor_imports_them`：拖入图片和 zip，文件夹被跳过。
- `dropping_a_tree_note_onto_the_editor_inserts_a_link`：从文件树拖到编辑区得到 `[[a]]`，按住 ⌥ 得到 `![[a]]`。

```sh
cargo test --locked --offline -p aero-app --bin aero dropping_files_onto_the_editor
cargo test --locked --offline -p aero-app --bin aero dropping_a_tree_note
```
