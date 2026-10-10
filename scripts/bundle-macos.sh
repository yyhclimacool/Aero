#!/usr/bin/env bash
# 构建带 Finder / Dock 图标的 Aero.app。需要 macOS、Rust、Python 3 和 Xcode 命令行工具。
# 用法：scripts/bundle-macos.sh [debug|release]（默认 debug）
#
# 签名：
# - 默认 ad-hoc 签名（本机运行）；
# - 设 AERO_SIGN_IDENTITY="Developer ID Application: Name (TEAMID)" 则用该证书正式签名，
#   带 hardened runtime、安全时间戳与 resources/macos/entitlements.plist（公证的前提）。
# 公证与 .dmg 见 scripts/release-macos.sh。
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

aero_lib_dir="${ZVEC_LIB_DIR:-$aero_root/vendor/zvec/lib}"
for resource in libzvec_c_api.dylib data/jieba_dict/jieba.dict.utf8 data/jieba_dict/hmm_model.utf8; do
  if [[ ! -f "$aero_lib_dir/$resource" ]]; then
    printf 'Missing zvec resource: %s\nRun scripts/fetch-zvec.sh or set ZVEC_LIB_DIR.\n' "$aero_lib_dir/$resource" >&2
    exit 1
  fi
done
aero_lib_dir="$(cd "$aero_lib_dir" && pwd)"
# 构建和打包必须使用同一份动态库。
export ZVEC_LIB_DIR="$aero_lib_dir"

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
mkdir -p "$aero_app/Contents/MacOS" "$aero_app/Contents/Resources" "$aero_app/Contents/Frameworks"
cp "$aero_binary" "$aero_app/Contents/MacOS/aero"
cp "$aero_lib_dir/libzvec_c_api.dylib" "$aero_app/Contents/Frameworks/libzvec_c_api.dylib"
LC_ALL=C SRC_DIR="$aero_lib_dir/data/jieba_dict" \
  bash "$aero_root/scripts/trim-jieba-dict.sh" "$aero_app/Contents/Resources/jieba_dict"
cp assets/branding/aero-icon-v1.icns "$aero_app/Contents/Resources/Aero.icns"
cp crates/aero-app/resources/macos/Info.plist "$aero_app/Contents/Info.plist"
plutil -replace CFBundleVersion -string "$aero_version" "$aero_app/Contents/Info.plist"
plutil -replace CFBundleShortVersionString -string "$aero_version" "$aero_app/Contents/Info.plist"
plutil -lint "$aero_app/Contents/Info.plist"

# 只修改包内副本，避免开发机的绝对 rpath 掩盖漏装的动态库。
python3 - "$aero_app/Contents/MacOS/aero" <<'PY'
import re
import subprocess
import sys

binary = sys.argv[1]
commands = subprocess.check_output(["otool", "-l", binary], text=True)
paths = re.findall(r"^\s*path (.*?) \(offset \d+\)$", commands, re.MULTILINE)
if "@executable_path/../Frameworks" not in paths:
    sys.exit("Aero executable is missing the bundle Frameworks rpath.")
for path in paths:
    if path.startswith("/"):
        subprocess.run(["install_name_tool", "-delete_rpath", path, binary], check=True)
PY

# 嵌套代码先签名，外层签名才能封存完整资源。
if [[ -n "${AERO_SIGN_IDENTITY:-}" ]]; then
  # 正式签名：hardened runtime + 安全时间戳；动态库与可执行文件同一证书，库校验才能通过
  sign_flags=(--force --sign "$AERO_SIGN_IDENTITY" --timestamp --options runtime)
  codesign "${sign_flags[@]}" "$aero_app/Contents/Frameworks/libzvec_c_api.dylib"
  codesign "${sign_flags[@]}" --entitlements crates/aero-app/resources/macos/entitlements.plist "$aero_app"
else
  codesign --force --sign - --timestamp=none "$aero_app/Contents/Frameworks/libzvec_c_api.dylib"
  codesign --force --sign - --timestamp=none "$aero_app"
fi
codesign --verify --deep --strict "$aero_app"
if [[ "${1:-debug}" == release ]]; then
  # Keep symbols and exact signed images on the developer machine, outside the .app / DMG.
  python3 scripts/archive-symbols.py --app "$aero_app" --binary "$aero_binary" \
    --output "$(dirname "$aero_binary")/symbols" --version "$aero_version"
fi
printf 'Built: %s\n' "$aero_app"
