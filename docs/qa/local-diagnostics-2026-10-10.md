# 本地日志与诊断验收（2026-10-10）

方案先写入 `docs/project_plan.md` 16.1，随后实现。所有构建、单元 / 交互 / 子进程测试及图形验收均直接运行于本机 macOS，没有使用沙箱、容器或模拟器。

## 环境与自动检查

- macOS 27.0（26A428），Apple Silicon arm64，Rust 1.98.1。
- `bash scripts/check-m1.sh --bundle`：正式模块 **936 项通过**，默认跳过 3 项；包括 app 603、链接绘制集成 17、core 210、往返 5、embedding 21、index 79、反链集成 1。
- `cargo test --locked -p gpui-base`：**924 项通过**（单元 923、集成 1）。
- debug 应用打包及 `scripts/test_bundle_macos.py`：**12 项通过**，涵盖包结构、签名、资源、DMG 和发布预检。
- 诊断专项默认 **12 项通过**；默认跳过的写入微基准另行运行并通过。仍未运行的两项需要已下载的固定语义模型。
- 最终补齐保存成功通知中的诊断上下文后，`cargo test --locked -p aero-app` 再次通过：app **603 项**、链接集成 **17 项**。
- 日志：`target/diagnostics-regression.log`、`target/diagnostics-final-app.log`、`target/diagnostics-gpui.log`、`target/diagnostics-tests.log`、`target/diagnostics-benchmark.log`。

## 诊断专项覆盖

| 范围 | 验证结果 |
| --- | --- |
| 字段隐私 | 正文、查询、路径、任意消息和 Debug 对象被持久化白名单过滤；旧日志只留下安全元数据和当前操作关联 |
| 操作生命周期 | 跨线程父子关系、只完成一次、取消任务收尾、文档匿名编号、最终性能汇总 |
| 详细模式 | 普通模式保留起止；开启后可见中间阶段；过期 / 关闭后的 debug 事件被过滤 |
| 容量与轮转 | 缩小阈值验证轮转文件数、过期 / 超预算清理；活动会话持有独立 lease，不被其他进程删除 |
| 拥塞与写盘故障 | 队列满立即返回并计数，警告 / 错误有单独有界容量；目录不可写及轮转失败不引发应用 panic |
| 异常与退出 | 真实子进程验证可恢复 panic、未捕获 panic、`_exit` 异常结束、原生 `exit()`；panic 有独立报告，异常结束没有正常结束标记 |
| 导出范围与覆盖保护 | 仅导出受管理文件；忽略未知字段、符号链接及末尾不完整行；原子 create-only 写入，已有文件不变 |

第一次并行回归曾出现 CLI 索引重开时的锁冲突，单独复跑通过。退出测试现使用 macOS `POSIX_SPAWN_CLOEXEC_DEFAULT` 隔离子进程，避免继承原生引擎未设置 close-on-exec 的句柄；另有专门用例验证继承关闭。调整后的完整并行回归通过。

## 真实窗口验收

使用 `aero-ui-test` 的隔离 vault、数据目录和事件通道。原生保存面板使用独立身份 `dev.aero.diagnostics-test` 的临时测试应用，以免操作用户运行中的 Aero。状态与截图位于 `/tmp/aero-ui-diagnostics-2026-10-10/`，测试应用位于 `target/diagnostics-ui/`。

1. `⌘,` →「日志与诊断」：普通模式说明、15 分钟开关与实时倒计时显示正常。
2. 在真实编辑器中输入测试正文，保存、撤销、重做、再次保存，执行全文搜索和快速打开另一篇笔记。日志有 `note_save` / `save_write`、`undo` / `redo`、`search` / `fulltext_search`、`note_open` / `note_read_parse`、`index_write`，并有排队、读写、解析、主线程装回及帧边界阶段。
3. 搜索了测试查询词，确认正文标记已实际写入测试笔记；扫描持久化 JSONL，正文 / 查询标记、测试文件名及真实文件路径均未出现。
4. 通过 macOS 原生保存面板导出 `diagnostics-ui.aerodiag.json`：JSON 可解析，2 个会话、100 条记录、34,122 字节、文件权限 `0600`，未发生截断。诊断包内容隐私检查通过。
5. 保持详细模式开启，通过原生 `⌘Q` 退出：`native_quit`、`index_close`、`index_shutdown` 均完成；`complete.json` 中 `dropped=0`、`write_errors=0`。再次启动后详细模式为关闭。
6. CLI 的 `diagnostics path` 与 `diagnostics export` 无需 vault 即可使用；导出识别已正常退出的 GUI 会话。再次导出到已有文件返回失败，文件 SHA-256 不变。
7. 最终构建补充验证保存成功通知跨事件传递上下文：一次实际保存的 `note_save` → `index_submit` → `index_write` 操作编号分别为 `7 → 9 → 10`，父子关系正确，三者匿名文档编号均为 `2`。新增测试正文标记仍未进入日志。

初次测试窗口由验收脚本强制结束，其会话被准确保留为 `incomplete`；正常关闭的窗口为 `normal_exit`，运行中会话为 `active`。

## 写入开销

在完整回归结束、release 构建开始前，单独运行：

```sh
target/debug/deps/aero-1e75e5cb450dd4e9 --exact diagnostics::tests::writer_overhead_measurement --ignored --nocapture
```

debug 构建，详细模式，500 次操作 / 1,500 条记录：生产端 P50 **14 μs**、P95 **31 μs**；包含最后刷新与退出的总时间 **287.34 ms**，未丢弃记录。这是一次可重复的记录开销测量，不包含完整用户交互，也不能证明 GUI 延迟、帧率或内存预算达标。

## Release 符号验证

最终源码运行 `scripts/bundle-macos.sh release`，使用本机 ad-hoc 签名。构建、`codesign --verify --deep --strict` 和符号归档通过。

- 应用：`target/release/bundle/Aero.app`。
- 应用 / 归档二进制 / dSYM / release CLI 运行时清单的 UUID 均为 `62C649D7-55D7-36ED-86CA-BD1CE51EF38A`。
- 签名后二进制 SHA-256：`cdb0507f4776fb48394c301217fbea4ba57c16b49f65a2428b4183363c4c900f`；归档副本重新计算一致。
- 动态库 UUID：`DB8D886D-410C-3F9F-A728-6480345F6564`，与运行时清单一致。
- 符号目录：`target/release/symbols/62C649D7-55D7-36ED-86CA-BD1CE51EF38A-cdb0507f4776/`。实际二进制与 dSYM 均存在，符号未装入应用包。
- 清单保留版本、基线 commit、工作树 dirty 标记、UUID 和文件哈希；`native_debug_symbols=false` 明确表示预编译原生库缺少独立 dSYM。
- release CLI 的 `diagnostics path` 成功，并在默认日志目录生成有正常结束标记的独立会话。
- 构建日志：`target/diagnostics-release-final.log`。

## 已知边界

- 普通模式记录业务起止和聚合耗时；详细模式增加安全阶段，15 分钟后自动恢复。队列满可丢记录，计数在日志和设置中可见。
- 每会话 JSONL 上限约 5 MiB；历史按 7 天 / 50 MiB 清理，所有活跃会话受保护，因此同时运行多个进程时总目录可能暂时超过历史预算。异常结束可能丢失尚未刷新的尾部记录。
- 记录中的帧边界是 GPUI 调度边界，不能视为 GPU 实际呈现时间。心跳只在活跃窗口记录 1 秒定时器超过 500 ms 的明显延误，超过 10 秒的间隔视作可能休眠；这不是 CPU / GPU 采样器。
- Rust panic 报告省略任意 payload 和源码绝对路径；可恢复 panic 不等于进程崩溃。SIGSEGV / SIGABRT 仍依赖 macOS `.ips`，系统报告不自动装入诊断包。
- 预编译 zvec 动态库目前没有独立 dSYM，归档保留 UUID、哈希和实际动态库，但其原生栈的符号信息仍有限。
- 此轮未替换已安装应用，未做正式证书签名、公证或发布。
