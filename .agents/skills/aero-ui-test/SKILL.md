---
name: aero-ui-test
description: >-
  在真实的 Aero 窗口里做界面验收：启动 debug 版应用，注入点击、拖动、按键、输入，
  截窗口图，逐像素比对前后差异并测量位移。Use when verifying Aero UI or editor
  changes visually, checking that clicking a block causes no layout jump, reproducing
  mouse/keyboard/selection bugs, or taking screenshots of the running Aero app
  (界面验收、截图比对、点击跳变、拖选、录屏).
---

# Aero 界面验收

入口是 `scripts/aero-ui`。它启动 debug 版 Aero，经应用内的命令通道（`crates/aero-app/src/ui_channel.rs`，只在 debug 构建里）注入鼠标和键盘事件，用 `screencapture -l` 截窗口图，再用 `scripts/img.py` 比对。事件不经过系统，不动真实鼠标，也不需要窗口在最前。

适合：真实排版与字体、焦点、悬停、输入路由、点击前后是否跳变。
回归测试不要靠它：把验证过的行为写成无头测试（`crates/aero-app/src/editor/layout_tests.rs`，`VisualTestContext` + `debug_selector` / `cx.debug_bounds`），`cargo test` 能跑。

## 前提

- 每条命令都用 Shell 的 `required_permissions: ["all"]` 运行：沙箱里起不了图形应用，也截不了屏。
- 首次 `start` 会 `cargo build -p aero-app`；首次截图会在 `target/ui-test/venv` 装 pillow 和 numpy。
- 不需要用 `block_until_ms: 0` 把应用挂在后台：`start` 让应用脱离当前会话，这次调用结束后应用照样运行，后续调用接着驱动它。

## 快速开始

```sh
S=.agents/skills/aero-ui-test/scripts/aero-ui
$S start                      # 构建，用样例库启动，窗口停在屏幕右缘外，等到能接收事件
$S shot full && $S grid full  # 截整窗，另存一张叠了坐标网格的；用 Read 看它打印出来的路径读坐标
$S probe para 450 272         # 点一段文字：点击前后、Esc 退出后各比一次
$S stop                       # 用完一定要停
```

`probe` 期望的输出：

```
== 点击 (450, 272) 前后
相同
   编辑器：[DEBUG aero::editor] focus 24..235 id=Some(BlockId(2))
== 退出编辑后与点击前
相同
```

有差异时会列出区域（窗口点），并打印差异图的路径。

## 看图：用打印出来的路径

`shot`、`grid`、`crop`、`stack`、`diff`、`probe` 打印的图片路径每次都不一样（`shots/view/<名字>~<序号>.png`）。
Read 按路径缓存图片：同名截图重拍后直接读 `shots/<名字>.png` 会拿到旧图；刚写出的文件偶尔也要隔一两秒才读得到（报 File not found 就再读一次）。
命令之间引用图片仍用短名（`diff para.0 para.1`、`crop full cell …`），短名指向 `shots/<名字>.png` 里最新的那张。

## 坐标：最容易错的地方

- 所有坐标都是**窗口里的点**：窗口 1280×820，左上角原点，含标题栏。
- Read 看到的图是缩小过的（2560 像素宽的截图显示成约 1024 宽），**不要按图上的像素位置估坐标**，只读网格上的刻度（每 100pt 标数字，细线间隔 50pt）。
- 要精确位置就先裁小再叠网格：`$S shot cell 330 600 400 120 && $S grid cell 10`。裁剪后的图仍按窗口坐标标注，`diff` / `grid` / `color` 报的也是窗口坐标。
- 界面改了坐标就会变，每次都从新截的网格图里读，不要沿用旧数字。

## 窗口的两种状态

- **停放（默认）**：窗口只有 1pt 露在主屏右缘，不激活，不打扰用户；macOS 仍给它出帧，截图完整。窗口不活跃，**光标和选区都不画**，所以点击前后比对时光标不会造成差异。进没进编辑态看 `probe` 打印的 focus 日志，或 `$S log focus`。
- **`start --show`**：窗口居中并激活，看得到光标和选区，但会抢走用户的焦点。只在必须看光标、选区时用，看完马上 `stop`。
- 不要把窗口整个移出屏幕或让它被完全挡住：macOS 不再给它出帧，截到的是旧画面，点击也落在旧布局上。

## 常用流程

**点进去不跳**（浏览与编辑一致，见 `docs/project_plan.md` 9.1）：`start` → `shot` + `grid` 读出每类块的坐标 → 对每块 `probe`（标题、段落、列表、任务文字、单元格、引用、callout 标题与正文、代码块）→ 有差异就 `crop` 两张图、`stack` 拼起来看，`shift` 量整体位移。

**交互 bug**：`click` / `drag` / `key` / `type` 复现，`shot` 看结果；源码是否写对，直接读 `/tmp/aero-ui/vault/` 里的文件；应用日志 `$S log [正则]`（默认 `AERO_LOG=debug`）。

**看悬停效果**：`$S move x y && $S do "wait 600" && $S shot hover`（链接预览 500ms 后出现）。`probe` 退出编辑后会把指针晃一下，悬停手柄才能回到点击前的样子。

## 隔离与状态

- 所有东西在 `/tmp/aero-ui/`：`vault/`（样例库副本）、`data/`（`AERO_DATA_DIR`，应用状态与索引）、`shots/`、`app.log`。用户自己的 `~/Library/Application Support/Aero` 不受影响。
- 每次 `start` 都重新复制样例库、清空 `data/`；想接着上次的状态用 `start <库目录> --keep-data`。
- 样例库 `fixtures/vault/体验.md` 覆盖各类块（行内格式、标题、列表、任务、带长文字的表格、引用、callout、代码块），启动时打开它。别的场景另加笔记，不要改动已有内容，免得别人的坐标失效。
- 临时要文件夹、更多笔记：`start` 之后直接写进 `/tmp/aero-ui/vault/`，文件监听一两百毫秒内刷新文件树；下次 `start` 会还原。

## 其他注意

- 不要用 CGEvent、`cliclick` 之类从系统层发事件：事件会落到最前面的窗口（比如浏览器），停放的 Aero 收不到。
- 每批输入等应用回执后再等 150ms（`AERO_UI_SETTLE_MS`），截图前通常不必再等。
- 按键用 GPUI 写法：`escape`、`enter`、`backspace`、`cmd-z`、`shift-down`、`cmd-shift-k`。
- 报错“应用没有回应”或“找不到窗口”：先看 `$S log`，应用多半 panic 了。

命令全表、通道协议、环境变量见 [reference.md](reference.md)。
