#!/usr/bin/env bash
# 裁剪 jieba 主词典（project_plan 10.3）：全词典 349k 词在进程里占 ≈200 MB，是索引侧最大的常驻内存项。
# 按词频取前 TOP_N 词（默认 100k，≈1.4 MB），长尾交给 HMM + search 模式的子词切分；
# 再并入 Aero 领域词（USER_DICT，zvec 的 JiebaTokenizer 只加载 jieba.dict.utf8 + hmm_model.utf8，
# 不读单独的 user.dict.utf8，所以领域词必须直接并进主词典）。hmm_model.utf8 原样拷贝。
#
# 用法：scripts/trim-jieba-dict.sh <out_dir> [top_n]
#   SRC_DIR    源词典目录（默认 $ZVEC_LIB_DIR/data/jieba_dict 或 vendor/zvec/lib/data/jieba_dict）
#   USER_DICT  领域词文件（默认 assets/jieba/user.dict.utf8，存在才合并）
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT_DIR="${1:?usage: trim-jieba-dict.sh <out_dir> [top_n]}"
TOP_N="${2:-100000}"
SRC_DIR="${SRC_DIR:-${ZVEC_LIB_DIR:-${ROOT}/vendor/zvec/lib}/data/jieba_dict}"
USER_DICT="${USER_DICT:-${ROOT}/assets/jieba/user.dict.utf8}"

SRC_DICT="${SRC_DIR}/jieba.dict.utf8"
SRC_HMM="${SRC_DIR}/hmm_model.utf8"
[[ -f "${SRC_DICT}" ]] || { echo "error: ${SRC_DICT} not found (run scripts/fetch-zvec.sh?)" >&2; exit 1; }
[[ -f "${SRC_HMM}" ]] || { echo "error: ${SRC_HMM} not found" >&2; exit 1; }

mkdir -p "${OUT_DIR}"
OUT_DICT="${OUT_DIR}/jieba.dict.utf8"
TMP="$(mktemp -d)"
trap 'rm -rf "${TMP}"' EXIT

# 领域词表：每行 "词 [词频] [词性]"，忽略空行与 # 注释。
# cppjieba 的 DictTrie 要求每行严格三列，缺词频补高值（保留且倾向整词切分），缺词性补 'n'。
USER_WORDS="${TMP}/user_words"
: > "${USER_WORDS}"
if [[ -f "${USER_DICT}" ]]; then
  awk 'NF && $1 !~ /^#/ {
    freq = ($2=="" ? "1000000" : $2)
    tag  = ($3=="" ? "n" : $3)
    print $1" "freq" "tag
  }' "${USER_DICT}" > "${USER_WORDS}"
fi

# 主词典：排除已在领域词表里的词（领域词版本优先），按词频降序取前 TOP_N。
# 分两步落盘，避免 head 提前关闭管道时 sort 收到 SIGPIPE（配合 pipefail 会误判失败）。
awk 'NR==FNR { seen[$1]=1; next } !($1 in seen)' "${USER_WORDS}" "${SRC_DICT}" \
  | sort -t' ' -k2,2nr -k1,1 > "${TMP}/sorted"
head -n "${TOP_N}" "${TMP}/sorted" > "${TMP}/base"

# 合并：领域词在前，再接裁剪后的主词典。
cat "${USER_WORDS}" "${TMP}/base" > "${OUT_DICT}"
cp "${SRC_HMM}" "${OUT_DIR}/hmm_model.utf8"

src_lines="$(wc -l < "${SRC_DICT}" | tr -d ' ')"
out_lines="$(wc -l < "${OUT_DICT}" | tr -d ' ')"
user_lines="$(wc -l < "${USER_WORDS}" | tr -d ' ')"
src_size="$(du -h "${SRC_DICT}" | cut -f1)"
out_size="$(du -h "${OUT_DICT}" | cut -f1)"
echo "trimmed jieba dict:"
echo "  source : ${src_lines} words (${src_size})  ${SRC_DICT}"
echo "  domain : ${user_lines} words  ${USER_DICT}"
echo "  output : ${out_lines} words (${out_size})  ${OUT_DICT}"
