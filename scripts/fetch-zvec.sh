#!/usr/bin/env bash
# 下载 zvec 预编译 C 库到 vendor/zvec/lib，供 ZVEC_LIB_DIR 离线构建使用（见 .cargo/config.toml）。
# 校验和登记在 scripts/zvec-prebuilt.sha256；新增平台或升级版本时用 `--print-sha` 生成新行。
set -euo pipefail

ZVEC_RUST_VERSION="${ZVEC_RUST_VERSION:-0.7.1}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="${TARGET:-$(rustc -vV | sed -n 's/^host: //p')}"
ASSET="zvec-prebuilt-${TARGET}.tar.gz"
URL="https://github.com/zvec-ai/zvec-rust/releases/download/v${ZVEC_RUST_VERSION}/${ASSET}"
DEST="${ROOT}/vendor/zvec/lib"
SUMS="${ROOT}/scripts/zvec-prebuilt.sha256"

mkdir -p "${DEST}"
TMP="$(mktemp -d)"
trap 'rm -rf "${TMP}"' EXIT

echo "fetch ${URL}"
curl -fsSL -o "${TMP}/${ASSET}" "${URL}"

sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
ACTUAL="$(sha "${TMP}/${ASSET}")"

if [[ "${1:-}" == "--print-sha" ]]; then
  echo "${ACTUAL}  v${ZVEC_RUST_VERSION}/${ASSET}"
  exit 0
fi

EXPECTED="$(grep -F " v${ZVEC_RUST_VERSION}/${ASSET}" "${SUMS}" | cut -d' ' -f1 || true)"
if [[ -z "${EXPECTED}" ]]; then
  echo "error: no checksum for v${ZVEC_RUST_VERSION}/${ASSET} in ${SUMS}; run with --print-sha and add it" >&2
  exit 1
fi
if [[ "${ACTUAL}" != "${EXPECTED}" ]]; then
  echo "error: checksum mismatch for ${ASSET}: expected ${EXPECTED}, got ${ACTUAL}" >&2
  exit 1
fi

rm -rf "${DEST:?}"/*
tar -xzf "${TMP}/${ASSET}" -C "${DEST}"
echo "ok: $(ls "${DEST}") -> ${DEST}"
