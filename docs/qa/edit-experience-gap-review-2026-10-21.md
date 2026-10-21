# 块交互体验实现差距与代码审查（2026-10-21）

本记录对照 [A01–A37 与 P1–P3 需求基线](../edit_experience/edit_experience_request.md#15-验收标准)检查当前代码，区分已经接入但仍需验证、部分实现且需要修正、尚未实现三类状态。它是代码阅读结论，不替代自动回归、真实窗口验收或性能采样。

## 审查范围与证据边界

- 代码基线：`0baae8973904400517246ac3f562eaff9607ee1a` 加 2026-10-21 当前暂存修改；暂存区涉及 9 个文件，共增加 302 行、删除 27 行。
- 需求基线：[交互体验需求及已定决策](../edit_experience/edit_experience_request.md)，重点检查 A01–A37 与 P1–P3。
- 历史验收：[2026-10-11—13 实施与验收记录](edit-experience-2026-10-12.md)及其证据文件。
- 已核对历史验收记录使用的 53 个源码指纹，其中 49 个与当前一致，另外 4 个属于本轮系统字符面板与 Unicode 字素修正涉及的文件。49 个未变化文件对应的历史子集证据继续保留；发生变化的路径必须按当前代码重新验证。
- `results/2026-10-13-edit-experience/build-evidence.json` 与 `results/2026-10-13-sync-recovery/build-evidence.json` 保留了“A32 表情面板”的历史编号笔误。按当前需求基线，系统字符面板是 A19，A32 是子文档创建事务；原始证据文件不回写。
- 本轮没有复跑完整自动测试、真实窗口矩阵或 P1–P3 性能采样。因此，下列代码缺口不能反推历史子集失败，历史子集通过也不能证明完整需求已经交付。

## 状态定义

| 状态 | 含义 |
| --- | --- |
| 代码已接入，待验证 | 已找到预期实现路径，但缺少当前基线下的完整自动或真实界面证据 |
| 部分实现，待修正 | 主流程已经存在，但至少一个需求分支会产生错误结果或丢失信息 |
| 尚未实现 | 当前代码没有需求要求的入口、反馈或处理分支 |

## 需要继续开发或修正

| 优先级 | 项目 | 当前状态 | 发现与影响 | 关联需求 | 代码证据 |
| --- | --- | --- | --- | --- | --- |
| 高 | 无损拖拽与转换 | 部分实现，待修正 | 普通拖动完成后会压缩整篇文档的连续空行，可能改写操作范围外的代码块；引用内转换为多行代码时没有为续行完整补回容器前缀。 | A04、A07、A20 | [`ops.rs:653–706`](../../crates/aero-app/src/editor/ops.rs#L653-L706)、[`ops.rs:923–951`](../../crates/aero-app/src/editor/ops.rs#L923-L951) |
| 高 | 子块操作范围 | 部分实现，待修正；直接输入尚未实现 | 部分列表正文拖动以及键盘复制、剪切、删除仍取整个列表项范围；选中容器后直接输入，没有进入第一个可编辑子块。 | A02、A04、A14、A27、A28 | [`ops.rs:342–359`](../../crates/aero-app/src/editor/ops.rs#L342-L359)、[`ops.rs:427–439`](../../crates/aero-app/src/editor/ops.rs#L427-L439)、[`mod.rs:1617–1640`](../../crates/aero-app/src/editor/mod.rs#L1617-L1640)、[`mod.rs:1702–1715`](../../crates/aero-app/src/editor/mod.rs#L1702-L1715)、[`mod.rs:2406–2411`](../../crates/aero-app/src/editor/mod.rs#L2406-L2411) |
| 高 | 容器连续编辑与嵌套移动 | 代码已接入，待验证；部分实现，待修正 | 空容器占位、末尾退出和首部 Backspace 已有处理路径，但仍需按 A27 做真实窗口边界验证。将内容移入嵌套高亮容器时，当前移动计划没有同步调整所有祖先围栏长度。 | A04、A08、A20、A27 | [`units.rs:132–139`](../../crates/aero-app/src/editor/units.rs#L132-L139)、[`ops.rs:25–31`](../../crates/aero-app/src/editor/ops.rs#L25-L31)、[`ops.rs:182–192`](../../crates/aero-app/src/editor/ops.rs#L182-L192)、[`mod.rs:2272–2290`](../../crates/aero-app/src/editor/mod.rs#L2272-L2290)、[`ops.rs:484–555`](../../crates/aero-app/src/editor/ops.rs#L484-L555)、[`containers.rs:227–257`](../../crates/aero-core/src/containers.rs#L227-L257) |
| 高 | 行内自定义颜色 | 部分实现，待修正 | 行内自定义颜色可以写入，但正文样式解析和 presenter 只消费预设颜色类；临时预览可见，确认后正文主视图不会显示自定义色。扩大配色选区覆盖纯颜色载体时会删除整个载体，只重建当前颜色通道，可能清掉另一通道。 | A10、A11、A34 | [`colors.rs:68–76`](../../crates/aero-core/src/colors.rs#L68-L76)、[`markup.rs:316–329`](../../crates/aero-core/src/markup.rs#L316-L329)、[`presenter.rs:410–420`](../../crates/aero-app/src/editor/presenter.rs#L410-L420)、[`colors_ui.rs:730–740`](../../crates/aero-app/src/editor/colors_ui.rs#L730-L740)、[`colors.rs:425–565`](../../crates/aero-core/src/colors.rs#L425-L565) |
| 中 | 取色与恢复默认 | 代码已接入，待验证；部分实现，待修正；无效旧值提示尚未实现 | 块目标的自定义色预览已接入；行内目标选择“应用到整块”时，确认写入已接入，但预览被排除。已有无效色值会被静默忽略，只对当前新输入显示错误。普通选中 `==高亮==` 内正文时，“默认背景”不产生清除编辑；只有把两侧标记一并选中才能清除。 | A10、A11、A34 | [`colors_ui.rs:480–500`](../../crates/aero-app/src/editor/colors_ui.rs#L480-L500)、[`colors_ui.rs:693–740`](../../crates/aero-app/src/editor/colors_ui.rs#L693-L740)、[`mod.rs:3600–3602`](../../crates/aero-app/src/editor/mod.rs#L3600-L3602)、[`colors.rs:43–57`](../../crates/aero-core/src/colors.rs#L43-L57)、[`colors.rs:287–353`](../../crates/aero-core/src/colors.rs#L287-L353)、[`colors.rs:517–539`](../../crates/aero-core/src/colors.rs#L517-L539) |
| 中 | 列表缩进 | 部分实现，待修正 | 列表项菜单仍写入视觉 `indent`，没有执行需求规定的列表层级增加／减少；Tab 结构缩进和最小可编辑宽度下的禁用规则仍需补齐。 | A12、A28 | [`block_menu.rs:401–404`](../../crates/aero-app/src/editor/block_menu.rs#L401-L404)、[`block_menu.rs:591–608`](../../crates/aero-app/src/editor/block_menu.rs#L591-L608) |
| 高 | 同步引用与身份 | 部分实现，待修正 | 跨文档同步插入把“为来源补 ID”和“向目标插入引用”放进同一关联事务，撤销目标插入时会同时移除来源 ID，违背来源身份应独立保留的约束。复制同步引用后，剪贴板规范化还会把可解析的 `id:` 地址改写为文件路径。 | A29、A30、A33 | [`sync_picker.rs:184–218`](../../crates/aero-app/src/editor/sync_picker.rs#L184-L218)、[`history.rs:103–110`](../../crates/aero-app/src/workspace/history.rs#L103-L110)、[`clipboard.rs:210–235`](../../crates/aero-core/src/clipboard.rs#L210-L235) |
| 中 | 链接与同步异常展示 | 部分实现，待修正；异常入口尚未实现 | 段落链接正文仍按普通链接显示；悬停预览已有动态文档标题和摘要，不能将该能力记为完全缺失。同步来源冲突没有透传到引用状态；循环或超过 3 跳的占位没有继续前往来源的入口。 | A16、A30 | [`toolbar.rs:204–222`](../../crates/aero-app/src/editor/toolbar.rs#L204-L222)、[`preview.rs:93–249`](../../crates/aero-app/src/editor/preview.rs#L93-L249)、[`link_preview.rs:632–650`](../../crates/aero-app/src/editor/link_preview.rs#L632-L650)、[`live_sources.rs:101–129`](../../crates/aero-app/src/editor/live_sources.rs#L101-L129)、[`preview.rs:371–451`](../../crates/aero-app/src/editor/preview.rs#L371-L451) |
| 中 | 子文档与身份冲突处理 | 尚未实现；部分实现，待修正 | 文件导入仍保留原字节，文件树没有完整的“作为副本导入”入口。创建子文档后会打开新文件，但不会强制切换到文档视图。旧同级没有顺序值时，有序新项会排在无序项之前，不能保证追加到末尾。 | A31、A32、A35 | [`files.rs:300–317`](../../crates/aero-app/src/workspace/files.rs#L300-L317)、[`files.rs:380–405`](../../crates/aero-app/src/workspace/files.rs#L380-L405)、[`file_tree.rs:1303–1317`](../../crates/aero-app/src/file_tree.rs#L1303-L1317)、[`workspace.rs:590–595`](../../crates/aero-app/src/workspace.rs#L590-L595)、[`relations.rs:77–84`](../../crates/aero-core/src/relations.rs#L77-L84)、[`relations.rs:147–185`](../../crates/aero-core/src/relations.rs#L147-L185) |
| 高 | 四种对齐的边界 | 部分实现，待修正 | 公式和 HTML 换行绕过统一 Textarea 布局；自动折行仍按字符切分，只有两端对齐分支按字素处理；跨折行选区没有统一逐行应用对齐偏移。四种对齐主体已经接入，但不能据此关闭 A13/A37。 | A13、A37 | [`presenter.rs:165–172`](../../crates/aero-app/src/editor/presenter.rs#L165-L172)、[`text_wrapper.rs:220–249`](../../forks/gpui-base/src/input/editor/display_map/text_wrapper.rs#L220-L249)、[`text_wrapper.rs:877–924`](../../forks/gpui-base/src/input/editor/display_map/text_wrapper.rs#L877-L924)、[`element.rs:650–699`](../../forks/gpui-base/src/input/base/element.rs#L650-L699) |
| 高 | 事务准备期保护 | 部分实现，待修正 | 内部剪切先删除来源正文；工作区随后执行普通保存，再启动有关联日志的事务。恢复日志建立前存在进程中断空档。事务前的普通保存遇到外部变化会备份后覆盖，没有完整沿用跨文档事务的“冲突即拒绝”约束。 | A25、A26 | [`clipboard_ui.rs:28–87`](../../crates/aero-app/src/editor/clipboard_ui.rs#L28-L87)、[`workspace.rs:1591–1615`](../../crates/aero-app/src/workspace.rs#L1591-L1615)、[`transaction.rs:167–203`](../../crates/aero-core/src/transaction.rs#L167-L203)、[`io.rs:223–239`](../../crates/aero-app/src/editor/io.rs#L223-L239) |

## 已有实现，但尚未完成验收

以下项目属于交付证据缺口，不能直接归类为功能缺失，也不能从已有自动测试或历史窗口子集推断已经达标。

1. **完整 A01–A37 真实界面矩阵。** 现有记录覆盖容器、列表、子文档、同步恢复和部分输入法场景，但只读来源、多窗口同时编辑、完整嵌套操作区、双主题与输入法组合仍未逐项留证。详见[历史 QA 的未关闭门槛](edit-experience-2026-10-12.md#尚未关闭的交付门槛)。
2. **P1–P3 性能门槛。** 规定规模下的 200 次延迟采样、30 秒滚动／拖动帧间隔、同步传播、事务反馈和发布基线 RSS 对照尚未完成。现有 CPU 微基准和 debug 窗口记录不能关闭这些指标。
3. **原生输入。** 系统简体拼音、日文输入法及完整窗口位置／切换矩阵仍未完成；系统字符面板入口和字素处理已有代码增强，但真实面板内点选、插入位置、删除与撤销仍需人工验收。详见 [IME 清单](ime.md)及 A19。

## 修复与验收顺序

1. 先修复会改写无关内容、扩大操作范围、破坏身份或留下事务恢复空档的问题。
2. 再修复颜色解析／恢复默认、列表层级、同步异常反馈和子文档入口。
3. 为每个修复补充最小自动回归，并复跑当前受影响 crate；发生变化的历史指纹路径不能沿用旧日志作为当前基线证据。
4. 代码稳定后，集中执行 A01–A37 真实界面矩阵、原生 IME／字符面板和 P1–P3 性能验收。

## 结论

当前代码已经接入大类功能，但上述 11 类问题中没有一类可以仅凭代码存在认定为完整交付。项目状态应保持为“部分实现并持续修正，完整交付门槛未关闭”；后续状态更新必须同时引用修复后的自动回归、真实窗口证据或性能数据。

本次文档更新的范围、编辑摘要和一致性校验见 [文档审阅报告](results/2026-10-21-edit-experience-gap-review/document-review.html)。
