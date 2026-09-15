#!/usr/bin/env bash
# 把 spike 二进制装进一个最小 .app，验证 zvec dylib 走 Contents/Frameworks + rpath 的完整链路：
#   1. 去掉开发期写进二进制的绝对 rpath（vendor/zvec/lib），只留 @executable_path/../Frameworks
#   2. dylib 进 Frameworks，jieba 词典进 Resources，运行时用 --jieba-dict 指过去
#   3. 先签 dylib 再签 .app（inside-out），默认 ad-hoc；有 Developer ID 时 SIGN_ID="Developer ID Application: ..." 并加 hardened runtime
#   4. 从 .app 里启动，跑一遍小语料，确认加载的是 bundle 内的 dylib
# 用法：spikes/zvec-index/bundle.sh [输出目录]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="${1:-/tmp/aero-spike-bundle}"
APP="${OUT}/SpikeZvec.app"
BIN_SRC="${CARGO_TARGET_DIR:-${ROOT}/target}/release/spike-zvec-index"
LIB_SRC="${ROOT}/vendor/zvec/lib/libzvec_c_api.dylib"
DICT_SRC="${ROOT}/vendor/zvec/lib/data/jieba_dict"
SIGN_ID="${SIGN_ID:--}"   # "-" = ad-hoc

[[ -x "${BIN_SRC}" ]] || { echo "build first: cargo build --release -p spike-zvec-index" >&2; exit 1; }

rm -rf "${APP}"
mkdir -p "${APP}/Contents/MacOS" "${APP}/Contents/Frameworks" "${APP}/Contents/Resources/jieba_dict"
cp "${BIN_SRC}" "${APP}/Contents/MacOS/spike-zvec-index"
cp "${LIB_SRC}" "${APP}/Contents/Frameworks/"
cp "${DICT_SRC}"/*.utf8 "${APP}/Contents/Resources/jieba_dict/"
cat > "${APP}/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>dev.aero.spike.zvec</string>
  <key>CFBundleName</key><string>SpikeZvec</string>
  <key>CFBundleExecutable</key><string>spike-zvec-index</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
</dict></plist>
PLIST

BIN="${APP}/Contents/MacOS/spike-zvec-index"
# 1. rpath：删掉所有绝对路径，只留 @executable_path/../Frameworks
otool -l "${BIN}" | awk '/LC_RPATH/{f=1} f&&/path /{print $2; f=0}' | while read -r rp; do
  if [[ "${rp}" == /* ]]; then install_name_tool -delete_rpath "${rp}" "${BIN}"; fi
done
echo "rpaths now:"; otool -l "${BIN}" | awk '/LC_RPATH/{f=1} f&&/path /{print "  " $2; f=0}'

# 2. 签名（inside-out）。Developer ID 时加 --options runtime（hardened runtime），公证要求
SIGN_FLAGS=(--force --timestamp=none)
if [[ "${SIGN_ID}" != "-" ]]; then SIGN_FLAGS=(--force --timestamp --options runtime); fi
codesign "${SIGN_FLAGS[@]}" --sign "${SIGN_ID}" "${APP}/Contents/Frameworks/libzvec_c_api.dylib"
codesign "${SIGN_FLAGS[@]}" --sign "${SIGN_ID}" "${APP}"
codesign --verify --deep --strict --verbose=2 "${APP}"
echo "signature: $(codesign -dv "${APP}" 2>&1 | grep -E '^Signature|^Authority' | head -2 | tr '\n' ' ')"

# 3. 从 bundle 里运行，并确认 dyld 装载的是 bundle 内的 dylib
echo "--- run from bundle"
DYLD_PRINT_LIBRARIES=1 "${BIN}" --notes 200 --vec-docs 300 --jieba-dict "${APP}/Contents/Resources/jieba_dict" \
  --dir "${OUT}/data" 2>&1 | grep -E 'libzvec_c_api|jieba dict dir|save→searchable|self@1|final RSS' | sed 's/^dyld\[[0-9]*\]: //'

if [[ "${SIGN_ID}" != "-" ]]; then
  echo "--- notarize: ditto -c -k --keepParent \"${APP}\" app.zip && xcrun notarytool submit app.zip --keychain-profile <profile> --wait && xcrun stapler staple \"${APP}\""
fi
