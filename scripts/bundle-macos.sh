#!/usr/bin/env bash
# 构建带 Finder / Dock 图标的本地 Aero.app。需要 macOS、Rust、Python 3 和 Xcode 命令行工具。
# 用法：scripts/bundle-macos.sh [debug|release]（默认 debug）
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This script requires macOS." >&2
  exit 1
fi

aero_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$aero_root"
aero_build_flags=(--locked -p aero-app --bin aero --message-format=json-render-diagnostics)
case "${1:-debug}" in
  debug) ;;
  release) aero_build_flags+=(--release) ;;
  *) echo "Usage: $0 [debug|release]" >&2; exit 1 ;;
esac
if [[ "$#" -gt 1 ]]; then
  echo "Usage: $0 [debug|release]" >&2
  exit 1
fi

# 从 Cargo 的实际构建结果读取路径，兼容自定义 target 目录与构建 target。
aero_binary="$(cargo build "${aero_build_flags[@]}" | python3 -c '
import json, sys
executable = None
for line in sys.stdin:
    message = json.loads(line)
    if (message.get("reason") == "compiler-artifact"
            and message.get("target", {}).get("name") == "aero"
            and message.get("executable")):
        executable = message["executable"]
if executable is None:
    sys.exit("Cargo did not produce the Aero executable.")
print(executable)
')"
aero_version="$(cargo metadata --locked --offline --no-deps --format-version 1 | python3 -c '
import json, sys
metadata = json.load(sys.stdin)
print(next(p["version"] for p in metadata["packages"] if p["name"] == "aero-app"))
')"

aero_app="$(dirname "$aero_binary")/bundle/Aero.app"
mkdir -p "$aero_app/Contents/MacOS" "$aero_app/Contents/Resources"
cp "$aero_binary" "$aero_app/Contents/MacOS/aero"
cp assets/branding/aero-icon-v1.icns "$aero_app/Contents/Resources/Aero.icns"
cp crates/aero-app/resources/macos/Info.plist "$aero_app/Contents/Info.plist"
plutil -replace CFBundleVersion -string "$aero_version" "$aero_app/Contents/Info.plist"
plutil -replace CFBundleShortVersionString -string "$aero_version" "$aero_app/Contents/Info.plist"
plutil -lint "$aero_app/Contents/Info.plist"

# 本地 ad-hoc 签名；当前 aero-app 仅链接系统动态库，图片与字体均已内嵌。
codesign --force --sign - --timestamp=none "$aero_app"
codesign --verify --deep --strict "$aero_app"
printf 'Built: %s\n' "$aero_app"
