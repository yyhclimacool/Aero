//! 索引服务：把 `aero_index::Indexer`（同步、阻塞）包成一个实体，UI 只跟它打交道。
//!
//! - 打开 vault 时在后台线程打开索引并与磁盘对齐（每篇一把锁，期间搜索与保存不用等整个同步）；
//! - 编辑器保存 / 重命名 / 删除后增量更新；
//! - 空闲 30 s `flush()`，退出时同步 flush（ADR 0003：保存路径上不 flush）；
//! - 进度用原子量暴露，前台定时器轮询刷新状态栏（`ui_design.md` 2.1）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aero_core::{FileStamp, Vault, VaultPath};
use aero_index::{Indexer, SearchHit, SyncReport};
use gpui_kit::*;

const IDLE_FLUSH: Duration = Duration::from_secs(30);
const PROGRESS_POLL: Duration = Duration::from_millis(150);

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
        SearchService { indexer, shared, _poll: Some(poll), flush_task: None }
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
        self.run(cx, move |ix| ix.index_text(&path, &text, stamp).map(|_| ()).map_err(|e| e.to_string()));
    }

    /// 文件在磁盘上被外部改动后重读。
    pub fn note_changed(&mut self, path: VaultPath, cx: &mut Context<Self>) {
        self.run(cx, move |ix| ix.index_note(&path).map(|_| ()).map_err(|e| e.to_string()));
    }

    pub fn note_removed(&mut self, path: VaultPath, cx: &mut Context<Self>) {
        self.run(cx, move |ix| ix.remove_note(&path).map_err(|e| e.to_string()));
    }

    pub fn note_renamed(&mut self, old: VaultPath, new: VaultPath, cx: &mut Context<Self>) {
        self.run(cx, move |ix| ix.rename_note(&old, &new).map_err(|e| e.to_string()));
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

    fn run(
        &mut self,
        cx: &mut Context<Self>,
        op: impl FnOnce(&mut Indexer) -> Result<(), String> + Send + 'static,
    ) {
        let indexer = self.indexer.clone();
        let task = cx.background_spawn(async move {
            let mut guard = indexer.lock().unwrap();
            match guard.as_mut() {
                Some(ix) => op(ix),
                None => Ok(()),
            }
        });
        cx.spawn(async move |this, cx| {
            if let Err(e) = task.await {
                log::warn!("index update: {e}");
            }
            let _ = this.update(cx, |this, cx| {
                this.schedule_flush(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn schedule_flush(&mut self, cx: &mut Context<Self>) {
        let indexer = self.indexer.clone();
        self.flush_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(IDLE_FLUSH).await;
            cx.background_spawn(async move {
                if let Ok(mut g) = indexer.lock()
                    && let Some(ix) = g.as_mut()
                    && let Err(e) = ix.flush()
                {
                    log::warn!("index flush: {e}");
                }
            })
            .await;
            let _ = this.update(cx, |this, _| this.flush_task = None);
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
