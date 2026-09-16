#!/usr/bin/env bash
# 提交公证并装订（staple）。用法：scripts/notarize-macos.sh <Aero.app|Aero.dmg>
#
# 凭据二选一（都由环境变量提供，脚本不存任何秘密）：
# - AERO_NOTARY_PROFILE：`xcrun notarytool store-credentials` 存进钥匙串的配置名；
# - AERO_NOTARY_APPLE_ID + AERO_NOTARY_TEAM_ID + AERO_NOTARY_PASSWORD（App 专用密码）。
# .app 会先 zip 再提交（notarytool 只收 zip / dmg / pkg），装订回 .app 本身。
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This script requires macOS." >&2
  exit 1
fi
if [[ "$#" -ne 1 ]]; then
  echo "Usage: $0 <Aero.app|Aero.dmg>" >&2
  exit 1
fi
target="$1"
if [[ ! -e "$target" ]]; then
  echo "No such file: $target" >&2
  exit 1
fi

auth=()
if [[ -n "${AERO_NOTARY_PROFILE:-}" ]]; then
  auth=(--keychain-profile "$AERO_NOTARY_PROFILE")
elif [[ -n "${AERO_NOTARY_APPLE_ID:-}" && -n "${AERO_NOTARY_TEAM_ID:-}" && -n "${AERO_NOTARY_PASSWORD:-}" ]]; then
  auth=(--apple-id "$AERO_NOTARY_APPLE_ID" --team-id "$AERO_NOTARY_TEAM_ID" --password "$AERO_NOTARY_PASSWORD")
else
  cat >&2 <<'EOF'
Notarization credentials missing. Set either
  AERO_NOTARY_PROFILE            (xcrun notarytool store-credentials <name>)
or
  AERO_NOTARY_APPLE_ID, AERO_NOTARY_TEAM_ID, AERO_NOTARY_PASSWORD
EOF
  exit 2
fi

# 提交前先确认是正式签名：ad-hoc 或未签名的东西公证一定失败，早点报错
# （不用 `| grep -q`：pipefail 下 grep 提前退出会让 codesign 吃到 SIGPIPE，结果不稳定）
signature="$(codesign -dv "$target" 2>&1 || true)"
if ! codesign --verify --strict "$target" 2>/dev/null || [[ "$signature" == *"Signature=adhoc"* ]]; then
  echo "$target is not signed with a Developer ID (set AERO_SIGN_IDENTITY when bundling)." >&2
  exit 3
fi

submission="$target"
tmp=""
case "$target" in
  *.app)
    tmp="$(mktemp -d -t aero-notarize)"
    trap 'rm -rf "$tmp"' EXIT
    submission="$tmp/$(basename "$target" .app).zip"
    # ditto -k 保留签名与扩展属性，是 Apple 文档推荐的打包方式
    ditto -c -k --keepParent "$target" "$submission"
    ;;
  *.dmg | *.pkg) ;;
  *) echo "Can only notarize .app, .dmg or .pkg: $target" >&2; exit 1 ;;
esac

echo "Submitting $submission for notarization…"
xcrun notarytool submit "$submission" "${auth[@]}" --wait --output-format plist >"${tmp:-/tmp}/aero-notary-result.plist" || {
  echo "notarytool submit failed" >&2
  cat "${tmp:-/tmp}/aero-notary-result.plist" >&2 || true
  exit 4
}
status="$(plutil -extract status raw -o - "${tmp:-/tmp}/aero-notary-result.plist" 2>/dev/null || echo unknown)"
id="$(plutil -extract id raw -o - "${tmp:-/tmp}/aero-notary-result.plist" 2>/dev/null || echo "")"
if [[ "$status" != "Accepted" ]]; then
  echo "Notarization status: $status (submission $id)" >&2
  if [[ -n "$id" ]]; then
    xcrun notarytool log "$id" "${auth[@]}" >&2 || true
  fi
  exit 5
fi
xcrun stapler staple "$target"
xcrun stapler validate "$target"
printf 'Notarized and stapled: %s\n' "$target"
