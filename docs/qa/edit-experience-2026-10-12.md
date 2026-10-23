# 块交互体验实施与验收（2026-10-11—13）

需求基线为 [A01–A37 与 P1–P3](../edit_experience/edit_experience_request.md)，技术方案与阶段状态见 [project_plan.md §23](../project_plan.md#23-块交互体验技术方案2026-10-10)。本记录区分自动回归、真实窗口子集与未完成项，不代表整项需求已交付。

状态更新于 2026-10-23：下文日期明确的结果保留为历史证据；当前仍有 [G01–G11 实现差距](edit-experience-gap-review-2026-10-21.md)，不是只差验收。系统表情面板已在后续验证中实际弹出，见补充记录，旧“未显示”结果不再作为当前状态。

环境：Apple M5 Pro、48 GiB、macOS 27.0.1（26A434），Retina 2×。代码基线 `695a1316e1aba158da3aff604de2465895486f39` 加本任务未提交工作区修改。窗口验收使用 debug 构建、独立临时应用包和 `/tmp/aero-ui/vault`、`/tmp/aero-ui/data`，窗口 1280 × 820 逻辑点；10-11 的子集使用浅色主题，10-12 补测嵌套正文的深浅双主题。主题通过 debug 通道仅在测试应用内切换，不修改系统外观；未操作用户笔记库。此环境不满足 P1–P3 的 release／1440 × 900／双主题条件。

10-13 后续同步与失败恢复验收使用独立的 `/tmp/aero-ui-sync-recovery/vault`、`/tmp/aero-ui-sync-recovery/data`，相同窗口尺寸与 debug 构建。准备期取消／冲突夹具为 7 篇基础笔记加 100 篇不需改写的观察笔记，生成方法、字节数和全库基线指纹见 [夹具及原生检查](results/2026-10-13-sync-recovery/fixture-and-native-checks.json)。该夹具用于让真实准备过程足够长以注入取消／外部变化，不是性能采样。

## 已接入及自动回归范围

- 文件表达：正式文档／块身份、旧短 ID 作用域别名、歧义拒绝、YAML 局部补写、容器和列表项元数据归属、自定义颜色精度与通道优先级。
- 工作区操作：新身份复制及内部引用映射、单次剪切移动、原资源位置保留、所有关联文档版本校验、事务日志恢复、关联撤销。故障测试覆盖逐写入点失败、只读目标、提交期间外部修改及首次写入前的剪切恢复。
- 编辑操作：引用和高亮递归到正文叶块，整体容器操作保留子树；菜单／快捷键／斜杠共用转换校验。新增拆分回归覆盖嵌套行内格式、代码、链接、行内颜色、标题前后类型、主 ID／别名原属、列表层级与子树、撤销重做和自动保存。不同块样式仅移动光标并显示轻提示，不增加撤销项。
- 排版与组织：四种对齐共用字形偏移，命中／选区／光标／IME 使用相同布局；未保存源修订订阅、过期回复丢弃、共享读取／解析；逻辑文档树与子文档创建事务。系统表情入口的 Unicode 插入位置及一次撤销已自动验证，实际系统面板另列。

2026-10-11 完整回归：应用 610、应用链接集成 17、核心 235、核心集成 5、索引 80、反链集成 1、排版底座 925、底座元素集成 1，共 1,874 项通过。应用 2 项显式忽略：诊断开销手动测试、依赖已下载模型的 embedding 测试。原始输出见 [自动回归日志](results/2026-10-11-edit-experience/automated-2026-10-11.log)。之后新增的边界修复须使用最新日志确认，不能沿用旧计数。

2026-10-12 前一轮完整回归为 1,883 项通过，见 [当时日志](results/2026-10-11-edit-experience/automated-2026-10-12.log)。本次继续补齐空占位会话状态、多行首次粘贴及容器手柄命中，最新完整回归退出码 0：应用 **622**、应用链接集成 **17**、核心 **236**、核心集成 **5**、索引 **80**、反链集成 **1**、排版底座 **925**、底座元素集成 **1**，共 **1,887 项通过、0 失败、2 项显式忽略**（原因同上）。新增回归覆盖多个空占位保留不同样式、失焦返回、其他块编辑后锚点位移、多行粘贴的逐段继承与显式覆盖，以及从嵌套容器移动到手柄后仍命中容器。普通单行首次提交保持原输入客户端；组合输入不落盘，撤销可恢复原空占位样式。见 [最新完整日志](results/2026-10-12-edit-experience/automated.log)、[本次源码及测试二进制校验值](results/2026-10-12-edit-experience/build-evidence.json)。清理了误带入的无关格式改动，`git diff --check` 通过；测试应用完成后已停止。


2026-10-13 完整回归退出码 0：应用 **625**、应用链接集成 **17**、核心 **236**、核心集成 **5**、索引 **80**、反链集成 **1**、排版底座 **925**、底座元素集成 **1**，共 **1,890 项通过、0 失败、2 项显式忽略**。新增回归验证短列表与虚拟化长文档中整体／单项／正文的目标切换、单行列表下方类型按钮，以及文档树上／下／内部落点、跳过后代的同级排序、预览不改行高和 Escape／离开取消。子文档回归先复现“文件已撤销、标签页仍在”的失败，再验证关闭被事务删除的标签页、右侧另有标签时回到父文档并重做恢复相同文件字节。完整回归之后仅调整文件树预览线及拖动文字的主题颜色，最终源码另跑 **22 项文件树回归全部通过**，并完成原生双主题截图；未把这两处颜色改动声称为重新执行了完整回归。见 [完整日志](results/2026-10-13-edit-experience/automated.log)、[最终文件树回归](results/2026-10-13-edit-experience/tree-final.log)、[源码／构建／证据校验值](results/2026-10-13-edit-experience/build-evidence.json)。

2026-10-13 后续最终完整回归退出码 0：应用 **628**、应用链接集成 **17**、核心 **236**、核心集成 **5**、索引 **80**、反链集成 **1**、排版底座 **925**、底座元素集成 **1**，共 **1,893 项通过、0 失败、2 项显式忽略**。新增三项回归覆盖直接／嵌套来源删除及同字节／新版本恢复、撤销移除打开文档后继续执行已接受队列、以及目标在初次读取与全库扫描之间变化的确定性竞态。最后一项先复现旧粘贴结果覆盖外部新正文，再验证拒绝操作且不写入；原生窗口复测确认源内容恢复。已关闭子文档实体即使暂被观察者持有，也不参与之后的文件屏障；关联重做回归保留该引用验证这一点。见 [最终完整日志](results/2026-10-13-sync-recovery/automated.log)、[修复前竞态回归](results/2026-10-13-sync-recovery/paste-race-regression-before.log)、[最终源码／构建校验](results/2026-10-13-sync-recovery/build-evidence.json)。

## 真实窗口子集

使用仓库 `aero-ui-test` 技能驱动真实窗口，截图来自临时笔记；原生组字使用系统按键路径，由 WeType 3.0.0 产生候选窗，未用直接输入 Unicode 代替组字。

| 场景 | 操作与检查 | 结果及证据 |
| --- | --- | --- |
| 嵌套容器零跳变 | 外层高亮、引用正文、列表续段、内层高亮分别点击进入编辑，再 Escape 退出；确认焦点日志后比较完整窗口 | 4 处 × 2 次比较，精确变化像素均为 0；[像素记录](results/2026-10-11-edit-experience/pixel-comparison.json)、[夹具](results/2026-10-11-edit-experience/nested-fixture.md)。停放窗口排除光标闪烁；未使用颜色阈值放宽 |
| 类型标记菜单与颜色预览 | 从引用叶块类型图标打开菜单，输入 `rgba(20,40,60,0.123456)`；预览时核对磁盘，Escape 取消，重开确认并一次撤销 | 预览不写文件，确认仅改目标叶段属性，保留透明度原精度，撤销恢复原文；[预览图](results/2026-10-11-edit-experience/color-preview.png) |
| 已引用标题中间拆分（10-12） | 在加粗标题 `ab cd` 中间按 Enter，继续输入“新”，检查真实窗口及保存源码 | 前半为标题、后半为正文，两半均加粗、红色、居中和缩进 2；原块 ID 仅在前半；[截图](results/2026-10-11-edit-experience/split-typed.png)、[夹具](results/2026-10-11-edit-experience/split-fixture.md) |
| 空段落首次输入（10-12） | 在该标题末尾 Enter，空占位输入“空”，核对源码，再撤销输入与拆分并等待保存 | 新正文保留红色、居中、缩进 2，原身份和尾部正文完整；两次撤销后磁盘逐字恢复原文；[截图](results/2026-10-11-edit-experience/split-empty-typed.png)、[保存源码](results/2026-10-11-edit-experience/split-empty-typed.md) |
| 空占位失焦及多行粘贴（10-12 续） | 标题末尾 Enter、Escape，点其他段落后重新点空行；通过原生粘贴输入两段，其中末段显式设蓝色／右对齐；撤销后再点空行输入“再” | 继承红色／居中／缩进 2，末段蓝色／右对齐覆盖继承而保留缩进；再次输入恢复原继承样式；撤销输入及拆分逐字恢复夹具。原生粘贴工具返回后存在窗口失焦，重新点击占位再输入已验证；[粘贴图](results/2026-10-12-edit-experience/blank-multiline-pasted.png)、[保存源码](results/2026-10-12-edit-experience/blank-multiline-saved.md)、[再次输入](results/2026-10-12-edit-experience/blank-retyped.png) |
| 容器外缘手柄（10-12 续） | 悬停外层边框，移入六点手柄并点击；菜单移出后重新移入；深浅主题分别打开 | 修复原来的左侧裁切、不可见子按钮拦截及子段落抢走悬停目标；正确出现“解除容器”，操作不修改源码；[目标稳定](results/2026-10-12-edit-experience/container-gutter-stable.png)、[容器菜单](results/2026-10-12-edit-experience/container-menu.png)、[深色菜单](results/2026-10-12-edit-experience/container-menu-dark.png) |
| 拖出／拖入与整棵容器移动（10-12 续） | 将带身份／红色右对齐的内层正文移到容器外，再移入空内层容器；将整个外层容器移到尾段之后，再一次撤销 | 子块身份、显式样式及内部顺序保留；整棵移动一次撤销逐字恢复本次基线。正文移回时允许操作涉及的空行调整；[叶块移出](results/2026-10-12-edit-experience/drag-leaf-out.png)、[移入](results/2026-10-12-edit-experience/drag-leaf-in.png)、[整棵移动](results/2026-10-12-edit-experience/container-subtree-moved.png)、[移动前](results/2026-10-12-edit-experience/container-drag-baseline.md)、[移动后](results/2026-10-12-edit-experience/container-drag-moved.md) |
| 非法落点与 Escape（10-12 续） | 外层容器拖到自身后代；再次拖到外部落点，在松手前 Escape | 两次磁盘源码均逐字保持原基线；[取消前的预览](results/2026-10-12-edit-experience/container-drag-preview.png)。未据此覆盖跨文档事务的全部失败路径 |
| 文档树改挂／撤销／拒绝成环（10-12 续） | 子文档从父甲拖到父乙，打开子文档后一次撤销；父甲拖到其子文档 | 只改变 `aero.parent`，文件路径和正文不变，一次撤销逐字恢复原关系；成环操作被拒绝，父子源码均不变；[改挂](results/2026-10-12-edit-experience/logical-reparented.png)、[撤销](results/2026-10-12-edit-experience/logical-reparent-undo.png)、[拒绝成环](results/2026-10-12-edit-experience/logical-cycle-rejected.png) |
| 列表三种操作范围（10-13） | 在含 80 段尾部正文的虚拟化长文档中，分别从外缘窄列、列表标记列、实际正文打开菜单；删除列表整体／首项后一次撤销 | 整体删除全部列表，单项删除包含续段及嵌套项而保留第二项；两次撤销均逐字恢复夹具；正文菜单正确显示正文类型及源码入口。修复虚拟化后缺失整体手柄、叶块外伸区截走标记列、单行列表下方类型按钮失去目标；[整体菜单](results/2026-10-13-edit-experience/list-whole-menu.png)、[项菜单](results/2026-10-13-edit-experience/list-item-menu.png)、[正文菜单](results/2026-10-13-edit-experience/list-paragraph-menu.png)、[夹具](results/2026-10-13-edit-experience/list-fixture.md)、[整体删除源码](results/2026-10-13-edit-experience/list-deleted.md)、[项删除源码](results/2026-10-13-edit-experience/list-item-deleted.md) |
| 子文档创建及多标签页撤销／重做（10-13） | 在两段正文之间执行 `/子文档`；从新建子文档撤销，随后从父文档重做，再撤销 | 仅创建一个文件，父链接位置、子文档身份／父关系／顺序正确，自动打开并在侧栏归入父文档。修复内部事务删除被误留为外部删除标签页，以及右侧另有标签时焦点跳离父文档的问题；撤销恢复全部文件字节，重做保留相同身份与文件字节，关联操作可继续使用；[创建](results/2026-10-13-edit-experience/child-created.png)、[撤销回到父文档](results/2026-10-13-edit-experience/child-undo.png)、[父文档重做](results/2026-10-13-edit-experience/child-redo.png)、[父文档创建前](results/2026-10-13-edit-experience/child-parent-before.md)、[创建后](results/2026-10-13-edit-experience/child-parent-created.md)、[子文档](results/2026-10-13-edit-experience/child-created.md) |
| 文档树同级排序／撤销／取消（10-13） | 将“排序丙”拖到“排序甲”上边缘，从移动文档一次撤销；再次拖动后 Escape | 实际顺序与边缘落点一致，未误改挂为子文档。由于原库部分同级缺少顺序值，首次排序补齐同级 order；只增加／修改顺序元数据，正文及已有身份保持原字节。一次撤销恢复全库文件，Escape 不写入。修复 GPUI 拖动回调对非命中行也触发时覆盖落点的问题；[排序](results/2026-10-13-edit-experience/tree-reordered.png)、[撤销](results/2026-10-13-edit-experience/tree-undo.png)、[顺序及受影响同级](results/2026-10-13-edit-experience/sort-orders.json) |
| 文档树预览及列表菜单双主题（10-13） | 分别停在行上缘／中间／下缘；深浅主题观察插入线；打开深色列表整体菜单 | 插入线与改挂框清晰可见，深色拖动标签使用正确文字颜色；所有预览取消后全库源码保持基线。修复原预览采用近似背景色的问题；[浅色插入线](results/2026-10-13-edit-experience/tree-preview-light.png)、[改挂框](results/2026-10-13-edit-experience/tree-preview-inside.png)、[下方插入线](results/2026-10-13-edit-experience/tree-preview-after.png)、[深色插入线及拖动标签](results/2026-10-13-edit-experience/tree-preview-dark.png)、[深色列表整体菜单](results/2026-10-13-edit-experience/list-whole-menu-dark.png)。未覆盖全部列表操作的双主题矩阵 |
| 打开的同步源删除／恢复（10-13 后续） | 来源保持在另一标签页，目标同时含直接及两层嵌套引用；删除来源，再分别恢复相同字节及新正文，深浅主题均观察 | 删除后两处引用显示缺失提示，不显示编辑器保留的旧正文；恢复相同字节也重新发布来源修订，恢复新版本则两处显示新正文。目标及其他文件不变，恢复缓冲由自动回归验证保留；[浅色缺失](results/2026-10-13-sync-recovery/source-deleted-light.png)、[浅色相同恢复](results/2026-10-13-sync-recovery/source-restored-same-light.png)、[浅色新版本](results/2026-10-13-sync-recovery/source-restored-new-light.png)、[深色缺失](results/2026-10-13-sync-recovery/source-deleted-dark.png)、[深色相同恢复](results/2026-10-13-sync-recovery/source-restored-same-dark.png)、[深色新版本](results/2026-10-13-sync-recovery/source-restored-new-dark.png) |
| 跨文档剪切遇到只读目标（10-13 后续） | 确认源段落已剪下；目标设只读后粘贴；解除只读，再用同一剪贴板粘贴并一次撤销 | 提示只读，源和目标均逐字恢复。重试生成新块身份的副本，原来源不变；一次撤销恢复全部原文。最终构建在含 107 篇笔记的夹具中复测失败恢复，所有文件字节一致；[只读失败](results/2026-10-13-sync-recovery/readonly-failure.png)、[重试](results/2026-10-13-sync-recovery/paste-retry.png)、[重试保存源码](results/2026-10-13-sync-recovery/paste-after-failure.md) |
| 跨文档准备期间取消（10-13 后续） | 确认剪切源已删除目标段；在包含 100 篇观察文档的粘贴准备过程中按 Escape | 真实窗口出现处理中反馈及取消通知，源恢复，目标不变；全库 107 篇笔记与本次基线逐字一致，界面可继续操作；[处理中](results/2026-10-13-sync-recovery/cancel-pending.png)、[取消后](results/2026-10-13-sync-recovery/cancel-complete.png)、[取消前已剪下的源](results/2026-10-13-sync-recovery/cancel-source-cut.md)。未据此推断 100 ms 反馈或其他性能预算达标 |
| 目标在准备期间被外部修改（10-13 后续） | 源段落剪下后开始粘贴；后台扫描尚未完成时写入目标新正文 | 首次原生验收发现新正文会被旧结果覆盖；已修复初次编辑快照与后续扫描快照不一致却继续提交的问题。最终构建拒绝粘贴、显示新目标正文，源及全部观察文档恢复，外部新字节完整保留；[修复前](results/2026-10-13-sync-recovery/external-race-before.png)、[修复后](results/2026-10-13-sync-recovery/external-change-rejected.png)、[保留的新正文](results/2026-10-13-sync-recovery/external-destination.md) |
| 双主题嵌套正文零跳变（10-12 续） | 外层正文、引用、列表续段、带身份的内层正文分别点击／Escape；每次核对焦点日志 | 2 主题 × 4 场景 × 2 比较，正文及容器全区域 `(260,100,740,460)` 精确变化像素均为 0。该区域内无遮罩、不放宽阈值；反链区随带身份段落焦点切换，状态栏保存计时也会变化，均在所比较正文区域之外；[像素记录](results/2026-10-12-edit-experience/pixel-comparison.json)、[操作及焦点日志](results/2026-10-12-edit-experience/theme-probes.log) |
| 引用正文原生中文输入 | 输入 `n`、`i`，确认候选窗和标记文字；提交“你”，检查磁盘；一次撤销；重新组字后 Escape | 组字未落盘，提交落在指定引用段，撤销与取消恢复原文；此场景未另存候选窗截图 |
| 居中长段原生中文输入 | 自动换行后的行末组字，检查候选窗；提交并撤销 | 通过；[组字截图](results/2026-10-11-edit-experience/native-center-preedit.png)、[候选窗](results/2026-10-11-edit-experience/native-center-candidates.png)、[坐标](results/2026-10-11-edit-experience/native-center-geometry.json) |
| 两端对齐原生中文输入 | 滚动后在段落中途组字；核对全文件未变；提交“你”仅将“用于验证”改为“用于验你证”；一次撤销并重新组字取消 | 通过；[组字截图](results/2026-10-11-edit-experience/native-justify-preedit.png)、[候选窗](results/2026-10-11-edit-experience/native-justify-candidates.png)、[坐标](results/2026-10-11-edit-experience/native-justify-geometry.json) |
| 右对齐原生中文输入 | 行末组字，确认真实候选窗和全文件未变，Escape 取消 | 通过该子集；[组字截图](results/2026-10-11-edit-experience/native-right-preedit.png)、[候选窗](results/2026-10-11-edit-experience/native-right-candidates.png)。此场景未验证提交／重做 |
| 系统表情面板 | `/表情` 删除命令、恢复插入位置，经过一次完整绘制后的第二个帧边界调用系统入口；另尝试原生快捷键 | **未通过原生验收**：命令删除及插入点恢复正常，停放／显示窗口与原生快捷键均未观察到字符面板；同环境下“文本编辑”的原生“表情与符号”菜单也未显示面板，但尚不能据此确定根因。不能计为可选取表情；[调用前](results/2026-10-12-edit-experience/emoji-command.png)、[调用后](results/2026-10-12-edit-experience/emoji-command-complete.png) |

对齐夹具见 [alignment-fixture.md](results/2026-10-11-edit-experience/alignment-fixture.md)，包含中英混排、组合重音与家庭表情。居中候选窗左上角为窗口内 `(861, 515)`，两端对齐为 `(520, 614)`，均位于该次组字位置下方；这不是全部光标位置的误差测量。

10-13 继续排查 A19：使用独立应用包，将窗口显示并激活后执行 `/表情`，以及真正的系统快捷键。确认系统 `CharacterPalette` 进程启动，但仍没有可选取字符的窗口；通过原生辅助功能读取该系统应用两次均超时，尚不能确定根因。命令删除及插入点恢复仍正常；[激活窗口调用后](results/2026-10-13-edit-experience/emoji-shown-complete.png)。未改动系统输入法／安全设置，未将进程启动计为面板验收通过。测试应用已停止，临时夹具全部恢复。

## 系统表情补充记录（2026-10-23）

本节补记本会话前一轮已执行的功能验证，不表示 2026-10-23 文档更新重新运行了测试。相关代码现已包含于 `e9d6172`。

- 已接入“编辑 → 表情与符号”、命令面板、`⌃⌘Space` 和 `/表情`／`/emoji` 入口；斜杠命令保留周围空白，延迟打开前核对原输入控件、焦点和选区。
- 字素回归覆盖肤色、ZWJ、旗帜、组合重音、跨 Rope chunk 边界、双击选择、前后删除和隐藏格式标记；命令面板集成回归覆盖选区恢复、Unicode 替换、撤销／重做和保存。不能将这些结果外推为 G10 的自动换行及原生候选窗矩阵通过。
- 实际执行 `cargo test -p gpui-base -p aero-app --no-fail-fast`，结果如下；随后 `cargo build -p aero-app` 与 `git diff --check` 通过。仅有依赖 `block 0.1.6` 的既有 future-incompatibility 警告。

| 测试范围 | 通过 | 失败 | 忽略 |
| --- | ---: | ---: | ---: |
| aero-app 单元／界面回归 | 634 | 0 | 2 |
| aero-app text_links 集成 | 17 | 0 | 0 |
| gpui-base 单元回归 | 927 | 0 | 0 |
| gpui-base element_ext 集成 | 1 | 0 | 0 |
| 合计 | 1,579 | 0 | 2 |

两项忽略分别为诊断开销手动测量、需要已下载模型的 embedding 测试。此次未包含 `aero-core` 和 `aero-index`，与 10-13 的 1,893 项测试范围不同，不视为全仓库重新验收，也不以计数变化推断退化或需求完成率。

原生验证使用隔离目录 `/tmp/aero-ui-emoji-f38aa646`、debug 构建及 1280 × 820 窗口，已看到系统“表情与符号”面板实际弹出，并核对打开前后测试文档不变，不只是看到 `CharacterPalette` 进程。弹窗截图来自带临时诊断日志的验证构建，最终代码已移除诊断日志；本地临时截图为 `/tmp/aero-ui-emoji-f38aa646/shots/native-character-palette.png`，未归档到仓库，不作为完整 A19 的持久化验收包。

系统拒绝 `osascript` 辅助访问，未自动点选面板中的表情，也没有修改系统权限。**当前 A19 状态为入口／字素代码已实现，原生弹出子集通过，真实点选至插入、取消、删除与撤销仍待手工验收。** 测试应用已停止；10-12／13 的“未显示”记录仅保留为历史现象。

## 后续代码审查

2026-10-21 初次审查对照 `0baae89` 加当时暂存修改；这些代码现已进入 `e9d6172`。2026-10-23 沿用同一份 [G01–G11 实现差距与关闭条件](edit-experience-gap-review-2026-10-21.md)，同步阶段状态，不另建一份重复清单。

内容无损、子块操作范围、容器边界、颜色通道、列表层级、同步身份、子文档、对齐和事务准备期仍有实现缺口，不是全部功能已完成、只需补截图。尤其已有只读目标／外部目标变化恢复测试，不覆盖 G11 中普通预保存及日志发布之前的全部中断窗口。

初次复核的 53 个历史源码指纹中有 49 个一致，其余 4 个属于系统表情和字素修改路径；该核对用于界定历史证据，不能据此证明全部需求通过。本次仅更新文档，未逐项新跑 G01–G11 的运行时复现、完整测试或原生场景；不能把代码阅读结论记成新的界面失败。

## 尚未关闭的交付门槛

1. **实现缺口 G01–G11**：按差距清单先补失败回归、修复、再验证。容器内部空位／末尾空位、选中后输入及跨边界删除不能仅凭已有顶层空占位测试通过而关闭 A22／A27。
2. **真实窗口完整矩阵**：保留列表目标、子文档事务、排序、同步删除恢复、取消／只读目标／外部变化恢复和双主题零跳变子集；继续覆盖只读来源、多窗口同时编辑、完整嵌套操作区／主题／输入法组合，以及新增修复的场景，不替代全部 A01–A37。
3. **原生输入法**：系统简体拼音、日文，以及 WeType 在调整窗口、屏幕底部、跨笔记失焦和全部标记边界的行为仍待验证；[IME 清单](ime.md) 保持未完成。
4. **A19**：原生面板弹出已观察；补完真实面板点选、原插入位置、取消不改正文、删除与撤销／重做，并归档可追溯证据。
5. **P1–P3**：尚未完成规定 release／窗口／双主题条件下的夹具、200 次采样、30 秒滚动／拖动、共享源传播和发布基线 RSS 对照；不从测试耗时、诊断函数耗时或 debug 截图推断达标。
6. **缓存及内存**：磁盘源及 Markdown AST 缓存各有 128 项／32 MiB **源字节**上限，已有同修订共享、失效、条目／字节上限、超大源不缓存、旧版本计数回收与活动读取者淘汰回归。进程内存峰值和稳定内存相对 20% 门槛仍待实测，源字节上限不是 AST 或 RSS 上限。

复跑自动回归：`cargo test --locked -p aero-app -p aero-core -p aero-index -p gpui-base`。真实窗口复跑遵循 `.agents/skills/aero-ui-test/SKILL.md`，使用临时库，完成后停止测试应用。
