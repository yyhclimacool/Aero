//! 索引服务：把 `aero_index::Indexer`（同步、阻塞）包成一个实体，UI 只跟它打交道。
//!
//! - 打开 vault 时在后台线程打开索引并与磁盘对齐（每篇一把锁，期间搜索与保存不用等整个同步）；
//! - 编辑器保存 / 重命名 / 删除后增量更新；文件监听报告的路径按指纹对齐（`note_touched`）；
//! - 空闲 30 s `flush()`，退出时同步 flush（ADR 0003：保存路径上不 flush）；
//! - `optimize()`：全量同步写入超过阈值后立刻做一次；平时累计增量写入达到阈值、且距上次超过 10 分钟，
//!   在空闲 flush 之后顺带做（合并段文件，搜索不会越用越慢）；
//! - 进度用原子量暴露，前台定时器轮询刷新状态栏（`ui_design.md` 2.1）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aero_core::{FileStamp, Vault, VaultPath};
use aero_index::{Indexer, Refresh, SearchHit, SyncReport};
use gpui_kit::*;

const IDLE_FLUSH: Duration = Duration::from_secs(30);
const PROGRESS_POLL: Duration = Duration::from_millis(150);
/// 全量同步至少（重新）索引这么多篇才值得 optimize
const OPTIMIZE_AFTER_SYNC_NOTES: usize = 50;
/// 增量写入累计这么多次、且距上次 optimize 超过 [`OPTIMIZE_MIN_INTERVAL`] 时，在空闲 flush 后 optimize
const OPTIMIZE_AFTER_WRITES: usize = 200;
const OPTIMIZE_MIN_INTERVAL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// 正在打开引擎 / 元数据库
    Opening,
    /// 全量对齐中：已处理 / 总数
    Syncing {
        done: usize,
        total: usize,
    },
    Idle,
    /// 引擎打不开：搜索不可用，编辑照常
    Failed(String),
}

#[derive(Default)]
struct Shared {
    phase: AtomicU8,
    done: AtomicUsize,
    total: AtomicUsize,
    cancelled: AtomicBool,
    error: Mutex<Option<String>>,
    report: Mutex<Option<SyncReport>>,
}

const PHASE_OPENING: u8 = 0;
const PHASE_SYNCING: u8 = 1;
const PHASE_IDLE: u8 = 2;
const PHASE_FAILED: u8 = 3;

pub struct SearchService {
    indexer: Arc<Mutex<Option<Indexer>>>,
    shared: Arc<Shared>,
    _poll: Option<Task<()>>,
    flush_task: Option<Task<()>>,
    /// 上次 optimize 之后的增量写入次数
    writes_since_optimize: usize,
    last_optimize: Instant,
}

impl SearchService {
    /// 打开（或创建）`data_dir/<vault_id>/` 下的索引并开始同步。
    pub fn new(vault: Arc<Vault>, data_dir: PathBuf, cx: &mut Context<Self>) -> Self {
        let indexer: Arc<Mutex<Option<Indexer>>> = Arc::default();
        let shared = Arc::new(Shared::default());
        spawn_sync(vault, data_dir, indexer.clone(), shared.clone());

        // 前台轮询进度，直到进入稳定状态
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(PROGRESS_POLL).await;
                let Ok(done) = this.update(cx, |this, cx| {
                    cx.notify();
                    matches!(this.phase(), Phase::Idle | Phase::Failed(_))
                }) else {
                    return;
                };
                if done {
                    return;
                }
            }
        });
        SearchService {
            indexer,
            shared,
            _poll: Some(poll),
            flush_task: None,
            writes_since_optimize: 0,
            last_optimize: Instant::now(),
        }
    }

    /// 上次全量同步的结果。
    #[cfg(test)]
    pub fn last_report(&self) -> Option<SyncReport> {
        self.shared.report.lock().unwrap().clone()
    }

    pub fn phase(&self) -> Phase {
        match self.shared.phase.load(Ordering::Acquire) {
            PHASE_OPENING => Phase::Opening,
            PHASE_SYNCING => Phase::Syncing {
                done: self.shared.done.load(Ordering::Relaxed),
                total: self.shared.total.load(Ordering::Relaxed),
            },
            PHASE_FAILED => Phase::Failed(self.shared.error.lock().unwrap().clone().unwrap_or_default()),
            _ => Phase::Idle,
        }
    }

    /// 全文搜索。引擎还没打开时返回空。
    pub fn search(&self, text: String, limit: usize, cx: &App) -> Task<Result<Vec<SearchHit>, String>> {
        let indexer = self.indexer.clone();
        cx.background_spawn(async move {
            let guard = indexer.lock().unwrap();
            match guard.as_ref() {
                Some(ix) => ix.search(&text, limit).map_err(|e| e.to_string()),
                None => Ok(Vec::new()),
            }
        })
    }

    /// 已索引笔记的 `(路径, 标题)`（快速打开用）；引擎还没打开时返回空。
    pub fn note_titles(&self, cx: &App) -> Task<Vec<(VaultPath, String)>> {
        let indexer = self.indexer.clone();
        cx.background_spawn(async move {
            let guard = indexer.lock().unwrap();
            guard.as_ref().and_then(|ix| ix.note_titles().ok()).unwrap_or_default()
        })
    }

    /// 编辑器保存后：用刚落盘的文本增量索引。
    pub fn note_saved(&mut self, path: VaultPath, text: String, stamp: FileStamp, cx: &mut Context<Self>) {
        self.run(cx, move |ix| ix.index_text(&path, &text, stamp).map(|_| true).map_err(|e| e.to_string()));
    }

    /// 文件在磁盘上被外部改动后重读。
    pub fn note_changed(&mut self, path: VaultPath, cx: &mut Context<Self>) {
        self.run(cx, move |ix| ix.index_note(&path).map(|_| true).map_err(|e| e.to_string()));
    }

    pub fn note_removed(&mut self, path: VaultPath, cx: &mut Context<Self>) {
        self.run(cx, move |ix| ix.remove_note(&path).map(|_| true).map_err(|e| e.to_string()));
    }

    pub fn note_renamed(&mut self, old: VaultPath, new: VaultPath, cx: &mut Context<Self>) {
        self.run(cx, move |ix| ix.rename_note(&old, &new).map(|_| true).map_err(|e| e.to_string()));
    }

    /// 文件监听报告这些路径有动静：逐篇按指纹对齐（新建 / 改动 → 重索引，消失 → 删除，没变 → 跳过）。
    pub fn notes_touched(&mut self, paths: Vec<VaultPath>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        self.run(cx, move |ix| {
            let mut errors = Vec::new();
            let mut written = false;
            for path in &paths {
                match ix.refresh_note(path) {
                    Ok(Refresh::Unchanged) => {}
                    Ok(_) => written = true,
                    Err(e) => errors.push(format!("{}: {e}", path.as_str())),
                }
            }
            if errors.is_empty() { Ok(written) } else { Err(errors.join("; ")) }
        });
    }

    /// 目录级变化（重命名 / 删除文件夹、事件溢出）后与磁盘整体对齐一次：新文件索引、消失的删掉、
    /// 指纹没变的跳过。只 stat 不读内容，万篇量级也在百毫秒内。
    pub fn resync(&mut self, cx: &mut Context<Self>) {
        self.run(cx, |ix| {
            let report = ix.sync_all(|_, _| {});
            for e in &report.errors {
                log::warn!("index resync: {e}");
            }
            Ok(report.indexed + report.removed > 0)
        });
    }

    /// 退出前同步落盘。
    pub fn flush_blocking(&self) {
        if let Ok(mut g) = self.indexer.lock()
            && let Some(ix) = g.as_mut()
            && let Err(e) = ix.flush()
        {
            log::warn!("index flush: {e}");
        }
    }

    /// 上次 optimize 后累计的增量写入数（测试观察 optimize 策略用）。
    #[cfg(test)]
    pub fn writes_since_optimize(&self) -> usize {
        self.writes_since_optimize
    }

    /// 测试用：装作已经累计到阈值、且距上次 optimize 足够久。
    #[cfg(test)]
    pub fn pretend_optimize_due(&mut self) {
        self.writes_since_optimize = OPTIMIZE_AFTER_WRITES;
        self.last_optimize = Instant::now() - OPTIMIZE_MIN_INTERVAL;
    }

    /// 在后台线程上对索引做一次写操作。`op` 返回是否真的写了东西（只对齐了指纹算没写）；
    /// 写了才安排空闲 flush 并计入 optimize 策略。
    fn run(
        &mut self,
        cx: &mut Context<Self>,
        op: impl FnOnce(&mut Indexer) -> Result<bool, String> + Send + 'static,
    ) {
        let indexer = self.indexer.clone();
        let task = cx.background_spawn(async move {
            let mut guard = indexer.lock().unwrap();
            match guard.as_mut() {
                Some(ix) => op(ix),
                None => Ok(false),
            }
        });
        cx.spawn(async move |this, cx| {
            let written = match task.await {
                Ok(written) => written,
                Err(e) => {
                    log::warn!("index update: {e}");
                    true
                }
            };
            let _ = this.update(cx, |this, cx| {
                if written {
                    this.writes_since_optimize += 1;
                    this.schedule_flush(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 空闲 flush；累计写入够多且距上次够久时顺带 optimize。
    fn schedule_flush(&mut self, cx: &mut Context<Self>) {
        let indexer = self.indexer.clone();
        self.flush_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(IDLE_FLUSH).await;
            let Ok(optimize) = this.update(cx, |this, _| {
                this.writes_since_optimize >= OPTIMIZE_AFTER_WRITES
                    && this.last_optimize.elapsed() >= OPTIMIZE_MIN_INTERVAL
            }) else {
                return;
            };
            cx.background_spawn(async move {
                let Ok(mut g) = indexer.lock() else { return };
                let Some(ix) = g.as_mut() else { return };
                if let Err(e) = ix.flush() {
                    log::warn!("index flush: {e}");
                }
                if optimize {
                    let started = Instant::now();
                    match ix.optimize() {
                        Ok(()) => log::info!("index optimize: {:?}", started.elapsed()),
                        Err(e) => log::warn!("index optimize: {e}"),
                    }
                }
            })
            .await;
            let _ = this.update(cx, |this, _| {
                this.flush_task = None;
                if optimize {
                    this.writes_since_optimize = 0;
                    this.last_optimize = Instant::now();
                }
            });
        }));
    }
}

impl Drop for SearchService {
    fn drop(&mut self) {
        self.shared.cancelled.store(true, Ordering::Release);
    }
}

/// 专用线程：打开索引，然后逐篇对齐。每篇单独拿锁，保存 / 搜索可以插队。
fn spawn_sync(
    vault: Arc<Vault>,
    data_dir: PathBuf,
    indexer: Arc<Mutex<Option<Indexer>>>,
    shared: Arc<Shared>,
) {
    let shared_outer = shared.clone();
    let spawned = std::thread::Builder::new().name("aero-index-sync".into()).spawn(move || {
        let opened = Indexer::open(vault, &data_dir);
        let plan = match opened {
            Ok(ix) => {
                let plan = ix.plan_sync();
                *indexer.lock().unwrap() = Some(ix);
                plan
            }
            Err(e) => {
                log::error!("index open: {e}");
                *shared.error.lock().unwrap() = Some(e.to_string());
                shared.phase.store(PHASE_FAILED, Ordering::Release);
                return;
            }
        };
        let plan = match plan {
            Ok(p) => p,
            Err(e) => {
                log::error!("index plan: {e}");
                *shared.error.lock().unwrap() = Some(e.to_string());
                shared.phase.store(PHASE_FAILED, Ordering::Release);
                return;
            }
        };
        shared.total.store(plan.notes.len(), Ordering::Relaxed);
        shared.phase.store(PHASE_SYNCING, Ordering::Release);
        let mut report = SyncReport::default();
        for (i, (path, row)) in plan.notes.iter().enumerate() {
            if shared.cancelled.load(Ordering::Acquire) {
                return;
            }
            if let Some(ix) = indexer.lock().unwrap().as_mut() {
                ix.sync_step(path, row.as_ref(), &mut report);
            }
            shared.done.store(i + 1, Ordering::Relaxed);
        }
        if let Some(ix) = indexer.lock().unwrap().as_mut() {
            for row in &plan.removed {
                ix.remove_step(row, &mut report);
            }
            if let Err(e) = ix.flush() {
                report.errors.push(e.to_string());
            }
            // 首次建库 / 大量改动之后合并一次段文件；小改动留给空闲策略
            if report.indexed + report.removed >= OPTIMIZE_AFTER_SYNC_NOTES {
                let started = Instant::now();
                match ix.optimize() {
                    Ok(()) => log::info!("index optimize after sync: {:?}", started.elapsed()),
                    Err(e) => report.errors.push(format!("optimize: {e}")),
                }
            }
        }
        for e in &report.errors {
            log::warn!("index sync: {e}");
        }
        log::info!(
            "index sync: {} indexed, {} unchanged, {} removed, {} errors",
            report.indexed,
            report.unchanged,
            report.removed,
            report.errors.len()
        );
        *shared.report.lock().unwrap() = Some(report);
        shared.phase.store(PHASE_IDLE, Ordering::Release);
    });
    if let Err(e) = spawned {
        log::error!("cannot spawn index thread: {e}");
        *shared_outer.error.lock().unwrap() = Some(e.to_string());
        shared_outer.phase.store(PHASE_FAILED, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会把 GPUI 的 `test` 属性宏带进作用域，遮住标准 `#[test]`
    use std::sync::Arc;
    use std::time::Duration;

    use aero_core::{Vault, VaultPath};
    use gpui_kit::{AppContext as _, Entity, TestAppContext};

    use super::{IDLE_FLUSH, Phase, SearchService};

    fn idle(cx: &mut TestAppContext, service: &Entity<SearchService>) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            if cx.update(|cx| service.read(cx).phase()) == Phase::Idle {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "index never became idle");
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    /// 增量写入计数；空闲 flush 时只有"累计够多且距上次够久"才 optimize，optimize 后计数清零。
    #[gpui_kit::test]
    fn idle_flush_optimizes_only_when_due(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "one\n").unwrap();
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let service = cx.new(|cx| SearchService::new(vault.clone(), data.path().to_path_buf(), cx));
        idle(cx, &service);

        let a = VaultPath::new("a.md").unwrap();
        let (_, stamp) = vault.read(&a).unwrap();
        service.update(cx, |s, cx| s.note_saved(a.clone(), "one two\n".into(), stamp, cx));
        service.update(cx, |s, cx| s.note_saved(a.clone(), "one two three\n".into(), stamp, cx));
        // 没变的路径：对齐指纹，不算写入
        service.update(cx, |s, cx| s.notes_touched(vec![VaultPath::new("missing.md").unwrap()], cx));
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| service.read(cx).writes_since_optimize()), 2);

        // 空闲 flush：阈值没到，不 optimize，计数保留
        cx.executor().advance_clock(IDLE_FLUSH + Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| service.read(cx).writes_since_optimize()), 2);

        // 到阈值：下一次空闲 flush 顺带 optimize，计数清零
        service.update(cx, |s, cx| {
            s.pretend_optimize_due();
            s.note_saved(a.clone(), "one two three four\n".into(), stamp, cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(IDLE_FLUSH + Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| service.read(cx).writes_since_optimize()), 0);
    }
}
