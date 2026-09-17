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

use aero_core::links::Resolver;
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
    /// 打开 + 全量对齐的专用线程；[`Self::close_blocking`] 会等它停下
    sync_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
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
        let sync_thread = spawn_sync(vault, data_dir, indexer.clone(), shared.clone());

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
            sync_thread: Mutex::new(sync_thread),
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

    /// 当前笔记的反向链接；磁盘读取与 SQLite 查询都在后台执行。
    pub fn backlinks(
        &self,
        path: VaultPath,
        limit: usize,
        cx: &App,
    ) -> Task<Result<Vec<aero_index::Backlink>, String>> {
        let indexer = self.indexer.clone();
        cx.background_spawn(async move {
            let guard = indexer.lock().unwrap();
            match guard.as_ref() {
                Some(ix) => ix.backlinks(&path, limit).map_err(|e| e.to_string()),
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

    /// 预览点 wikilink 用的解析表：已索引笔记（含 front matter 标题）加磁盘上还没索引到的文件。
    /// 引擎还没打开时退化为只按磁盘文件列表解析（没有标题匹配）。
    pub fn link_resolver(&self, vault: Arc<Vault>, cx: &App) -> Task<Resolver> {
        let indexer = self.indexer.clone();
        cx.background_spawn(async move {
            let mut guard = indexer.lock().unwrap();
            match guard.as_mut().map(|ix| ix.link_resolver()) {
                Some(Ok(resolver)) => resolver,
                _ => Resolver::from_notes(vault.notes().unwrap_or_default()),
            }
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

    pub fn notes_renamed(&mut self, report: aero_core::rename::RenameReport, cx: &mut Context<Self>) {
        // 启动同步可能仍持有旧文件列表；等它结束再应用重命名，否则任务会在引擎尚未打开时丢失。
        if matches!(self.phase(), Phase::Opening | Phase::Syncing { .. }) {
            let mut report = Some(report);
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(PROGRESS_POLL).await;
                    let done = this
                        .update(cx, |this, cx| {
                            if this.shared.cancelled.load(Ordering::Acquire) {
                                return true;
                            }
                            if matches!(this.phase(), Phase::Opening | Phase::Syncing { .. }) {
                                return false;
                            }
                            if matches!(this.phase(), Phase::Idle) {
                                this.notes_renamed(report.take().expect("applied once"), cx);
                            }
                            true
                        })
                        .unwrap_or(true);
                    if done {
                        return;
                    }
                }
            })
            .detach();
            return;
        }
        self.run(cx, move |ix| {
            let mut errors = Vec::new();
            if let Err(e) = ix.rename_note(&report.from, &report.to) {
                errors.push(e.to_string());
            }
            for path in report.updated.iter().filter(|p| **p != report.to) {
                if let Err(e) = ix.refresh_note(path) {
                    errors.push(e.to_string());
                }
            }
            if errors.is_empty() { Ok(true) } else { Err(errors.join("; ")) }
        });
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

    /// 退出 / 切换 vault 前：停掉同步线程，flush，并在**当前线程**上释放引擎句柄。
    /// 之后还在飞的后台任务拿到锁只会看到 `None`，什么都不做。
    ///
    /// 必须同步做：引擎句柄若留给最后一个持有 `Arc` 的后台任务顺手释放，可能正好撞上 `exit()`
    /// 在跑 C++ 静态析构（销毁引擎的全局线程池），2026-09-16 的三份崩溃报告都是这一条
    /// （`zvec_collection_close` 在 GCD 线程上 vs 主线程 `__cxa_finalize_ranges`）。
    /// 同理，切换到同一个 vault 时新句柄必须在旧句柄关掉之后再开，否则锁不上。
    pub fn close_blocking(&self) {
        self.shared.cancelled.store(true, Ordering::Release);
        // 等同步线程停下（它每篇检查一次 cancelled；还在打开索引时会在存进锁之前看到 cancelled）
        if let Some(handle) = self.sync_thread.lock().unwrap_or_else(|p| p.into_inner()).take()
            && let Err(e) = handle.join()
        {
            log::error!("index sync thread panicked: {e:?}");
        }
        let mut guard = self.indexer.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(ix) = guard.as_mut() {
            if let Err(e) = ix.flush() {
                log::warn!("index flush: {e}");
            }
            log::debug!("index closed on {:?}", std::thread::current().name());
        }
        // 引擎句柄在这里关掉
        *guard = None;
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
    /// 实体在主线程上释放；引擎句柄也跟着在这里关，不留给后台任务。
    fn drop(&mut self) {
        self.close_blocking();
    }
}

/// 专用线程：打开索引，然后逐篇对齐。每篇单独拿锁，保存 / 搜索可以插队。
/// 返回线程句柄；起不来时置 `Failed` 并返回 `None`。
fn spawn_sync(
    vault: Arc<Vault>,
    data_dir: PathBuf,
    indexer: Arc<Mutex<Option<Indexer>>>,
    shared: Arc<Shared>,
) -> Option<std::thread::JoinHandle<()>> {
    let shared_outer = shared.clone();
    let spawned = std::thread::Builder::new().name("aero-index-sync".into()).spawn(move || {
        let opened = Indexer::open(vault, &data_dir);
        let plan = match opened {
            Ok(mut ix) => {
                let plan = ix.plan_sync();
                let mut slot = indexer.lock().unwrap();
                if shared.cancelled.load(Ordering::Acquire) {
                    // 服务已经关了：句柄就地释放（关闭方正 join 着本线程，不会同时在 exit）
                    drop(slot);
                    drop(ix);
                    return;
                }
                *slot = Some(ix);
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
        if shared.cancelled.load(Ordering::Acquire) {
            return;
        }
        if let Some(ix) = indexer.lock().unwrap().as_mut() {
            for row in &plan.removed {
                ix.remove_step(row, &mut report);
            }
            if let Err(e) = ix.finish_sync() {
                report.errors.push(e.to_string());
            }
            if let Err(e) = ix.flush() {
                report.errors.push(e.to_string());
            }
            // 首次建库 / 大量改动之后合并一次段文件；小改动留给空闲策略。关闭方在等锁时不做（可能要几秒）
            if report.indexed + report.removed >= OPTIMIZE_AFTER_SYNC_NOTES
                && !shared.cancelled.load(Ordering::Acquire)
            {
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
    match spawned {
        Ok(handle) => Some(handle),
        Err(e) => {
            log::error!("cannot spawn index thread: {e}");
            *shared_outer.error.lock().unwrap() = Some(e.to_string());
            shared_outer.phase.store(PHASE_FAILED, Ordering::Release);
            None
        }
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
            let phase = cx.update(|cx| service.read(cx).phase());
            if phase == Phase::Idle {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "index never became idle: {phase:?}");
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    /// 启动同步的旧文件列表处理完后，重命名更新仍必须执行一次。
    #[gpui_kit::test]
    fn rename_index_update_waits_for_initial_sync(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "targetunique\n").unwrap();
        std::fs::write(dir.path().join("b.md"), "[[a]]\n").unwrap();
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let service = cx.new(|cx| SearchService::new(vault.clone(), data.path().into(), cx));
        idle(cx, &service);
        let from = VaultPath::new("a.md").unwrap();
        let to = VaultPath::new("new.md").unwrap();
        let report = aero_core::rename::RenamePlan::prepare(&vault, from.clone(), to.clone())
            .unwrap()
            .execute()
            .unwrap();
        // 固定在“同步未完成”这个边界，避免依赖真线程跑得快慢。
        service.update(cx, |s, cx| {
            s.shared.phase.store(super::PHASE_SYNCING, std::sync::atomic::Ordering::Release);
            s.notes_renamed(report, cx);
        });
        cx.run_until_parked();
        let task = cx.update(|cx| service.read(cx).search("targetunique".into(), 10, cx));
        assert_eq!(cx.foreground_executor().block_on(task).unwrap()[0].path, from);
        service
            .update(cx, |s, _| s.shared.phase.store(super::PHASE_IDLE, std::sync::atomic::Ordering::Release));
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        let task = cx.update(|cx| service.read(cx).search("targetunique".into(), 10, cx));
        assert_eq!(cx.foreground_executor().block_on(task).unwrap()[0].path, to);
        let task = cx.update(|cx| service.read(cx).backlinks(to, 10, cx));
        let hits = cx.foreground_executor().block_on(task).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path.as_str(), "b.md");
        cx.update(|cx| service.read(cx).close_blocking());
    }

    /// 关掉之后引擎句柄已释放：同一份索引能立刻再打开（锁拿得到，不是删掉重建）；
    /// 旧服务上还在飞的操作看到 `None`，静默返回。
    #[gpui_kit::test]
    fn close_blocking_releases_the_engine_for_reopen(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "hello world\n").unwrap();
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let first = cx.new(|cx| SearchService::new(vault.clone(), data.path().to_path_buf(), cx));
        idle(cx, &first);
        let hits = cx.update(|cx| first.read(cx).search("hello".into(), 10, cx));
        assert_eq!(cx.foreground_executor().block_on(hits).unwrap().len(), 1);

        cx.update(|cx| first.read(cx).close_blocking());
        let hits = cx.update(|cx| first.read(cx).search("hello".into(), 10, cx));
        assert!(
            cx.foreground_executor().block_on(hits).unwrap().is_empty(),
            "closed service answers nothing"
        );
        // 关掉后的写操作也不能 panic
        let a = VaultPath::new("a.md").unwrap();
        let (_, stamp) = vault.read(&a).unwrap();
        first.update(cx, |s, cx| s.note_saved(a.clone(), "changed\n".into(), stamp, cx));
        cx.run_until_parked();

        // 旧句柄还没 drop（实体仍活着），第二个服务照样能打开同一目录并沿用已有索引
        let second = cx.new(|cx| SearchService::new(vault.clone(), data.path().to_path_buf(), cx));
        idle(cx, &second);
        let report = cx.update(|cx| second.read(cx).last_report()).unwrap();
        assert_eq!((report.indexed, report.unchanged), (0, 1), "index reused, not rebuilt: {report:?}");
        let hits = cx.update(|cx| second.read(cx).search("hello".into(), 10, cx));
        assert_eq!(cx.foreground_executor().block_on(hits).unwrap().len(), 1);
        // 临时目录先于实体销毁；显式关掉，别让引擎在被删掉的目录上收尾
        cx.update(|cx| second.read(cx).close_blocking());
    }

    /// 全量对齐还没跑完就丢掉服务：同步线程被等停、句柄就地释放，接着再开同一份索引不会撞锁。
    #[gpui_kit::test]
    fn dropping_mid_sync_stops_the_thread_and_frees_the_lock(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..400 {
            std::fs::write(dir.path().join(format!("n{i}.md")), format!("# note {i}\n\nbody {i}\n")).unwrap();
        }
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        for _ in 0..3 {
            // 还在打开 / 对齐途中就关：返回时同步线程已经 join、句柄已释放
            let service = cx.new(|cx| SearchService::new(vault.clone(), data.path().to_path_buf(), cx));
            cx.update(|cx| {
                let s = service.read(cx);
                s.close_blocking();
                assert!(s.sync_thread.lock().unwrap().is_none(), "sync thread joined");
                assert!(s.indexer.lock().unwrap().is_none(), "engine handle released");
            });
        }
        // 不等 Idle 直接丢实体：Drop 走同一条路（实体在下一次 update 冲刷效果时释放）
        let service = cx.new(|cx| SearchService::new(vault.clone(), data.path().to_path_buf(), cx));
        drop(service);
        cx.update(|_| {});
        let service = cx.new(|cx| SearchService::new(vault.clone(), data.path().to_path_buf(), cx));
        idle(cx, &service);
        let report = cx.update(|cx| service.read(cx).last_report()).unwrap();
        assert_eq!(report.indexed + report.unchanged, 400, "{report:?}");
        assert!(report.errors.is_empty(), "{report:?}");
        let hits = cx.update(|cx| service.read(cx).search("body".into(), 10, cx));
        assert_eq!(cx.foreground_executor().block_on(hits).unwrap().len(), 10);
        cx.update(|cx| service.read(cx).close_blocking());
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
