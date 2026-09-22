//! vault 文件监听（方案 12）：`notify`（macOS 上是 FSEvents）→ 专用线程按静默窗口合并 → 前台一批一批处理。
//!
//! 监听只负责"哪些路径动过"，不判断是谁动的：编辑器自己的保存也会触发事件，由下游按指纹筛掉
//! （编辑器比 `FileStamp`，索引比元数据里的指纹）。一批里区分三类：
//! - `changed`：现在存在于磁盘上的笔记（新建 / 修改 / 重命名的目标）；
//! - `removed`：已不在磁盘上的笔记（删除 / 重命名的来源 / 移出 vault）；
//! - `rescan`：目录级变化或事件溢出——文件树整体重扫，索引做一次增量对齐。
//!
//! 隐藏路径（`.aero/`、`.git/`、临时文件）与 `*.assets/` 内的东西直接忽略。
//!
//! 线程 → 前台不用通道唤醒而是前台定时取：与索引服务一致，GPUI 的测试调度器不允许外部线程唤醒任务。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aero_core::{Vault, VaultPath};
use gpui_kit::*;
use notify::{RecursiveMode, Watcher as _};

/// 一连串事件之后静默这么久才算一批（原子保存 = 临时文件 + rename；同步工具一次落很多文件）
const SETTLE: Duration = Duration::from_millis(300);
/// 一批最多攒这么久，持续写入时也能及时看到前面的变化
const MAX_BATCH: Duration = Duration::from_secs(2);
/// 前台取批的间隔
pub const POLL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Batch {
    pub changed: Vec<VaultPath>,
    pub removed: Vec<VaultPath>,
    /// 动过的附件（图片 / PDF，含 `*.assets/` 里的）：渲染缓存据此失效，不影响文件树
    pub attachments: Vec<VaultPath>,
    pub rescan: bool,
}

impl Batch {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.removed.is_empty() && self.attachments.is_empty() && !self.rescan
    }
}

pub enum WatchEvent {
    Changed(Batch),
}

pub struct VaultWatcher {
    _watcher: Option<notify::RecommendedWatcher>,
    _task: Option<Task<()>>,
    /// 监听起不来（权限、文件描述符耗尽等）：编辑器退回 2 s 轮询，索引只在启动时对齐
    pub error: Option<String>,
}

impl EventEmitter<WatchEvent> for VaultWatcher {}

impl VaultWatcher {
    pub fn new(vault: Arc<Vault>, cx: &mut Context<Self>) -> Self {
        let roots = Roots::of(vault.root());
        let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<notify::Event>>();
        let pending: Arc<Mutex<Vec<Batch>>> = Arc::default();
        let watcher = notify::recommended_watcher(move |res| {
            let _ = raw_tx.send(res);
        })
        .and_then(|mut w| w.watch(vault.root(), RecursiveMode::Recursive).map(|()| w));
        let watcher = match watcher {
            Ok(w) => w,
            Err(e) => {
                log::warn!("watch {}: {e}", vault.root().display());
                return VaultWatcher { _watcher: None, _task: None, error: Some(e.to_string()) };
            }
        };
        let sink = pending.clone();
        let spawned = std::thread::Builder::new()
            .name("aero-watch".into())
            .spawn(move || debounce_loop(&roots, &raw_rx, &sink));
        if let Err(e) = spawned {
            log::warn!("watch thread: {e}");
            return VaultWatcher { _watcher: None, _task: None, error: Some(e.to_string()) };
        }
        let task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let batches: Vec<Batch> = std::mem::take(&mut *pending.lock().unwrap());
                let alive = this.update(cx, |_, cx| {
                    for batch in batches {
                        cx.emit(WatchEvent::Changed(batch));
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        VaultWatcher { _watcher: Some(watcher), _task: Some(task), error: None }
    }
}

/// vault 根的两种写法：用户给的与解析过符号链接的（FSEvents 报的是真实路径，`/var` → `/private/var`）。
struct Roots {
    given: PathBuf,
    real: PathBuf,
}

impl Roots {
    fn of(root: &Path) -> Roots {
        Roots {
            given: root.to_path_buf(),
            real: std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()),
        }
    }

    fn relative<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        path.strip_prefix(&self.real).or_else(|_| path.strip_prefix(&self.given)).ok()
    }
}

/// 某条路径对我们意味着什么。
#[derive(Debug, PartialEq, Eq)]
enum Interest {
    /// 一篇笔记
    Note(VaultPath),
    /// 一个附件（图片 / PDF）：只影响渲染缓存
    Attachment(VaultPath),
    /// 目录或别的文件：文件树可能要重扫
    Other,
    /// 隐藏路径、附件目录里的非附件文件、vault 之外：不关心
    Ignored,
}

fn interest(roots: &Roots, path: &Path) -> Interest {
    let Some(rel) = roots.relative(path) else { return Interest::Ignored };
    let Some(rel) = rel.to_str() else { return Interest::Ignored };
    if rel.is_empty() {
        return Interest::Ignored;
    }
    let components: Vec<&str> = rel.split(std::path::MAIN_SEPARATOR).collect();
    if components.iter().any(|c| c.starts_with('.')) {
        return Interest::Ignored;
    }
    let in_assets = components.iter().any(|c| c.ends_with(".assets"));
    match VaultPath::new(rel) {
        Ok(p) if p.is_note() && !in_assets => Interest::Note(p),
        // 附件按后缀认，`x.assets/` 里的也要认：渲染缓存靠它失效
        Ok(p) if aero_core::attachments::Kind::for_path(p.as_str()).is_some() => Interest::Attachment(p),
        // `x.assets/` 里的其他东西不关心；目录本身的出现 / 消失也不影响树（树不显示它）
        Ok(_) if in_assets => Interest::Ignored,
        Ok(_) => Interest::Other,
        Err(_) => Interest::Ignored,
    }
}

#[derive(Default)]
struct Accumulator {
    paths: BTreeSet<PathBuf>,
    rescan: bool,
}

impl Accumulator {
    fn push(&mut self, event: notify::Result<notify::Event>) {
        match event {
            Ok(event) => {
                if event.need_rescan() {
                    self.rescan = true;
                }
                if matches!(event.kind, notify::EventKind::Access(_)) {
                    return;
                }
                self.paths.extend(event.paths);
            }
            Err(e) => {
                // 事件队列溢出等：不知道丢了什么，整体重扫
                log::warn!("watch: {e}");
                self.rescan = true;
            }
        }
    }

    fn finish(self, roots: &Roots) -> Batch {
        classify(roots, self.paths.iter().map(PathBuf::as_path), self.rescan)
    }
}

/// 把一批触碰过的路径分成"还在的笔记 / 不在了的笔记 / 需要重扫"。
fn classify<'a>(roots: &Roots, paths: impl Iterator<Item = &'a Path>, mut rescan: bool) -> Batch {
    let mut changed = Vec::new();
    let mut removed = Vec::new();
    let mut attachments = Vec::new();
    for path in paths {
        match interest(roots, path) {
            Interest::Note(note) => {
                if path.is_file() {
                    changed.push(note);
                } else {
                    removed.push(note);
                }
            }
            // 新增 / 改动 / 删除都只是"要重新渲染"，存在性由渲染服务自己判断
            Interest::Attachment(file) => attachments.push(file),
            Interest::Other => rescan = true,
            Interest::Ignored => {}
        }
    }
    Batch { changed, removed, attachments, rescan }
}

fn debounce_loop(
    roots: &Roots,
    rx: &mpsc::Receiver<notify::Result<notify::Event>>,
    sink: &Mutex<Vec<Batch>>,
) {
    loop {
        let Ok(first) = rx.recv() else { return };
        let mut acc = Accumulator::default();
        acc.push(first);
        let started = Instant::now();
        let mut disconnected = false;
        loop {
            match rx.recv_timeout(SETTLE) {
                Ok(event) => {
                    acc.push(event);
                    if started.elapsed() >= MAX_BATCH {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        let batch = acc.finish(roots);
        if !batch.is_empty() {
            sink.lock().unwrap().push(batch);
        }
        if disconnected {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会把 GPUI 的 `test` 属性宏带进作用域，遮住标准 `#[test]`
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use aero_core::{Vault, VaultPath};
    use gpui_kit::{AppContext as _, TestAppContext};

    use super::{Batch, POLL, Roots, VaultWatcher, WatchEvent, classify};

    #[test]
    fn classify_splits_notes_by_existence_and_ignores_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(root.join(".aero")).unwrap();
        std::fs::create_dir_all(root.join("pic.assets")).unwrap();
        std::fs::write(root.join("a.md"), "a").unwrap();
        std::fs::write(root.join("sub/b.md"), "b").unwrap();
        std::fs::write(root.join(".aero/vault.toml"), "").unwrap();
        std::fs::write(root.join("pic.assets/x.md"), "").unwrap();
        std::fs::write(root.join("notes.txt"), "").unwrap();
        let roots = Roots::of(root);

        let touched = [
            root.join("a.md"),
            root.join("sub/b.md"),
            root.join("sub/gone.md"),
            root.join(".aero/vault.toml"),
            root.join(".a.md.tmp"),
            root.join("pic.assets/x.md"),
            root.to_path_buf(),
        ];
        let batch = classify(&roots, touched.iter().map(PathBuf::as_path), false);
        assert_eq!(batch.changed, [VaultPath::new("a.md").unwrap(), VaultPath::new("sub/b.md").unwrap()]);
        assert_eq!(batch.removed, [VaultPath::new("sub/gone.md").unwrap()]);
        assert!(!batch.rescan);

        // 非笔记文件 / 目录：只要求重扫
        let batch =
            classify(&roots, [root.join("notes.txt"), root.join("sub")].iter().map(PathBuf::as_path), false);
        assert!(batch.changed.is_empty() && batch.removed.is_empty());
        assert!(batch.rescan);

        // 真实路径（/private/var/...）也能对回 vault
        let real = std::fs::canonicalize(root).unwrap();
        let batch = classify(&roots, [real.join("a.md")].iter().map(PathBuf::as_path), false);
        assert_eq!(batch.changed, [VaultPath::new("a.md").unwrap()]);

        // 空批
        assert!(classify(&roots, std::iter::empty(), false).is_empty());
        assert!(!classify(&roots, std::iter::empty(), true).is_empty());
    }

    /// 真监听：外部写入 / 删除各到一批。FSEvents 延迟不确定，最多等 5 s。
    #[gpui_kit::test]
    fn watcher_reports_external_writes_and_removals(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.md"), "keep\n").unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let batches: Arc<Mutex<Vec<Batch>>> = Arc::default();
        let watcher = cx.new(|cx| VaultWatcher::new(vault.clone(), cx));
        let sink = batches.clone();
        cx.update(|cx| {
            assert!(watcher.read(cx).error.is_none());
            cx.subscribe(&watcher, move |_, event, _| {
                let WatchEvent::Changed(batch) = event;
                sink.lock().unwrap().push(batch.clone());
            })
            .detach();
        });
        // 给 FSEvents 一点时间建立流；早于此的事件可能收不到
        std::thread::sleep(Duration::from_millis(300));

        std::fs::write(dir.path().join("new.md"), "hello\n").unwrap();
        wait_for(cx, &batches, |b| b.iter().any(|b| b.changed.contains(&VaultPath::new("new.md").unwrap())));
        assert!(
            !batches.lock().unwrap().iter().any(|b| !b.removed.is_empty()),
            "{:?}",
            batches.lock().unwrap()
        );

        std::fs::remove_file(dir.path().join("keep.md")).unwrap();
        wait_for(cx, &batches, |b| b.iter().any(|b| b.removed.contains(&VaultPath::new("keep.md").unwrap())));

        // 隐藏文件不产生批
        let before = batches.lock().unwrap().len();
        std::fs::write(dir.path().join(".aero").join("ignore"), "").unwrap();
        std::thread::sleep(Duration::from_millis(800));
        cx.executor().advance_clock(POLL);
        cx.run_until_parked();
        assert_eq!(batches.lock().unwrap().len(), before);
    }

    /// 监听线程是真线程、前台取批的定时器是假时钟：真实等待 + 推进时钟。
    fn wait_for(cx: &mut TestAppContext, batches: &Arc<Mutex<Vec<Batch>>>, ok: impl Fn(&[Batch]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            cx.executor().advance_clock(POLL);
            cx.run_until_parked();
            if ok(&batches.lock().unwrap()) {
                return;
            }
            assert!(Instant::now() < deadline, "no matching batch within 5 s: {:?}", batches.lock().unwrap());
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
