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

script_dir="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d -t aero-dmg)"
mount="$work/mount"
mounted=0
cleanup() {
  if [[ "$mounted" -eq 1 ]]; then
    # A failed layout/conversion must not leave a writable installer mounted.
    hdiutil detach -quiet "$mount" 2>/dev/null || hdiutil detach -quiet -force "$mount" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT
staging="$work/staging"
mkdir -p "$staging/.background"
# ditto 保留扩展属性与签名；背景和 Finder 布局都是隐藏文件。
ditto "$app" "$staging/$(basename "$app")"
ln -s /Applications "$staging/Applications"
xcrun swift "$script_dir/dmg-background.swift" "$staging/.background/install.tiff"

# Configure the actual volume so Finder's background alias survives conversion
# and remounting. Finder writes the window and icon positions into .DS_Store.
hdiutil create -quiet -volname "Aero" -fs HFS+ -srcfolder "$staging" -format UDRW "$work/writable.dmg"
hdiutil attach -quiet -nobrowse -noautoopen -mountpoint "$mount" "$work/writable.dmg"
mounted=1
osascript "$script_dir/dmg-layout.applescript" "$mount" "$(basename "$app")"
if [[ ! -s "$mount/.DS_Store" ]]; then
  echo "Finder did not save the DMG layout (.DS_Store)." >&2
  exit 1
fi
sync
hdiutil detach -quiet "$mount"
mounted=0

mkdir -p "$(dirname "$out")"
# UDZO：zlib 压缩的只读镜像，所有 macOS 都能挂载
hdiutil convert -quiet "$work/writable.dmg" -format UDZO -o "$work/installer.dmg"

if [[ -n "${AERO_SIGN_IDENTITY:-}" ]]; then
  codesign --force --sign "$AERO_SIGN_IDENTITY" --timestamp "$work/installer.dmg"
  codesign --verify --strict "$work/installer.dmg"
fi
mv -f "$work/installer.dmg" "$out"
printf 'Built: %s\n' "$out"
