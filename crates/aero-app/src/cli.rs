//! 命令行（方案 13.1）。图形界面、这里、以及以后的 MCP 都调用同一套 vault 与索引。
//!
//! 无参数或 `aero <库目录>` 仍启动窗口。出现子命令、`-h` / `--help` 或其他旗标时走这里。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aero_core::links::{Resolution, creation_path};
use aero_core::{Document, Vault, VaultPath};
use aero_index::{Indexer, Meta, ZvecFts};
use clap::{Parser, Subcommand};
use serde_json::json;

use crate::paths::{PersistedState, app_data_dir};

#[derive(Parser, Debug)]
#[command(
    name = "aero",
    about = "Aero 知识库。不带子命令时启动图形界面；aero <库目录> 直接打开该库。",
    disable_help_subcommand = true
)]
struct Cli {
    /// 知识库目录。缺省时用环境变量 AERO_VAULT，再否则用最近打开且仍在的库
    #[arg(long, global = true)]
    vault: Option<PathBuf>,
    /// 以 JSON 输出到标准输出
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 本地诊断，不需要打开知识库，也不会联网
    Diagnostics {
        #[command(subcommand)]
        action: DiagnosticCmd,
    },
    /// 全文检索，结果定位到块
    Search {
        /// 查询。`#tag` / `tag:tag` 与搜索框同一套语法；可以只给 `--tag`
        #[arg(default_value = "")]
        query: String,
        /// 再要求一个标签（可重复）。写成 `rust` 或 `#rust` 都可以
        #[arg(long)]
        tag: Vec<String>,
        /// 最多返回多少条
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 读一篇笔记，或其中一块
    Read {
        /// 路径、文件名、标题、别名，或索引里的 note id
        note: String,
        /// 只输出这个块（`^id` 或 `id`）
        #[arg(long)]
        block: Option<String>,
    },
    /// 列出指向这篇笔记的引用
    Backlinks {
        note: String,
        /// 只看指向这个块的引用
        #[arg(long)]
        block: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// 在笔记末尾追加一段 Markdown
    Append {
        note: String,
        #[arg(long)]
        text: String,
    },
    /// 新建一篇笔记。已存在则失败，不覆盖
    New {
        /// 库内路径。不带 `.md` 时自动补上
        path: String,
        /// `.aero/templates/` 里的模板名（不含 `.md`）
        #[arg(long)]
        template: Option<String>,
    },
    /// 导出一篇笔记
    Export {
        note: String,
        /// `md` 源码，`html` 渲染结果，`json` 路径、标题、源码和块列表
        #[arg(long, value_enum)]
        format: ExportFormat,
    },
    /// 索引状态，或清空后按磁盘重建
    Index {
        #[command(subcommand)]
        action: IndexCmd,
    },
}

#[derive(Subcommand, Debug)]
enum DiagnosticCmd {
    /// 导出可检查的本地诊断包。目标文件必须尚不存在
    Export { output: PathBuf },
    /// 显示本地日志目录
    Path,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum ExportFormat {
    Md,
    Html,
    Json,
}

#[derive(Subcommand, Debug)]
enum IndexCmd {
    /// 已索引的笔记数、块数，以及磁盘上的笔记数
    Status,
    /// 清空索引并按当前磁盘全量重建
    Rebuild,
}

/// 这些参数启动命令行，而不是窗口。
pub fn wants_cli(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some(flag) if flag.starts_with('-') => true,
        Some(
            "search" | "read" | "backlinks" | "append" | "new" | "index" | "export" | "diagnostics" | "help",
        ) => true,
        _ => false,
    }
}

pub fn run(args: &[String]) -> i32 {
    let cli = match Cli::try_parse_from(std::iter::once("aero".to_owned()).chain(args.iter().cloned())) {
        Ok(cli) => cli,
        Err(err) => {
            let code = if err.use_stderr() { 2 } else { 0 };
            let _ = err.print();
            return code;
        }
    };
    match execute(&cli, &app_data_dir()) {
        Ok(text) => {
            if !text.is_empty() {
                println!("{text}");
            }
            0
        }
        Err(err) => {
            eprintln!("{err}");
            1
        }
    }
}

fn execute(cli: &Cli, data_dir: &Path) -> Result<String, String> {
    if let Cmd::Diagnostics { action } = &cli.command {
        return match action {
            DiagnosticCmd::Export { output } => crate::diagnostics::export(output)
                .map(|_| {
                    if cli.json {
                        json!({"exported": output}).to_string()
                    } else {
                        format!("已导出：{}", output.display())
                    }
                })
                .map_err(str::to_owned),
            DiagnosticCmd::Path => Ok(if cli.json {
                json!({"path": crate::diagnostics::log_directory()}).to_string()
            } else {
                crate::diagnostics::log_directory().display().to_string()
            }),
        };
    }
    let operation = crate::diagnostics::Operation::new(
        match &cli.command {
            Cmd::Search { .. } => "cli_search",
            Cmd::Read { .. } => "cli_read",
            Cmd::Backlinks { .. } => "cli_backlinks",
            Cmd::Append { .. } => "cli_append",
            Cmd::New { .. } => "cli_new",
            Cmd::Export { .. } => "cli_export",
            Cmd::Index { .. } => "cli_index",
            Cmd::Diagnostics { .. } => unreachable!(),
        },
        Default::default(),
    );
    let result = operation.scope(|| execute_vault(cli, data_dir));
    operation.finish(if result.is_ok() {
        crate::diagnostics::Outcome::Ok
    } else {
        crate::diagnostics::Outcome::Failed
    });
    result
}

fn execute_vault(cli: &Cli, data_dir: &Path) -> Result<String, String> {
    let root = vault_root(cli.vault.as_deref(), data_dir)?;
    let vault = Arc::new(Vault::open(&root).map_err(|e| e.to_string())?);
    match &cli.command {
        Cmd::Read { note, block } => {
            if let Some(path) = existing_note(&vault, note) {
                return read_at(&vault, &path, block.as_deref(), cli.json);
            }
            let mut indexer = open_index(&vault, data_dir, false)?.0;
            read(&mut indexer, &vault, note, block.as_deref(), cli.json)
        }
        Cmd::Export { note, format } => {
            if let Some(path) = existing_note(&vault, note) {
                return export_at(&vault, &path, *format, cli.json);
            }
            let mut indexer = open_index(&vault, data_dir, false)?.0;
            export_note(&mut indexer, &vault, note, *format, cli.json)
        }
        Cmd::Append { note, text } => {
            if let Some(path) = existing_note(&vault, note) {
                return append_at(&vault, &path, text, cli.json, data_dir);
            }
            let mut indexer = open_index(&vault, data_dir, false)?.0;
            append(&mut indexer, &vault, note, text, cli.json)
        }
        Cmd::New { path, template } => create_note(&vault, path, template.as_deref(), cli.json, data_dir),
        command => {
            let allow_snapshot = !matches!(command, Cmd::Index { action: IndexCmd::Rebuild });
            let (mut indexer, _snapshot) = open_index(&vault, data_dir, allow_snapshot)?;
            match command {
                Cmd::Search { query, tag, limit } => search(&indexer, query, tag, *limit, cli.json),
                Cmd::Backlinks { note, block, limit } => {
                    backlinks(&mut indexer, &vault, note, block.as_deref(), *limit, cli.json)
                }
                Cmd::Index { action } => match action {
                    IndexCmd::Status => status(&indexer, &vault, cli.json),
                    IndexCmd::Rebuild => rebuild(&mut indexer, cli.json),
                },
                Cmd::Read { .. }
                | Cmd::Export { .. }
                | Cmd::Append { .. }
                | Cmd::New { .. }
                | Cmd::Diagnostics { .. } => {
                    unreachable!("已在上面处理")
                }
            }
        }
    }
}

fn vault_root(flag: Option<&Path>, data_dir: &Path) -> Result<PathBuf, String> {
    if let Some(path) = flag {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("AERO_VAULT") {
        return Ok(PathBuf::from(path));
    }
    PersistedState::load(data_dir)
        .recent_vaults
        .into_iter()
        .find(|path| path.is_dir())
        .ok_or_else(|| "没有打开的知识库。请用 --vault 指定目录，或先在 Aero 里打开一次。".to_owned())
}

fn open_index(
    vault: &Arc<Vault>,
    data_dir: &Path,
    snapshot_if_locked: bool,
) -> Result<(Indexer, Option<tempfile::TempDir>), String> {
    match Indexer::open(Arc::clone(vault), &data_dir.join("index")) {
        Ok(indexer) => Ok((indexer, None)),
        Err(err) if snapshot_if_locked && is_lock_error(&err) => {
            let (indexer, snapshot) = open_snapshot(vault, data_dir)?;
            Ok((indexer, Some(snapshot)))
        }
        Err(err) => {
            let message = err.to_string();
            if is_lock_error(&err) {
                Err(format!(
                    "无法打开索引：{message}。搜索、反链和 index 要等 Aero 退出后才能用；读、导出、新建和追加不需要这份锁。"
                ))
            } else {
                Err(format!("无法打开索引：{message}"))
            }
        }
    }
}

/// 图形界面占着引擎锁时，复制一份全文索引再打开。元数据库用 WAL，可以同时读。
fn open_snapshot(vault: &Arc<Vault>, data_dir: &Path) -> Result<(Indexer, tempfile::TempDir), String> {
    let dir = data_dir.join("index").join(vault.id());
    let snapshot = tempfile::tempdir().map_err(|e| e.to_string())?;
    copy_index_dir(&dir.join("fts"), &snapshot.path().join("fts"))?;
    let meta = Meta::open(&dir.join("meta.sqlite")).map_err(|e| e.to_string())?;
    let fts = ZvecFts::open(&snapshot.path().join("fts")).map_err(|e| e.to_string())?;
    Ok((Indexer::with_parts(Arc::clone(vault), meta, Box::new(fts)), snapshot))
}

fn copy_index_dir(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(from).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        if name == "LOCK" {
            continue;
        }
        let dest = to.join(&name);
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_index_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), dest).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn is_lock_error(err: &aero_index::IndexerError) -> bool {
    err.to_string().to_ascii_lowercase().contains("can't lock")
}

fn locate(indexer: &mut Indexer, vault: &Vault, target: &str) -> Result<VaultPath, String> {
    if let Some(path) = existing_note(vault, target) {
        return Ok(path);
    }
    match indexer.resolve_note(target).map_err(|e| e.to_string())? {
        Resolution::Resolved { path, .. } if vault.exists(&path) => Ok(path),
        Resolution::Resolved { path, .. } => Err(format!("索引里的笔记已不在磁盘上：{}", path.as_str())),
        Resolution::Ambiguous { candidates } => Err(format!(
            "有多篇笔记匹配「{target}」，请写出路径：{}",
            candidates.iter().map(VaultPath::as_str).collect::<Vec<_>>().join("、")
        )),
        Resolution::Unresolved => Err(format!("找不到笔记：{target}")),
    }
}

fn existing_note(vault: &Vault, target: &str) -> Option<VaultPath> {
    let target = target.trim();
    for candidate in [target.to_owned(), format!("{target}.md")] {
        let Ok(path) = VaultPath::new(&candidate) else { continue };
        if vault.exists(&path) {
            return Some(path);
        }
    }
    None
}

fn search(
    indexer: &Indexer,
    query: &str,
    tags: &[String],
    limit: usize,
    json: bool,
) -> Result<String, String> {
    let mut text = query.to_owned();
    for tag in tags {
        let tag = tag.trim().trim_start_matches('#');
        if !tag.is_empty() {
            text.push_str(" tag:");
            text.push_str(tag);
        }
    }
    if aero_core::FtsQuery::parse(&text).is_empty() {
        return Err("请提供查询文字或 --tag。".into());
    }
    let hits = indexer.search(&text, limit.max(1)).map_err(|e| e.to_string())?;
    if json {
        return Ok(serde_json::to_string_pretty(&json!(
            hits.iter()
                .map(|hit| json!({
                    "path": hit.path.as_str(),
                    "title": hit.title,
                    "block_ord": hit.block_ord,
                    "score": hit.score,
                    "body": hit.body,
                }))
                .collect::<Vec<_>>()
        ))
        .map_err(|e| e.to_string())?);
    }
    if hits.is_empty() {
        return Ok("没有结果".into());
    }
    Ok(hits
        .iter()
        .map(|hit| {
            let body = hit.body.split_whitespace().collect::<Vec<_>>().join(" ");
            let body: String = body.chars().take(80).collect();
            format!("{}\t{}\n  {body}", hit.path.as_str(), hit.title)
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

fn read(
    indexer: &mut Indexer,
    vault: &Vault,
    note: &str,
    block: Option<&str>,
    json: bool,
) -> Result<String, String> {
    let path = locate(indexer, vault, note)?;
    read_at(vault, &path, block, json)
}

fn read_at(vault: &Vault, path: &VaultPath, block: Option<&str>, json: bool) -> Result<String, String> {
    let (doc, _) = vault.read(&path).map_err(|e| e.to_string())?;
    let (text, block_id) = match block {
        Some(id) => (block_text(&doc, id)?, Some(id.trim().trim_start_matches('^').to_owned())),
        None => (doc.to_string(), None),
    };
    if json {
        return Ok(serde_json::to_string_pretty(&json!({
            "path": path.as_str(),
            "title": doc.title_or(path.stem()),
            "block": block_id,
            "text": text,
        }))
        .map_err(|e| e.to_string())?);
    }
    Ok(text.trim_end().to_owned())
}

fn block_text(doc: &Document, id: &str) -> Result<String, String> {
    let id = id.trim().trim_start_matches('^');
    if id.is_empty() {
        return Err("请提供块 id。".into());
    }
    let block_id = aero_core::block_refs::resolve(doc, id)?;
    let block = doc.blocks().iter().find(|(_, block)| block.id == block_id).map(|(_, block)| block).unwrap();
    Ok(doc.slice(doc.range_of(block)).to_string())
}

fn backlinks(
    indexer: &mut Indexer,
    vault: &Vault,
    note: &str,
    block: Option<&str>,
    limit: usize,
    json: bool,
) -> Result<String, String> {
    let path = locate(indexer, vault, note)?;
    let fragment = block.map(|id| {
        let id = id.trim().trim_start_matches('^');
        format!("^{id}")
    });
    let page = indexer.backlinks(&path, fragment.as_deref(), limit.max(1)).map_err(|e| e.to_string())?;
    if json {
        return Ok(serde_json::to_string_pretty(&json!({
            "path": path.as_str(),
            "total": page.total,
            "matched": page.matched,
            "hits": page.hits.iter().map(|hit| json!({
                "path": hit.path.as_str(),
                "title": hit.title,
                "label": hit.label,
                "body": hit.body,
            })).collect::<Vec<_>>(),
        }))
        .map_err(|e| e.to_string())?);
    }
    let mut lines = vec![format!("{} 条反向链接", page.total)];
    if fragment.is_some() {
        lines.push(format!("其中 {} 条指向这一块", page.matched));
    }
    for hit in &page.hits {
        let body = hit.body.split_whitespace().collect::<Vec<_>>().join(" ");
        lines.push(format!("{}\t{}\n  {body}", hit.path.as_str(), hit.title));
    }
    if page.hits.is_empty() {
        lines.push("没有条目".into());
    }
    Ok(lines.join("\n"))
}

fn append(
    indexer: &mut Indexer,
    vault: &Vault,
    note: &str,
    text: &str,
    json: bool,
) -> Result<String, String> {
    let path = locate(indexer, vault, note)?;
    write_append(vault, &path, text)?;
    reindex(indexer, &path)?;
    written(&path, json)
}

fn append_at(
    vault: &Arc<Vault>,
    path: &VaultPath,
    text: &str,
    json: bool,
    data_dir: &Path,
) -> Result<String, String> {
    write_append(vault, path, text)?;
    try_reindex(vault, data_dir, path)?;
    written(path, json)
}

fn write_append(vault: &Vault, path: &VaultPath, text: &str) -> Result<(), String> {
    let (bytes, _) = vault.read_bytes(path).map_err(|e| e.to_string())?;
    let existing = String::from_utf8(bytes).map_err(|_| format!("笔记不是 UTF-8：{}", path.as_str()))?;
    let next = join_markdown(&existing, text)?;
    let doc = Document::from_text(&next);
    vault.write(path, &doc).map(|_| ()).map_err(|e| e.to_string())
}

fn create_note(
    vault: &Arc<Vault>,
    path: &str,
    template: Option<&str>,
    json: bool,
    data_dir: &Path,
) -> Result<String, String> {
    let path = write_new(vault, path, template)?;
    try_reindex(vault, data_dir, &path)?;
    written(&path, json)
}

fn write_new(vault: &Vault, path: &str, template: Option<&str>) -> Result<VaultPath, String> {
    let path = creation_path("", path).ok_or_else(|| format!("无效的笔记路径：{path}"))?;
    if vault.exists(&path) {
        return Err(format!("笔记已存在：{}", path.as_str()));
    }
    let body = match template {
        Some(name) => template_body(vault, name)?,
        None => format!("# {}\n\n", path.stem()),
    };
    vault.create_bytes(&path, body.as_bytes()).map_err(|e| e.to_string())?;
    Ok(path)
}

fn template_body(vault: &Vault, name: &str) -> Result<String, String> {
    if name.is_empty() || name.contains(['/', '\\']) || name == "." || name == ".." {
        return Err("模板名不能包含路径。".into());
    }
    let file = vault.root().join(".aero").join("templates").join(format!("{name}.md"));
    std::fs::read_to_string(&file).map_err(|_| format!("没有模板：{name}"))
}

fn join_markdown(existing: &str, addition: &str) -> Result<String, String> {
    let addition = addition.trim_end_matches(['\r', '\n']);
    if addition.trim().is_empty() {
        return Err("追加的文字是空的。".into());
    }
    let mut out = existing.to_owned();
    if out.is_empty() {
        out.push_str(addition);
    } else {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(addition);
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

fn reindex(indexer: &mut Indexer, path: &VaultPath) -> Result<(), String> {
    indexer.refresh_note(path).map_err(|e| e.to_string())?;
    indexer.flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// 索引被图形界面占用时不失败：文件已经落盘，监听会把它收进索引。
fn try_reindex(vault: &Arc<Vault>, data_dir: &Path, path: &VaultPath) -> Result<(), String> {
    match Indexer::open(Arc::clone(vault), &data_dir.join("index")) {
        Ok(mut indexer) => reindex(&mut indexer, path),
        Err(err) if is_lock_error(&err) => Ok(()),
        Err(err) => Err(format!("无法打开索引：{err}")),
    }
}

fn written(path: &VaultPath, json: bool) -> Result<String, String> {
    if json {
        Ok(serde_json::to_string_pretty(&json!({ "path": path.as_str() })).map_err(|e| e.to_string())?)
    } else {
        Ok(path.as_str().to_owned())
    }
}

fn status(indexer: &Indexer, vault: &Vault, json: bool) -> Result<String, String> {
    let (notes, blocks) = indexer.stats();
    let on_disk = vault.notes().map_err(|e| e.to_string())?.len();
    if json {
        return Ok(serde_json::to_string_pretty(&json!({
            "notes": notes,
            "blocks": blocks,
            "on_disk": on_disk,
        }))
        .map_err(|e| e.to_string())?);
    }
    let mut text = format!("已索引笔记 {notes}\n块 {blocks}\n磁盘上的笔记 {on_disk}");
    if notes as usize != on_disk {
        text.push_str("\n索引与磁盘不一致。运行 aero index rebuild 对齐。");
    }
    Ok(text)
}

fn rebuild(indexer: &mut Indexer, json: bool) -> Result<String, String> {
    let report = indexer.rebuild(|_, _| {}).map_err(|e| e.to_string())?;
    if !report.errors.is_empty() {
        return Err(report.errors.join("\n"));
    }
    indexer.flush().map_err(|e| e.to_string())?;
    if json {
        return Ok(serde_json::to_string_pretty(&json!({
            "indexed": report.indexed,
            "unchanged": report.unchanged,
            "removed": report.removed,
        }))
        .map_err(|e| e.to_string())?);
    }
    Ok(format!("已重建：写入 {}，未变化 {}，删除 {}", report.indexed, report.unchanged, report.removed))
}

fn export_note(
    indexer: &mut Indexer,
    vault: &Vault,
    note: &str,
    format: ExportFormat,
    json: bool,
) -> Result<String, String> {
    let path = locate(indexer, vault, note)?;
    export_at(vault, &path, format, json)
}

fn export_at(vault: &Vault, path: &VaultPath, format: ExportFormat, json: bool) -> Result<String, String> {
    let (doc, _) = vault.read(&path).map_err(|e| e.to_string())?;
    let markdown = doc.to_string();
    let title = doc.title_or(path.stem());
    let pretty = |value: serde_json::Value| serde_json::to_string_pretty(&value).map_err(|e| e.to_string());
    match format {
        ExportFormat::Md if json => pretty(json!({
            "path": path.as_str(),
            "title": title,
            "markdown": markdown,
        })),
        ExportFormat::Md => Ok(markdown.trim_end().to_owned()),
        ExportFormat::Html => {
            let html = aero_core::highlight::export_html(&markdown);
            if json {
                pretty(json!({
                    "path": path.as_str(),
                    "title": title,
                    "html": html,
                }))
            } else {
                Ok(html)
            }
        }
        ExportFormat::Json => {
            let blocks = doc
                .blocks()
                .iter()
                .map(|(_, block)| {
                    json!({
                        "kind": block.kind.tag(),
                        "text": doc.slice(doc.content_range(block)).to_string(),
                    })
                })
                .collect::<Vec<_>>();
            pretty(json!({
                "path": path.as_str(),
                "title": title,
                "markdown": markdown,
                "blocks": blocks,
            }))
        }
    }
}

#[cfg(test)]
fn execute_args(args: &[&str], data_dir: &Path) -> Result<String, String> {
    let cli = Cli::try_parse_from(std::iter::once("aero").chain(args.iter().copied()))
        .map_err(|e| e.to_string())?;
    execute(&cli, data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault() -> (tempfile::TempDir, tempfile::TempDir) {
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
    }

    #[test]
    fn gui_launch_is_not_a_subcommand() {
        assert!(!wants_cli(&[]));
        assert!(!wants_cli(&["/tmp/notes".into()]));
        assert!(wants_cli(&["search".into(), "ripgrep".into()]));
        assert!(wants_cli(&["--help".into()]));
        assert!(wants_cli(&["index".into(), "status".into()]));
        assert!(wants_cli(&["export".into(), "note".into(), "--format".into(), "md".into()]));
    }

    #[test]
    fn help_lists_the_six_commands() {
        let err = Cli::try_parse_from(["aero", "--help"]).unwrap_err();
        let text = err.to_string();
        for name in ["search", "read", "backlinks", "append", "new", "index", "export"] {
            assert!(text.contains(name), "{text}");
        }
        assert!(!err.use_stderr());
    }

    #[test]
    fn new_append_read_search_and_backlinks_share_one_index() {
        let (dir, data) = vault();
        let root = dir.path();
        let vault_arg = root.to_str().unwrap();
        let created = execute_args(&["--vault", vault_arg, "new", "rust/cli"], data.path()).unwrap();
        assert_eq!(created, "rust/cli.md");
        assert_eq!(std::fs::read_to_string(root.join("rust/cli.md")).unwrap(), "# cli\n\n");

        execute_args(&["--vault", vault_arg, "append", "rust/cli", "--text", "见 [[ripgrep]]"], data.path())
            .unwrap();
        std::fs::write(root.join("ripgrep.md"), "# Ripgrep\n\n一个搜索工具\n").unwrap();
        execute_args(&["--vault", vault_arg, "index", "rebuild"], data.path()).unwrap();

        let body = execute_args(&["--vault", vault_arg, "read", "cli"], data.path()).unwrap();
        assert!(body.contains("见 [[ripgrep]]"), "{body}");
        let block =
            execute_args(&["--vault", vault_arg, "read", "ripgrep", "--block", "^missing"], data.path());
        assert!(block.unwrap_err().contains("找不到块"));

        let hits = execute_args(&["--vault", vault_arg, "--json", "search", "Ripgrep"], data.path()).unwrap();
        let hits: serde_json::Value = serde_json::from_str(&hits).unwrap();
        assert_eq!(hits[0]["path"], "ripgrep.md");

        let links = execute_args(&["--vault", vault_arg, "backlinks", "ripgrep"], data.path()).unwrap();
        assert!(links.contains("rust/cli.md"), "{links}");
        assert!(links.contains('1'), "{links}");
    }

    #[test]
    fn new_refuses_to_overwrite_and_template_is_copied() {
        let (dir, data) = vault();
        let vault_arg = dir.path().to_str().unwrap();
        std::fs::create_dir_all(dir.path().join(".aero/templates")).unwrap();
        std::fs::write(dir.path().join(".aero/templates/meeting.md"), "# 会议\n\n- 议题\n").unwrap();
        let created =
            execute_args(&["--vault", vault_arg, "new", "notes/today", "--template", "meeting"], data.path())
                .unwrap();
        assert_eq!(created, "notes/today.md");
        assert_eq!(std::fs::read_to_string(dir.path().join("notes/today.md")).unwrap(), "# 会议\n\n- 议题\n");
        let again = execute_args(&["--vault", vault_arg, "new", "notes/today"], data.path());
        assert!(again.unwrap_err().contains("已存在"));
        let missing =
            execute_args(&["--vault", vault_arg, "new", "x", "--template", "../vault"], data.path());
        assert!(missing.unwrap_err().contains("模板名"));
    }

    #[test]
    fn recent_vault_is_used_when_no_flag_is_given() {
        let (dir, data) = vault();
        std::fs::write(dir.path().join("a.md"), "正文\n").unwrap();
        let mut state = PersistedState::default();
        state.touch_vault(dir.path());
        state.save(data.path());
        let previous = std::env::var_os("AERO_VAULT");
        unsafe { std::env::remove_var("AERO_VAULT") };
        let text = execute_args(&["read", "a"], data.path());
        if let Some(value) = previous {
            unsafe { std::env::set_var("AERO_VAULT", value) };
        }
        assert_eq!(text.unwrap(), "正文");
    }

    #[test]
    fn append_keeps_a_blank_line_and_rejects_empty_text() {
        assert_eq!(join_markdown("# 标题\n", "下一段").unwrap(), "# 标题\n\n下一段\n");
        assert_eq!(join_markdown("# 标题\n\n", "下一段\n").unwrap(), "# 标题\n\n下一段\n");
        assert!(join_markdown("x\n", "  \n").is_err());
    }

    #[test]
    fn export_writes_markdown_html_and_json() {
        let (dir, data) = vault();
        let vault_arg = dir.path().to_str().unwrap();
        std::fs::write(dir.path().join("note.md"), "# 标题\n\n前 [文字]{.red} 后\n").unwrap();
        let md =
            execute_args(&["--vault", vault_arg, "export", "note", "--format", "md"], data.path()).unwrap();
        assert!(md.contains("# 标题"), "{md}");
        let html =
            execute_args(&["--vault", vault_arg, "export", "note", "--format", "html"], data.path()).unwrap();
        assert!(html.contains("<h1>标题</h1>"), "{html}");
        assert!(html.contains("<span class=\"red\">文字</span>"), "{html}");
        let raw =
            execute_args(&["--vault", vault_arg, "export", "note", "--format", "json"], data.path()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["path"], "note.md");
        assert_eq!(value["title"], "note");
        assert!(value["markdown"].as_str().unwrap().contains("[文字]{.red}"));
        assert!(value["blocks"].as_array().unwrap().len() >= 2, "{raw}");
    }

    #[test]
    fn file_commands_work_while_the_index_lock_is_held() {
        let (dir, data) = vault();
        let vault_arg = dir.path().to_str().unwrap();
        std::fs::write(dir.path().join("a.md"), "正文\n").unwrap();
        let opened = Arc::new(aero_core::Vault::open(dir.path()).unwrap());
        let _held = aero_index::Indexer::open(opened, &data.path().join("index")).unwrap();
        let text = execute_args(&["--vault", vault_arg, "read", "a.md"], data.path()).unwrap();
        assert_eq!(text, "正文");
        let exported =
            execute_args(&["--vault", vault_arg, "export", "a.md", "--format", "md"], data.path()).unwrap();
        assert!(exported.contains("正文"), "{exported}");
        let created = execute_args(&["--vault", vault_arg, "new", "fresh"], data.path()).unwrap();
        assert_eq!(created, "fresh.md");
        assert!(dir.path().join("fresh.md").is_file());
        execute_args(&["--vault", vault_arg, "append", "a.md", "--text", "又一段"], data.path()).unwrap();
        assert!(std::fs::read_to_string(dir.path().join("a.md")).unwrap().contains("又一段"));
        let search = execute_args(&["--vault", vault_arg, "search", "正文"], data.path()).unwrap();
        assert!(search.contains("没有结果") || search.contains("a.md"), "{search}");
    }
}
