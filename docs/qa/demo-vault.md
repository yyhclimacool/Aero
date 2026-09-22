# 示例库验收

更新日期：2026-09-22。对应 [UI 设计 11 节](../ui_design.md)（首次运行与 vault 管理）与 [方案 5.3](../project_plan.md)（应用数据目录）。

## 范围

- 无参数启动恢复最近打开且目录还在的库，失效条目跳过且不弹错误；首次运行或没有可恢复的库时停在库选择页，此时磁盘上不创建任何东西（连应用数据目录都不建）。命令行给了路径则直接打开该库，既不碰示例库也不恢复最近列表。
- 库选择页的「查看示例库」按需交付并打开示例库，停在 `00-欢迎.md`。示例库进入最近列表；从最近列表点进去、或启动时自动恢复到它，都走同一条路径（按实际目录比较，规范化后的路径也能认出来），因此落点始终是欢迎页而不是最近改过的那篇。
- 六篇中文示例的源码在 `assets/demo-vault/`，用 `include_str!` 在编译期嵌入可执行文件：运行时不读安装包内的文件，`.app` 被移动或挂在只读镜像上也能交付；每个 `.dmg` 带的就是那次构建的示例。
- 首次交付：先在应用数据目录下的 `.demo-vault-<pid>-<nonce>/` 里写完整副本（含交付记录），再 `rename` 整体发布。准备中失败会删掉临时目录、在选择页报错，不会留下半个库。
- 再次进入（含应用升级后示例内容变化）按 `.aero/demo.toml` 里每篇最近一次交付的内容指纹增量更新：
  - 指纹吻合（用户没动过）且内容与这一版不同 → 换成这一版；
  - 指纹不吻合（用户改过）→ 原样保留，基线也不动，以后发版同样不覆盖；
  - 交付过但文件不在（用户删了或改名了）→ 不恢复；
  - 交付记录里没有且文件不在（这一版新增的示例）→ 补写；
  - 目录里没有 `.aero/demo.toml` → 不是我们交付的示例库（用户自己建的同名文件夹），一个字都不写。
- 交付记录放在 `.aero/` 下，不出现在文件树、笔记枚举与索引里。读不到或读不懂都当作"没有记录"，宁可不更新也不冒覆盖用户笔记的风险。
- 示例库是笔记而不是缓存：与 `index/`、`cache/` 同在应用数据目录，但删除它会丢用户内容。只重装应用保留，清空应用数据才重新初始化。
- 示例内容覆盖 M1 / M2 已实现的能力：标题与列表、任务、表格、callout（含折叠）、代码块、行内格式与高亮、`[[双链]]`、显示别名 `|`、front matter `aliases` / `tags`、`#章节` 与 `#^块` 引用、反链与「此块的引用」、普通 Markdown 链接与引用式定义（供改名练习观察路径改写）。故意留一个未解析链接 `[[下一篇灵感笔记]]`。

## 自动化

本机结果：格式检查通过；三个正式 crate 共 **445 项通过**（app 227、链接绘制集成 17、core 145、往返 5、index 50、反链计数集成 1），其中示例库与启动相关回归 14 项，另有 1 项打包断言。

```sh
cargo fmt -p aero-core -p aero-index -p aero-app -- --check
cargo test --locked -p aero-app demo_vault
cargo test --locked -p aero-core -p aero-index -p aero-app
bash scripts/bundle-macos.sh debug && python3 scripts/test_bundle_macos.py BundleTests
```

`demo_vault.rs`：

- `bundled_notes_match_the_repository_assets`：内嵌表与 `assets/demo-vault/` 下的 `.md` 逐个对应且内容逐字节一致——漏登记一篇新示例在这里就失败，不会等到用户装完才发现少一篇。
- `demo_vault_initializes_six_connected_notes`：六篇齐全；用 resolver 走一遍全部链接，只允许 `下一篇灵感笔记` 未解析，`#章节` 必须命中真实标题、`#^id` 必须能定位到块；交付记录覆盖全部六篇；应用数据目录里没有临时目录残留。
- `demo_vault_reopen_preserves_edits_deletions_and_renames`：改写欢迎页、删一篇、改名一篇后再次进入，内容与结构原样保留，vault id 不变；`state.toml` 里的其他 vault 不影响结果。
- `demo_vault_update_delivers_new_content_without_touching_user_notes`：把库改成"上一版"的样子后再次进入——没动过的示例换成当前版本，用户改过的保留（再进一次也不会被覆盖），交付记录里没有的那篇作为"新增示例"补写。
- `demo_vault_never_populates_a_directory_it_did_not_deliver`：没有交付记录的同名目录（哪怕只有用户自己的一个文件）不写入任何示例。
- `demo_vault_does_not_restore_a_deleted_welcome_page`：欢迎页删掉后不重新生成。
- `demo_vault_reports_file_conflict_without_overwriting_it` / `demo_vault_rejects_a_symlink_without_writing_through_it`：同名普通文件或符号链接占位时报错，不覆盖该文件、不穿过链接往目标目录写。
- `demo_vault_notes_render_in_the_editor`：每篇示例都在 GPUI 编辑器里真实加载与绘制，标题与索引指纹一致。

`workspace.rs`：

- `startup_stays_on_the_library_chooser_until_the_demo_vault_is_picked`：没有最近打开的库时启动不打开任何库、磁盘上也没有 `demo-vault/`；在真实绘制出的选择页上点「查看示例库」才交付、打开并停在欢迎页，随后示例库出现在最近列表且被识别为示例库入口。
- `startup_restores_the_most_recent_vault_and_skips_stale_entries`：最近列表里第一个条目已被删除时跳过它、恢复下一个仍然存在的库并打开其中的笔记，不显示错误，也不创建示例库。
- `startup_restoring_the_demo_vault_goes_through_the_demo_entry`：最近打开的是示例库时走示例库入口——落在欢迎页而不是刚被改过的那篇，用户的修改保持原样。
- `startup_with_an_explicit_path_skips_the_demo_vault`：带路径启动打开该库，忽略最近列表，应用数据目录里不出现 `demo-vault/`。
- `picking_an_unusable_demo_vault_reports_it_on_the_chooser`：占位文件挡路时点按钮留在选择页并显示「无法准备示例库…」，占位文件保持原样。

`scripts/test_bundle_macos.py`：

- `BundleTests.test_embedded_demo_vault_matches_the_repository`：`Aero.app` 的可执行文件里逐字节包含仓库当前的六篇示例，即"每次打包都带最新示例"这条要求的实际检查。

为了让这些测试可复现，`Workspace` 持有应用数据目录（`Workspace::new(initial_vault, data_dir, …)`，`PersistedState::load/save` 同样按目录取），测试用临时目录，不写入真实的 `~/Library/Application Support/Aero`。

## 尚未覆盖

- 原生窗口人工视觉验收未做：选择页三个按钮的排布、欢迎页排版、表格与 callout 的实际观感均未在真实窗口中确认。
- 未验证已安装 / 已签名 `.app` 的首次启动，也未验证 macOS 沙箱或只读挂载下的行为；未替换已安装应用。
- 交付记录是这一版才有的：更早版本建立的 `demo-vault/` 没有 `.aero/demo.toml`，因此不会被更新（需要手动删除目录重新交付）。
- 示例内容只有简体中文；`⌘,` 设置里没有「重置示例库」入口，恢复出厂示例需要手动删除 `demo-vault/`。
- 示例库不会删除"这一版已经移除"的旧示例笔记，只做新增与替换。
