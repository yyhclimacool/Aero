# aero-ui 参考

## 命令

坐标都是窗口里的点。图名可写短名（在 `/tmp/aero-ui/shots/` 下，省略 `.png`）或路径。
产出图片的命令打印的是一次性的副本路径（`shots/view/<名字>~<序号>.png`），给 Read 用；短名仍指向原图。

| 命令 | 作用 |
| --- | --- |
| `start [库目录] [--show] [--no-build] [--keep-data]` | 构建并启动，等窗口出现、通道回应后打印 pid、窗口号与位置 |
| `stop` / `status` | 停止 / 查看状态 |
| `vault [目录]` | 把样例库复制到目录（默认 `/tmp/aero-ui/vault`），覆盖旧的 |
| `click x y [shift\|cmd\|alt\|ctrl]` | 移过去、按下、松开 |
| `dclick x y` / `tclick x y` | 双击 / 三击 |
| `drag x1 y1 x2 y2 [步数]` | 按住左键拖动，默认 12 步 |
| `move x y` | 移动指针（按着键时就是拖动） |
| `scroll x y dy [dx]` | 滚轮，像素；负数向下翻 |
| `key 键...` | 依次按键，每个之间停 30ms |
| `type 文字` | 逐字输入，可含中文和空格 |
| `do 行...` | 原样发送通道命令，用来组合按下、移动、松开 |
| `shot 名字 [x y w h]` | 截窗口，可裁剪 |
| `grid 图 [步长]` | 叠坐标网格，另存为 `<图>.grid.png` |
| `crop 图 新名字 x y w h` | 从已有截图裁一块 |
| `diff 图a 图b [--ignore x y w h]...` | 列出差异区域，差异图写到 `<a>-vs-<b>.png` |
| `shift 图a 图b x y w h [--range pt]` | 区域内整体平移了多少（默认搜索 ±6pt） |
| `stack 新名字 图...` | 上下拼接，每张上方标文件名 |
| `color 图 x y` | 取色 |
| `probe 名字 x y [修饰键]` | 悬停截图 → 点击 → 截图 → Esc 两次 → 截图，报告两次比对和 focus 日志 |
| `log [正则]` | 应用日志最后 40 行，或按正则过滤 |
| `window` | 窗口号 x y 宽 高（屏幕点） |

`probe` 生成 `<名字>.0.png`（点击前）、`<名字>.1.png`（点击后）、`<名字>.2.png`（退出后）和 `<名字>.diff.png`。

`diff`、`shift` 按像素对齐比较，两张图要么都是整窗，要么裁的是同一块区域。

## 环境变量

| 变量 | 默认 | 作用 |
| --- | --- | --- |
| `AERO_UI_STATE` | `/tmp/aero-ui` | 状态目录 |
| `AERO_UI_SETTLE_MS` | `150` | 每批输入后额外等待 |
| `AERO_BIN` | `cargo metadata` 给出的 `target/debug/aero` | 指定可执行文件 |
| `AERO_LOG` | `debug` | 应用日志级别 |
| `WAIT` | `500` | `probe` 点击后等待的毫秒数 |
| `KEEP` | 空 | `probe` 点完不退出编辑态 |

## 通道协议

应用读取 `AERO_UI_CHANNEL` 指向的文件（每 16ms 一次，读完即删），逐行执行，一批完成后在同目录 `<文件名>.ack` 写递增序号。脚本先写 `cmd.tmp` 再改名，避免读到半个文件。

```
move x y
down x y [shift|cmd|alt|ctrl ...]
up x y
dclick x y
tclick x y
scroll x y dy [dx]
key <GPUI 按键>
type <文字>
wait <ms>
theme light|dark
```

`theme` 只切换当前测试应用的主题，便于双主题验收，不修改 macOS 外观设置。

例：从一段拖到另一段中途停一下再松开

```sh
$S do "move 400 262" "down 400 262" "wait 60" "move 500 300" "wait 200" "move 600 340" "up 600 340"
```

应用侧的开关：

| 变量 | 作用 |
| --- | --- |
| `AERO_UI_CHANNEL=<文件>` | 启用通道（只在 debug 构建里有效） |
| `AERO_UI_PARK=0` | 不停放：窗口居中并激活 |
| `AERO_DATA_DIR=<目录>` | 应用数据目录（debug 构建），代替 `~/Library/Application Support/Aero` |

## 图片工具

`scripts/img.py` 可以单独用（`target/ui-test/venv/bin/python`）。截图 PNG 的文本块记着缩放（`aero-scale`）和裁剪原点（`aero-origin`），所以裁过的图也按窗口坐标换算。差异判定：RGB 三通道差的和大于 40；间隔不超过 3pt 的差异像素并成一块。

## 排错

| 现象 | 原因与处理 |
| --- | --- |
| `截图失败` | 没在沙箱外运行，或终端没有屏幕录制权限 |
| `应用没有回应` / `找不到 Aero 的窗口` | 应用卡住或 panic 了，看 `log` |
| 点了没反应 | 坐标是从缩小的图上估的；从网格刻度重新读 |
| 截图和操作对不上 | 窗口被完全挡住或移出屏幕，macOS 不再出帧；用默认的停放位置 |
| 看不到光标、选区 | 停放的窗口不活跃，改用 `start --show` |
