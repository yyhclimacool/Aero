# ADR 0001 · UI 底座：GPUI Kit

- 状态：**已采纳**（2026-09-15，spike 一天完成，原限时一周）
- 关联：`project_plan.md` 3.1 / 18 M0；`spikes/gpui-ime/`

## 背景

方案选定 GPUI + gpui-component 作为 UI 层。写方案时（2026-09-14）的假设是"gpui-component 官方仍推荐 git 依赖、需要锁 rev"。spike 启动时核实：上游已重组为 **GPUI Kit**（`gpui-kit`，crates.io），它自带匹配版本的 GPUI（以 `gpui-pre` 名义发布在 crates.io），应用只需一个依赖，不再需要 git 依赖。

## 已确认的事实（2026-09-15）

| 项 | 结果 |
| --- | --- |
| 依赖 | `gpui-kit = "=0.6.1"`（2026-09-09 发布）→ `gpui-pre 0.3.5`、`gpui-base 0.6.1`、`gpui-component 0.6.1` |
| 工具链 | rustup stable 1.98.1（aarch64-apple-darwin）；Xcode 27.0 + Metal Toolchain 27A266a（Xcode 26 起需单独 `xcodebuild -downloadComponent MetalToolchain`，下载后 `xcrun --kill-cache`） |
| 首次全量编译 | 852 个包，3 分 24 秒（Apple Silicon，dev profile，依赖 O2）；增量编译数秒 |
| 编译告警 | 我方代码 0；依赖 `block 0.1.6` 有 future-incompat 提示 |
| 开窗 | 1100×720 居中窗口正常，无 panic；多次运行仅有 macOS InputMethodKit 的 `IMKCFRunLoopWakeUpReliable` 系统日志（Zed/VS Code 同样会打，可忽略） |
| `rust-version` | 不能在 workspace 里随手声明——会触发 MSRV 感知解析，把 wgpu/cosmic-text 等锁到旧版本 |
| 关键 API | `TextareaState`（多行）与 `InputState`（单行）分离；`InputEvent::{Change, PressEnter, Focus, Blur}`；`v_virtual_list(view, id, Rc<Vec<Size<Pixels>>>, render_range)` |

## 手工验收结果（macOS 26.6.2，系统简体拼音 + 第三方输入法 + 日文）

| # | 项 | 结果 |
| --- | --- | --- |
| 1 | 编译运行 | 通过 |
| 2 | 中文渲染（含全角标点、emoji、连字） | 通过 |
| 3 | VirtualList 10k 行滚动 | 通过（修复后，见下） |
| 4 | 组合中下划线显示 | 通过 |
| 5 | 候选框跟随光标（换行、滚动后） | 通过 |
| 6 | 提交正确性，状态行数值一致 | 通过 |
| 7 | 组合中 Esc / 方向键 / Backspace | 通过 |
| 8 | 跨语法边界（`**` 内、`[[` 后、`#` 后） | 通过 |
| 9 | 组合期间 `Change` 是否触发 | 通过（未记录触发频率；M1a 接重解析时再观测一次日志，决定组合态是否需要防抖） |
| 10 | 单行 vs 多行 IME 一致 | 通过 |
| 11 | 组合中切换输入法 | 通过 |
| 12 | ⌘Q / 关窗退出 | 通过 |

## 踩到的坑（M1a 会反复遇到）

- **VirtualList 必须由视图持有 `VirtualListScrollHandle` 并传给 `.track_scroll()`**。`v_virtual_list()` 内部每帧新建临时 handle，滚动偏移存在 handle 里，不传持久 handle 时偏移每帧归零，表现为"列表不能滚"。滚动条用容器的 `.vertical_scrollbar(&handle)`（`scroll::ScrollableElement`）。文件树、搜索结果、反链面板都要照此办理。
- 沙箱环境下首次构建需要写 `~/.cargo/registry`，必须在沙箱外执行。

## 决定

- **采用 GPUI Kit**。`aero-app` 直接依赖 `gpui-kit`，版本在根 `Cargo.toml` 的 `[workspace.dependencies]` 精确锁定（`=0.6.1`），升级按季度作为独立任务，不顺带升级。
- 聚焦块的编辑控件（Phase 1）直接用 `gpui-base` 的 `TextareaState` / `InputBaseState` 系列，IME 由其负责；Aero 不自建输入元素。
- 方案中"GPUI 处于 pre-1.0、git 依赖、锁 rev"的表述全部改为"crates.io 精确锁版本"。

## 对方案的修正

- `project_plan.md` 3.1、18 M0、21 风险表中关于 git 依赖 / 锁 rev 的表述已同步修改。
- `spikes/gpui-ime/` 保留到 `aero-app` 开出第一个窗口为止（作为唯一可运行的 GPUI 参考），之后删除。
