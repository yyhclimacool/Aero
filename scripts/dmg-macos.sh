#!/usr/bin/env bash
# 把 Aero.app 打成可分发的 .dmg（卷名 Aero，内含 Aero.app 与指向 /Applications 的快捷方式）。
# 用法：scripts/dmg-macos.sh <Aero.app> <输出.dmg>
# 设 AERO_SIGN_IDENTITY 时对 .dmg 也签名（公证 .dmg 的前提）。
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This script requires macOS." >&2
  exit 1
fi
if [[ "$#" -ne 2 ]]; then
  echo "Usage: $0 <Aero.app> <output.dmg>" >&2
  exit 1
fi

app="$1"
out="$2"
if [[ ! -d "$app/Contents/MacOS" ]]; then
  echo "Not an app bundle: $app" >&2
  exit 1
fi
case "$out" in
  *.dmg) ;;
  *) echo "Output must end with .dmg: $out" >&2; exit 1 ;;
esac

staging="$(mktemp -d -t aero-dmg)"
trap 'rm -rf "$staging"' EXIT
# ditto 保留扩展属性与签名；只放这两样，Finder 里拖过去就装好
ditto "$app" "$staging/$(basename "$app")"
ln -s /Applications "$staging/Applications"

mkdir -p "$(dirname "$out")"
rm -f "$out"
# UDZO：zlib 压缩的只读镜像，所有 macOS 都能挂载
hdiutil create -quiet -volname "Aero" -srcfolder "$staging" -ov -format UDZO "$out"

if [[ -n "${AERO_SIGN_IDENTITY:-}" ]]; then
  codesign --force --sign "$AERO_SIGN_IDENTITY" --timestamp "$out"
  codesign --verify --strict "$out"
fi
printf 'Built: %s\n' "$out"
