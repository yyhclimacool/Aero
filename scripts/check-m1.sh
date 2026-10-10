#!/usr/bin/env bash
# M1 可自动验收部分：从当前源码构建，失败立即退出，日志保存到 target/m1-qa。
# 用法：bash scripts/check-m1.sh [--bundle]
# --bundle 额外构建 ad-hoc 应用包并验收 DMG；不提交公证、不使用正式证书。
set -euo pipefail
cd "$(dirname "$0")/.."
case "${1:-}" in
  ""|--bundle) ;;
  *) echo "Usage: $0 [--bundle]" >&2; exit 2 ;;
esac
[[ $# -le 1 ]] || exit 2
mkdir -p target/m1-qa
exec > >(tee target/m1-qa/check.log) 2>&1
date -u '+%Y-%m-%dT%H:%M:%SZ'
git rev-parse HEAD
git diff --stat
rustc --version
sw_vers
cargo fmt -p aero-core -p aero-index -p aero-embedding -p aero-app -- --check
cargo test --locked -p aero-core -p aero-index -p aero-embedding -p aero-app
if [[ "${1:-}" == --bundle ]]; then
  # 预检测试明确验证 ad-hoc 包会被公证脚本拒绝，故不沿用用户发布凭据。
  unset AERO_SIGN_IDENTITY AERO_NOTARY_PROFILE AERO_NOTARY_APPLE_ID AERO_NOTARY_TEAM_ID AERO_NOTARY_PASSWORD
  bash scripts/bundle-macos.sh debug
  python3 scripts/test_bundle_macos.py
fi
echo 'M1 automated checks passed. Manual IME, GUI performance and formal release gates: docs/qa/m1.md'
