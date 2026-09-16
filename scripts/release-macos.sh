#!/usr/bin/env bash
# macOS 发布链路（project_plan 第 15 节 M1b）：release 构建 → 签名 → [公证并装订 .app] → .dmg → [公证并装订 .dmg]
# 用法：scripts/release-macos.sh [--notarize] [输出目录]（默认 dist/）
#
# - 没设 AERO_SIGN_IDENTITY：ad-hoc 签名的 .app 与未签名的 .dmg，只能在本机 / 关掉 Gatekeeper 的机器上用；
# - 设了 AERO_SIGN_IDENTITY：Developer ID 正式签名（hardened runtime）；
# - 再加 --notarize（需要 scripts/notarize-macos.sh 说明的凭据）：公证 .app 与 .dmg 并装订，
#   这才是可以公开分发的产物。
# 自动更新不在 M1 范围（砍单清单第一项），发布 = 上传 .dmg。
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This script requires macOS." >&2
  exit 1
fi

notarize=0
out_dir=""
for arg in "$@"; do
  case "$arg" in
    --notarize) notarize=1 ;;
    -*) echo "Usage: $0 [--notarize] [output-dir]" >&2; exit 1 ;;
    *)
      if [[ -n "$out_dir" ]]; then
        echo "Usage: $0 [--notarize] [output-dir]" >&2
        exit 1
      fi
      out_dir="$arg"
      ;;
  esac
done

aero_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$aero_root"
out_dir="${out_dir:-$aero_root/dist}"

if [[ "$notarize" -eq 1 ]]; then
  if [[ -z "${AERO_SIGN_IDENTITY:-}" ]]; then
    echo "--notarize requires AERO_SIGN_IDENTITY (a Developer ID Application certificate)." >&2
    exit 2
  fi
  if [[ -z "${AERO_NOTARY_PROFILE:-}" && ( -z "${AERO_NOTARY_APPLE_ID:-}" || -z "${AERO_NOTARY_TEAM_ID:-}" || -z "${AERO_NOTARY_PASSWORD:-}" ) ]]; then
    echo "--notarize requires AERO_NOTARY_PROFILE or AERO_NOTARY_APPLE_ID + AERO_NOTARY_TEAM_ID + AERO_NOTARY_PASSWORD." >&2
    exit 2
  fi
fi

app="$(bash scripts/bundle-macos.sh release | sed -n 's/^Built: //p' | tail -n 1)"
if [[ -z "$app" || ! -d "$app" ]]; then
  echo "bundle-macos.sh did not report the app path." >&2
  exit 1
fi
version="$(plutil -extract CFBundleShortVersionString raw -o - "$app/Contents/Info.plist")"

if [[ "$notarize" -eq 1 ]]; then
  bash scripts/notarize-macos.sh "$app"
fi

mkdir -p "$out_dir"
dmg="$out_dir/Aero-$version.dmg"
bash scripts/dmg-macos.sh "$app" "$dmg"
if [[ "$notarize" -eq 1 ]]; then
  bash scripts/notarize-macos.sh "$dmg"
fi

if [[ -z "${AERO_SIGN_IDENTITY:-}" ]]; then
  echo "Note: ad-hoc signed build; other Macs will need to right-click → Open (or set AERO_SIGN_IDENTITY)." >&2
elif [[ "$notarize" -eq 0 ]]; then
  echo "Note: signed but not notarized; run with --notarize before distributing." >&2
fi
printf 'Release: %s\n' "$dmg"
