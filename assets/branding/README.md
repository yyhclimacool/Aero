# Aero icon · v1

## 设计

将折叠书页与字母 **A** 融合：书页对应本地 Markdown 文档，轻盈的折面呼应 Aero 的名称和速度。石墨色圆角底板、纸白色主体与克制的立体层次，与项目专注内容的界面原则相呼应。

## 文件

- `aero-icon-v1.png`：生成的原始图标，保留透明外边距。
- `aero-icon-v1.icns`：用于 macOS 应用打包的图标，包含 16、32、64、128、256、512、1024 像素表示。
- `prompt.txt`：完整生成提示词。

使用内置 `image_gen` 工具生成。ICNS 由原始 PNG 使用 macOS `sips` 和 `iconutil` 缩放、封装，保持原始构图。

## 项目接入

- `crates/aero-app/src/branding.rs` 内嵌 PNG 和 ICNS，欢迎页与 macOS Dock 直接使用，运行时无需访问源码目录。
- `scripts/bundle-macos.sh` 将 ICNS 复制为 `Aero.app/Contents/Resources/Aero.icns`，由 `Info.plist` 的 `CFBundleIconFile` 引用，供 Finder 和 Dock 使用。
- 项目 README 展示 PNG 原图。
