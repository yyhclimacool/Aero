#!/usr/bin/env python3
"""把 Cursor 会话记录（agent-transcripts/<id>/<id>.jsonl）导出为可读的 Markdown。

用法：
    scripts/export_transcript.py <transcript.jsonl> docs/transcripts/<日期>-<主题>.md

保留用户提问与助手回复全文；工具调用只列摘要（读了什么、改了哪个文件、跑了什么命令）。
系统注入的上下文（上下文压缩后的说明、续写提示）与重试造成的重复提问会被去掉。
"""
import json, re, sys
from pathlib import Path

if len(sys.argv) != 3:
    sys.exit(__doc__)
SRC = Path(sys.argv[1])
OUT = Path(sys.argv[2]).resolve()
REPO = "/Users/aero/code/Aero/"
HARNESS_PREFIXES = ("Briefly inform the user about the task result",)


def rel(p: str) -> str:
    return p[len(REPO):] if p.startswith(REPO) else p


def summarize_tool(c: dict) -> str:
    name = c.get("name", "?")
    inp = c.get("input", {}) or {}
    if name in ("Read", "Write", "StrReplace", "Delete"):
        return f"`{name}` `{rel(inp.get('path', ''))}`"
    if name == "Shell":
        d = inp.get("description") or inp.get("command", "")
        d = d.replace("\n", " ")
        return f"`Shell` {d[:120]}"
    if name in ("Grep", "Glob"):
        pat = inp.get("pattern") or inp.get("glob_pattern", "")
        return f"`{name}` `{pat[:80]}`"
    if name == "CallDynamicTool":
        tool = f"{inp.get('namespace')}.{inp.get('toolName')}"
        args = inp.get("arguments", {}) or {}
        q = args.get("search_term") or args.get("url") or args.get("description") or ""
        return f"`{tool}` {str(q)[:120]}"
    if name == "GetDynamicTools":
        return f"`GetDynamicTools` {inp.get('namespace', '')}.{inp.get('toolName', '')}"
    if name == "Task":
        return f"`Task` {str(inp.get('description', ''))[:120]}"
    return f"`{name}`"


def demote_headings(text: str) -> str:
    out, in_fence = [], False
    for line in text.split("\n"):
        if line.lstrip().startswith("```") or line.lstrip().startswith("~~~"):
            in_fence = not in_fence
        if not in_fence and re.match(r"^#{1,4} ", line):
            line = "##" + line
        out.append(line)
    return "\n".join(out)


def parse_user(text: str):
    ts = re.search(r"<timestamp>(.*?)</timestamp>", text, re.S)
    q = re.search(r"<user_query>\n?(.*?)\n?</user_query>", text, re.S)
    uploads = re.findall(r"^- (/\S+)$", re.search(r"<uploaded_documents>(.*?)</uploaded_documents>", text, re.S).group(1), re.M) \
        if "<uploaded_documents>" in text else []
    return (ts.group(1).strip() if ts else ""), (q.group(1).strip() if q else text.strip()), uploads


turns = []  # {ts, query, uploads, parts: [("text", str) | ("tools", [str])]}
with SRC.open(encoding="utf-8") as f:
    for line in f:
        ev = json.loads(line)
        if "message" not in ev:
            continue
        role = ev["role"]
        content = ev["message"].get("content", [])
        if role == "user":
            raw = "".join(c.get("text", "") for c in content if c.get("type") == "text")
            if "<user_query>" not in raw:
                continue  # 上下文压缩后注入的系统上下文，不是用户发言
            ts, query, uploads = parse_user(raw)
            if query.startswith(HARNESS_PREFIXES):
                continue  # 系统注入的续写提示，不是用户发言
            if turns and turns[-1]["query"] == query and not any(k == "text" for k, _ in turns[-1]["parts"]):
                turns[-1]["parts"].clear()  # 重试 / 压缩前的空轮次：合并到同一轮
                turns[-1]["ts"] = ts
                continue
            turns.append({"ts": ts, "query": query, "uploads": uploads, "parts": []})
            continue
        if not turns:
            continue
        parts = turns[-1]["parts"]
        for c in content:
            if c.get("type") == "text" and c["text"].strip():
                parts.append(("text", c["text"].strip()))
            elif c.get("type") == "tool_use":
                if parts and parts[-1][0] == "tools":
                    parts[-1][1].append(summarize_tool(c))
                else:
                    parts.append(("tools", [summarize_tool(c)]))


def short_ts(ts: str) -> str:
    # "Monday, Sep 14, 2026, 9:48 PM (UTC+8)" -> "09-14 21:48"
    m = re.search(r"(\w{3}) (\d+), (\d{4}), (\d+):(\d+) (AM|PM)", ts)
    if not m:
        return ts
    mon = {"Jan": 1, "Feb": 2, "Mar": 3, "Apr": 4, "May": 5, "Jun": 6, "Jul": 7, "Aug": 8, "Sep": 9, "Oct": 10, "Nov": 11, "Dec": 12}[m.group(1)]
    h = int(m.group(4)) % 12 + (12 if m.group(6) == "PM" else 0)
    return f"{mon:02d}-{int(m.group(2)):02d} {h:02d}:{m.group(5)}"


if turns and not turns[-1]["parts"]:
    # 导出本身发生在最后一轮的回复之前
    turns[-1]["parts"].append(("text", f"（本轮的产出就是这份文档：`{OUT.relative_to(REPO) if str(OUT).startswith(REPO) else OUT}`，由 `scripts/export_transcript.py` 从 Cursor 会话记录生成。）"))

first_ts, last_ts = short_ts(turns[0]["ts"]), short_ts(turns[-1]["ts"])
n_tools = sum(len(p[1]) for t in turns for p in t["parts"] if p[0] == "tools")

md = []
md.append("# Aero 开发对话记录 · 从方案评审到 M0 完成\n")
md.append(f"> 导出自 Cursor 会话 `{SRC.stem[:8]}`，2026-{first_ts} → 2026-{last_ts}，{len(turns)} 轮对话，{n_tools} 次工具调用。  ")
md.append("> 用户提问与助手回复保留全文；工具调用只列摘要（读了什么、改了哪个文件、跑了什么命令），文件内容以仓库当前版本为准。  ")
md.append("> 会话中途经历过一次上下文压缩，压缩前后的记录都在这里。\n")
md.append("## 目录\n")
for i, t in enumerate(turns, 1):
    title = t["query"].split("\n")[0]
    title = re.sub(r"\s+", " ", title)[:60]
    md.append(f"{i}. [{short_ts(t['ts'])} · {title}](#{i})")
md.append("\n---\n")

for i, t in enumerate(turns, 1):
    title = re.sub(r"\s+", " ", t["query"].split("\n")[0])[:60]
    md.append(f'<a id="{i}"></a>\n')
    md.append(f"## {i} · {short_ts(t['ts'])} · {title}\n")
    md.append("**用户**\n")
    md.append("\n".join("> " + l for l in t["query"].split("\n")) + "\n")
    if t["uploads"]:
        md.append("附件：" + "、".join(f"`{Path(u).name}`" for u in t["uploads"]) + "\n")
    md.append("**助手**\n")
    for kind, payload in t["parts"]:
        if kind == "text":
            md.append(demote_headings(payload) + "\n")
        else:
            md.append(f"<details><summary>工具调用 × {len(payload)}</summary>\n")
            md.append("\n".join(f"- {s}" for s in payload))
            md.append("\n</details>\n")
    md.append("---\n")

OUT.parent.mkdir(parents=True, exist_ok=True)
OUT.write_text("\n".join(md), encoding="utf-8")
print(f"{len(turns)} turns, {n_tools} tool calls -> {OUT} ({OUT.stat().st_size // 1024} KB)")
