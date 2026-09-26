use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use aero_core::{FtsQuery, Vault, VaultPath};
use aero_index::SearchHit;
use gpui_kit::{App, AppContext as _, Context, Task};
use serde::{Deserialize, Serialize};

use crate::semantic_policy::{INDEX_BATCH, MODEL_IDLE_RELEASE, may_index, rest_after_batch};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchPreferences {
    pub semantic_enabled: bool,
    pub allow_model_downloads: bool,
    pub indexing_paused: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelStatus {
    Missing,
    Downloading { downloaded: u64, total: u64 },
    Loading,
    Ready,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SemanticPhase {
    Disabled,
    WaitingForModel,
    Paused,
    WaitingForIdle,
    Indexing { done: usize, total: usize },
    Ready { notes: usize, chunks: usize },
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticStatus {
    pub model: ModelStatus,
    pub phase: SemanticPhase,
    pub revision: u64,
    pub scope_revision: u64,
}

impl Default for SemanticStatus {
    fn default() -> Self {
        Self { model: ModelStatus::Missing, phase: SemanticPhase::Disabled, revision: 0, scope_revision: 0 }
    }
}

type Reply = Arc<Mutex<Option<Result<Vec<SearchHit>, String>>>>;

struct Query {
    text: String,
    limit: usize,
    reply: Reply,
    cancelled: Arc<AtomicBool>,
}

struct Control {
    preferences: SearchPreferences,
    save_preferences: bool,
    download: bool,
    rescan: bool,
    changed: BTreeSet<VaultPath>,
    query: Option<Query>,
}

struct Shared {
    control: Mutex<Control>,
    status: Mutex<SemanticStatus>,
    wake: Condvar,
    stopped: AtomicBool,
    download_cancelled: AtomicBool,
    activity: AtomicU64,
}

fn now_millis() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

impl Shared {
    fn phase(&self, phase: SemanticPhase) {
        self.status.lock().unwrap().phase = phase;
    }

    fn model(&self, model: ModelStatus) {
        self.status.lock().unwrap().model = model;
    }

    fn index_interrupted(&self, path: Option<&VaultPath>) -> bool {
        let control = self.control.lock().unwrap();
        let idle = Duration::from_millis(now_millis().saturating_sub(self.activity.load(Ordering::Acquire)));
        self.stopped.load(Ordering::Acquire)
            || !control.preferences.semantic_enabled
            || control.rescan
            || path.is_some_and(|path| control.changed.contains(path))
            || !may_index(idle, control.preferences.indexing_paused, control.query.is_some())
    }

    fn wait(&self, duration: Duration) {
        let guard = self.control.lock().unwrap();
        if self.stopped.load(Ordering::Acquire)
            || guard.query.is_some()
            || guard.save_preferences
            || guard.download
        {
            return;
        }
        let _ = self.wake.wait_timeout(guard, duration).unwrap();
    }
}

pub struct SemanticService {
    preferences: SearchPreferences,
    shared: Arc<Shared>,
    thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    close_lock: Arc<Mutex<()>>,
    poll: Option<Task<()>>,
}

pub(crate) struct SemanticShutdown {
    shared: Arc<Shared>,
    thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    lock: Arc<Mutex<()>>,
}

impl Drop for SemanticShutdown {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        self.shared.download_cancelled.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        let _closing = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(thread) = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = thread.join();
        }
    }
}

impl SemanticService {
    pub fn new(vault: Arc<Vault>, data_dir: PathBuf, cx: &mut Context<Self>) -> Self {
        let preferences: SearchPreferences = std::fs::read_to_string(data_dir.join("search.toml"))
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default();
        let shared = Arc::new(Shared {
            control: Mutex::new(Control {
                preferences: preferences.clone(),
                save_preferences: false,
                download: false,
                rescan: true,
                changed: BTreeSet::new(),
                query: None,
            }),
            status: Mutex::new(SemanticStatus::default()),
            wake: Condvar::new(),
            stopped: AtomicBool::new(false),
            download_cancelled: AtomicBool::new(false),
            activity: AtomicU64::new(now_millis()),
        });
        let worker = shared.clone();
        let thread = std::thread::Builder::new()
            .name("aero-semantic".into())
            .spawn(move || worker_loop(vault, data_dir, worker));
        let thread = match thread {
            Ok(thread) => Some(thread),
            Err(error) => {
                shared.phase(SemanticPhase::Failed(error.to_string()));
                None
            }
        };
        let mut this = Self {
            preferences,
            shared,
            thread: Arc::new(Mutex::new(thread)),
            close_lock: Arc::default(),
            poll: None,
        };
        this.start_poll(cx);
        this
    }

    pub fn preferences(&self) -> &SearchPreferences {
        &self.preferences
    }

    pub fn status(&self) -> SemanticStatus {
        self.shared.status.lock().unwrap().clone()
    }

    fn start_poll(&mut self, cx: &mut Context<Self>) {
        self.poll = Some(cx.spawn(async move |this, cx| {
            let mut previous = None;
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let Ok(done) = this.update(cx, |this, cx| {
                    let status = this.status();
                    if previous.as_ref() != Some(&status) {
                        previous = Some(status.clone());
                        cx.notify();
                    }
                    this.shared.stopped.load(Ordering::Acquire)
                        || (!this.preferences.semantic_enabled
                            && !matches!(status.model, ModelStatus::Downloading { .. }))
                }) else {
                    return;
                };
                if done {
                    return;
                }
            }
        }));
    }

    pub fn set_preferences(&mut self, preferences: SearchPreferences, cx: &mut Context<Self>) {
        if self.preferences == preferences {
            return;
        }
        if !preferences.allow_model_downloads {
            self.shared.download_cancelled.store(true, Ordering::Release);
        }
        {
            let mut control = self.shared.control.lock().unwrap();
            if preferences.semantic_enabled && !control.preferences.semantic_enabled {
                control.rescan = true;
            }
            control.preferences = preferences.clone();
            control.save_preferences = true;
        }
        self.preferences = preferences;
        self.shared.wake.notify_all();
        self.start_poll(cx);
        cx.notify();
    }

    pub fn download_model(&mut self, cx: &mut Context<Self>) {
        if !self.preferences.allow_model_downloads {
            self.shared.model(ModelStatus::Failed("尚未允许下载模型；笔记与查询不会发送到网络".into()));
            cx.notify();
            return;
        }
        if matches!(self.status().model, ModelStatus::Downloading { .. }) {
            return;
        }
        self.shared.download_cancelled.store(false, Ordering::Release);
        self.shared.control.lock().unwrap().download = true;
        self.shared.model(ModelStatus::Downloading { downloaded: 0, total: aero_embedding::DOWNLOAD_BYTES });
        self.shared.wake.notify_all();
        self.start_poll(cx);
        cx.notify();
    }

    pub fn cancel_download(&mut self, cx: &mut Context<Self>) {
        self.shared.download_cancelled.store(true, Ordering::Release);
        self.shared.control.lock().unwrap().download = false;
        self.shared.wake.notify_all();
        cx.notify();
    }

    pub fn touch_activity(&self) {
        self.shared.activity.store(now_millis(), Ordering::Release);
    }

    pub fn note_changed(&mut self, paths: Vec<VaultPath>, cx: &mut Context<Self>) {
        self.shared.control.lock().unwrap().changed.extend(paths.into_iter().filter(|p| {
            p.is_note()
                && !p.parent().split('/').any(|part| part.starts_with('.') || part.ends_with(".assets"))
        }));
        self.shared.wake.notify_all();
        if self.preferences.semantic_enabled {
            self.start_poll(cx);
        }
    }

    pub fn rescan(&mut self, cx: &mut Context<Self>) {
        self.shared.control.lock().unwrap().rescan = true;
        self.shared.wake.notify_all();
        if self.preferences.semantic_enabled {
            self.start_poll(cx);
        }
    }

    pub fn query(&self, text: String, limit: usize, cx: &App) -> Task<Result<Vec<SearchHit>, String>> {
        self.touch_activity();
        if !self.preferences.semantic_enabled {
            return Task::ready(Err("语义检索未启用".into()));
        }
        if !matches!(self.status().model, ModelStatus::Ready | ModelStatus::Loading) {
            return Task::ready(Err("本地模型尚未就绪，全文结果仍可用".into()));
        }
        let reply = Arc::new(Mutex::new(None));
        let cancelled = Arc::new(AtomicBool::new(false));
        let query =
            Query { text, limit: limit.clamp(1, 200), reply: reply.clone(), cancelled: cancelled.clone() };
        if let Some(old) = self.shared.control.lock().unwrap().query.replace(query) {
            old.cancelled.store(true, Ordering::Release);
            *old.reply.lock().unwrap() = Some(Err("查询已被更新".into()));
        }
        self.shared.wake.notify_all();
        let shared = self.shared.clone();
        let executor = cx.background_executor().clone();
        let cancellation = CancelQuery(cancelled);
        cx.background_spawn(async move {
            let _cancel = cancellation;
            loop {
                if let Some(result) = reply.lock().unwrap().take() {
                    return result;
                }
                if shared.stopped.load(Ordering::Acquire) {
                    return Err("知识库已关闭".into());
                }
                // A native worker cannot wake GPUI's deterministic test executor directly.
                executor.timer(Duration::from_millis(20)).await;
            }
        })
    }

    /// Completion includes the worker, downloader and native index/model destruction.
    /// The owner must await this before dropping the service or quitting/reopening.
    pub fn close_async(&self, cx: &App) -> Task<()> {
        self.request_stop();
        let guard = self.shutdown_guard();
        cx.background_spawn(async move { drop(guard) })
    }

    fn request_stop(&self) {
        self.shared.stopped.store(true, Ordering::Release);
        self.shared.download_cancelled.store(true, Ordering::Release);
        self.shared.wake.notify_all();
    }

    pub(crate) fn shutdown_guard(&self) -> SemanticShutdown {
        SemanticShutdown {
            shared: self.shared.clone(),
            thread: self.thread.clone(),
            lock: self.close_lock.clone(),
        }
    }

    pub fn close_blocking(&self) {
        drop(self.shutdown_guard());
    }
}

impl Drop for SemanticService {
    fn drop(&mut self) {
        self.close_blocking();
    }
}

struct CancelQuery(Arc<AtomicBool>);
impl Drop for CancelQuery {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn save_preferences(dir: &Path, preferences: &SearchPreferences) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let text = toml::to_string(preferences).map_err(|error| error.to_string())?;
    let pending = dir.join(format!(".search-{}.tmp", std::process::id()));
    std::fs::write(&pending, text).map_err(|error| error.to_string())?;
    std::fs::rename(pending, dir.join("search.toml")).map_err(|error| error.to_string())
}

struct IndexJob {
    plan: aero_index::semantic::SemanticPlan,
    vectors: Vec<Vec<f32>>,
}

fn load_model(
    engine: &mut Option<aero_embedding::LocalEmbedder>,
    root: &Path,
    shared: &Shared,
) -> Result<(), String> {
    if engine.is_none() {
        shared.model(ModelStatus::Loading);
        match aero_embedding::LocalEmbedder::load(root) {
            Ok(model) => *engine = Some(model),
            Err(error) => {
                shared.model(ModelStatus::Failed(error.clone()));
                return Err(error);
            }
        }
        shared.model(ModelStatus::Ready);
    }
    Ok(())
}

fn worker_loop(vault: Arc<Vault>, data_dir: PathBuf, shared: Arc<Shared>) {
    let models = data_dir.join("models");
    let mut available = aero_embedding::model_available(&models);
    shared.model(if available { ModelStatus::Ready } else { ModelStatus::Missing });
    let mut engine = None;
    let mut index: Option<aero_index::semantic::SemanticIndex> = None;
    let mut downloader: Option<std::thread::JoinHandle<()>> = None;
    let mut pending = VecDeque::new();
    let mut queued = BTreeSet::new();
    let mut live_paths = HashSet::new();
    let mut current: Option<IndexJob> = None;
    let mut needs_rescan = true;
    let mut completed = 0;
    let mut total = 0;
    let mut last_model_use = Instant::now();
    let mut rest_until = Instant::now();
    let mut failures = BTreeMap::<VaultPath, String>::new();
    let mut writes = 0usize;
    loop {
        if shared.stopped.load(Ordering::Acquire) {
            break;
        }
        if downloader.as_ref().is_some_and(|task| task.is_finished()) {
            let _ = downloader.take().unwrap().join();
            available = aero_embedding::model_available(&models);
            needs_rescan = true;
        }
        let (preferences, save, start_download, rescan, changed, query) = {
            let mut control = shared.control.lock().unwrap();
            (
                control.preferences.clone(),
                std::mem::take(&mut control.save_preferences),
                std::mem::take(&mut control.download),
                std::mem::take(&mut control.rescan),
                std::mem::take(&mut control.changed),
                control.query.take(),
            )
        };
        if save {
            if let Err(error) = save_preferences(&data_dir, &preferences) {
                shared.phase(SemanticPhase::Failed(format!("设置保存失败：{error}")));
            }
            if downloader.is_none() {
                available = aero_embedding::model_available(&models);
                shared.model(if available { ModelStatus::Ready } else { ModelStatus::Missing });
            }
        }
        if start_download && preferences.allow_model_downloads && downloader.is_none() {
            if available {
                shared.model(ModelStatus::Ready);
            } else {
                let root = models.clone();
                let state = shared.clone();
                match std::thread::Builder::new().name("aero-model-download".into()).spawn(move || {
                    let result = aero_embedding::download_model(
                        &root,
                        &state.download_cancelled,
                        |downloaded, total| {
                            state.model(ModelStatus::Downloading { downloaded, total });
                        },
                    );
                    state.model(match result {
                        Ok(()) => ModelStatus::Ready,
                        Err(_) if state.download_cancelled.load(Ordering::Acquire) => ModelStatus::Missing,
                        Err(error) => ModelStatus::Failed(error),
                    });
                    state.wake.notify_all();
                }) {
                    Ok(task) => downloader = Some(task),
                    Err(error) => shared.model(ModelStatus::Failed(error.to_string())),
                }
            }
        }
        needs_rescan |= rescan || changed.iter().any(|path| !live_paths.contains(path));
        if rescan {
            current = None;
        }
        for path in changed {
            failures.remove(&path);
            let replaces_current = current.as_ref().is_some_and(|job| job.plan.path == path);
            if replaces_current {
                current = None;
            }
            if queued.insert(path.clone()) {
                pending.push_back(path);
                if !replaces_current {
                    total += 1;
                }
            }
        }
        if !preferences.semantic_enabled || !available {
            if let Some(query) = query {
                *query.reply.lock().unwrap() = Some(Err("本地语义模型未启用或未就绪；全文结果仍可用".into()));
            }
            engine = None;
            if !preferences.semantic_enabled {
                index = None;
                current = None;
                pending.clear();
                queued.clear();
                failures.clear();
                completed = 0;
                total = 0;
                needs_rescan = true;
                shared.phase(SemanticPhase::Disabled);
            } else {
                shared.phase(SemanticPhase::WaitingForModel);
            }
            shared.wait(Duration::from_millis(250));
            continue;
        }
        if index.is_none() {
            if query.is_none() && preferences.indexing_paused {
                shared.phase(SemanticPhase::Paused);
                shared.wait(Duration::from_millis(100));
                continue;
            }
            match aero_index::semantic::SemanticIndex::open_interruptible(
                vault.clone(),
                &data_dir.join("index"),
                aero_embedding::MODEL_ID,
                false,
                || {
                    shared.stopped.load(Ordering::Acquire)
                        || !shared.control.lock().unwrap().preferences.semantic_enabled
                        || query.as_ref().is_some_and(|q| q.cancelled.load(Ordering::Acquire))
                },
            ) {
                Ok(Some(opened)) => {
                    index = Some(opened);
                    writes = 1;
                }
                Ok(None) => {
                    if let Some(query) = query {
                        *query.reply.lock().unwrap() = Some(Err("查询已取消".into()));
                    }
                    continue;
                }
                Err(error) => {
                    if let Some(query) = query {
                        *query.reply.lock().unwrap() = Some(Err(error.clone()));
                    }
                    shared.phase(SemanticPhase::Failed(error));
                    shared.wait(Duration::from_secs(2));
                    continue;
                }
            }
        }
        if needs_rescan {
            let started = Instant::now();
            let scan = vault
                .notes()
                .map_err(|e| e.to_string())
                .and_then(|paths| Ok((paths, index.as_ref().unwrap().known_paths()?)));
            match scan {
                Ok((paths, known)) => {
                    let next_live = paths.iter().cloned().collect();
                    if live_paths != next_live {
                        shared.status.lock().unwrap().scope_revision += 1;
                    }
                    live_paths = next_live;
                    index.as_mut().unwrap().set_live_paths(live_paths.clone());
                    for path in paths.into_iter().chain(known.into_iter().filter(|p| !live_paths.contains(p)))
                    {
                        if queued.insert(path.clone()) {
                            pending.push_back(path);
                        }
                    }
                    completed = 0;
                    total = pending.len() + usize::from(current.is_some());
                    needs_rescan = false;
                    rest_until = Instant::now() + rest_after_batch(started.elapsed());
                }
                Err(error) => {
                    if let Some(query) = query {
                        *query.reply.lock().unwrap() = Some(Err(error.clone()));
                    }
                    shared.phase(SemanticPhase::Failed(error));
                    shared.wait(Duration::from_secs(2));
                    continue;
                }
            }
        }
        if let Some(query) = query {
            let cancelled = || {
                let control = shared.control.lock().unwrap();
                shared.stopped.load(Ordering::Acquire)
                    || query.cancelled.load(Ordering::Acquire)
                    || !control.preferences.semantic_enabled
                    || control.query.is_some()
                    || control.rescan
            };
            if !cancelled() {
                let mut parsed = FtsQuery::parse(&query.text);
                parsed.limit = query.limit;
                let result = if parsed.text.is_empty() || index.as_ref().unwrap().stats().1 == 0 {
                    Ok(Vec::new())
                } else {
                    load_model(&mut engine, &models, &shared).and_then(|()| {
                        if cancelled() {
                            return Err("查询已取消".into());
                        }
                        last_model_use = Instant::now();
                        let vectors = engine.as_mut().unwrap().embed(&[parsed.text.clone()], true)?;
                        if cancelled() {
                            return Err("查询已取消".into());
                        }
                        index.as_ref().unwrap().search_interruptible(&vectors[0], &parsed, cancelled)
                    })
                };
                *query.reply.lock().unwrap() = Some(result);
            } else {
                *query.reply.lock().unwrap() = Some(Err("查询已取消".into()));
            }
            continue;
        }
        let idle =
            Duration::from_millis(now_millis().saturating_sub(shared.activity.load(Ordering::Acquire)));
        let query_pending = shared.control.lock().unwrap().query.is_some();
        if !may_index(idle, preferences.indexing_paused, query_pending) || Instant::now() < rest_until {
            shared.phase(if preferences.indexing_paused {
                SemanticPhase::Paused
            } else {
                SemanticPhase::WaitingForIdle
            });
            if engine.is_some() && last_model_use.elapsed() >= MODEL_IDLE_RELEASE {
                engine = None;
            }
            shared.wait(Duration::from_millis(100));
            continue;
        }
        let started = Instant::now();
        if current.is_none()
            && let Some(path) = pending.pop_front()
        {
            queued.remove(&path);
            let before = index.as_ref().unwrap().stats();
            let cancelled = std::cell::Cell::new(false);
            let prepared = index.as_mut().unwrap().prepare_note_interruptible(&path, || {
                cancelled.set(cancelled.get() || shared.index_interrupted(Some(&path)));
                cancelled.get()
            });
            if index.as_ref().unwrap().stats() != before {
                shared.status.lock().unwrap().revision += 1;
            }
            match prepared {
                Ok(Some(plan)) => {
                    current = Some(IndexJob { vectors: Vec::with_capacity(plan.chunks.len()), plan })
                }
                Ok(None) if cancelled.get() => {
                    if queued.insert(path.clone()) {
                        pending.push_front(path);
                    }
                    continue;
                }
                Ok(None) => {
                    completed += 1;
                    failures.remove(&path);
                }
                Err(error) => {
                    completed += 1;
                    failures.insert(path, error);
                }
            }
        }
        if let Some(job) = current.as_mut() {
            shared.phase(SemanticPhase::Indexing { done: completed, total: total.max(completed + 1) });
            let start = job.vectors.len();
            let end = (start + INDEX_BATCH).min(job.plan.chunks.len());
            let texts: Vec<_> = job.plan.chunks[start..end].iter().map(|chunk| chunk.text.clone()).collect();
            if shared.index_interrupted(Some(&job.plan.path)) {
                continue;
            }
            let result = if texts.is_empty() {
                Ok(Vec::new())
            } else {
                load_model(&mut engine, &models, &shared).and_then(|()| {
                    if shared.index_interrupted(Some(&job.plan.path)) {
                        return Ok(Vec::new());
                    }
                    last_model_use = Instant::now();
                    engine.as_mut().unwrap().embed(&texts, false)
                })
            };
            match result {
                Ok(vectors) => {
                    job.vectors.extend(vectors);
                    if job.vectors.len() == job.plan.chunks.len() {
                        match index.as_mut().unwrap().commit_note_interruptible(
                            &job.plan,
                            &job.vectors,
                            || shared.index_interrupted(Some(&job.plan.path)),
                        ) {
                            Ok(true) => {
                                shared.status.lock().unwrap().revision += 1;
                                failures.remove(&job.plan.path);
                                writes += 1;
                            }
                            Ok(false) if shared.index_interrupted(Some(&job.plan.path)) => continue,
                            Ok(false) => {
                                if queued.insert(job.plan.path.clone()) {
                                    pending.push_back(job.plan.path.clone());
                                    total += 1;
                                }
                            }
                            Err(error) => {
                                failures.insert(job.plan.path.clone(), error);
                            }
                        }
                        current = None;
                        completed += 1;
                    }
                }
                Err(error) => {
                    failures.insert(job.plan.path.clone(), error.clone());
                    shared.model(ModelStatus::Failed(error));
                    available = false;
                    current = None;
                    completed += 1;
                }
            }
            rest_until = Instant::now() + rest_after_batch(started.elapsed());
        } else if pending.is_empty() {
            if writes > 0 {
                match index.as_mut().unwrap().flush_interruptible(|| shared.index_interrupted(None)) {
                    Ok(true) => writes = 0,
                    Ok(false) => continue,
                    Err(error) => {
                        shared.phase(SemanticPhase::Failed(error));
                        shared.wait(Duration::from_secs(2));
                        continue;
                    }
                }
            }
            let (notes, chunks) = index.as_ref().unwrap().stats();
            shared.phase(match failures.iter().next() {
                Some((path, error)) => SemanticPhase::Failed(format!(
                    "已索引 {notes} 篇；{} 篇暂未完成（{path}：{error}），全文仍可用",
                    failures.len()
                )),
                None => SemanticPhase::Ready { notes, chunks },
            });
            if engine.is_some() && last_model_use.elapsed() >= MODEL_IDLE_RELEASE {
                engine = None;
            }
            shared.wait(Duration::from_millis(250));
        } else {
            if writes >= 32 {
                match index.as_mut().unwrap().flush_interruptible(|| shared.index_interrupted(None)) {
                    Ok(true) => writes = 0,
                    Ok(false) => continue,
                    Err(error) => {
                        shared.phase(SemanticPhase::Failed(error));
                        shared.wait(Duration::from_secs(2));
                        continue;
                    }
                }
            }
            rest_until = Instant::now() + rest_after_batch(started.elapsed());
        }
    }
    shared.download_cancelled.store(true, Ordering::Release);
    let control = shared.control.lock().unwrap();
    if control.save_preferences
        && let Err(error) = save_preferences(&data_dir, &control.preferences)
    {
        log::error!("search settings save on close: {error}");
        shared.phase(SemanticPhase::Failed(format!("设置保存失败：{error}")));
    }
    drop(control);
    if let Some(downloader) = downloader {
        let _ = downloader.join();
    }
    // Committed notes are already durable; leave interrupted cleanup journaled for reopen.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui_kit::test]
    fn async_close_joins_worker_and_persists_pending_preferences(cx: &mut gpui_kit::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let service = cx.new(|cx| SemanticService::new(vault, data.path().into(), cx));
        service.update(cx, |s, _| {
            let mut control = s.shared.control.lock().unwrap();
            control.preferences = SearchPreferences::default();
            control.save_preferences = true;
        });
        let close = cx.update(|cx| service.read(cx).close_async(cx));
        cx.foreground_executor().block_on(close);
        cx.update(|cx| assert!(service.read(cx).thread.lock().unwrap().is_none()));
        let stored: SearchPreferences =
            toml::from_str(&std::fs::read_to_string(data.path().join("search.toml")).unwrap()).unwrap();
        assert!(!stored.allow_model_downloads);
    }

    #[test]
    fn search_privacy_defaults_disable_models_and_network() {
        let preferences: SearchPreferences = toml::from_str("").unwrap();
        assert_eq!(preferences, SearchPreferences::default());
        assert!(!preferences.semantic_enabled && !preferences.allow_model_downloads);
    }

    #[test]
    fn shutdown_persists_pending_privacy_revocation() {
        let vault_dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(vault_dir.path()).unwrap());
        save_preferences(
            data.path(),
            &SearchPreferences { allow_model_downloads: true, ..Default::default() },
        )
        .unwrap();
        let shared = Arc::new(Shared {
            control: Mutex::new(Control {
                preferences: SearchPreferences::default(),
                save_preferences: true,
                download: false,
                rescan: false,
                changed: BTreeSet::new(),
                query: None,
            }),
            status: Mutex::new(SemanticStatus::default()),
            wake: Condvar::new(),
            stopped: AtomicBool::new(true),
            download_cancelled: AtomicBool::new(true),
            activity: AtomicU64::new(0),
        });
        worker_loop(vault, data.path().to_path_buf(), shared);
        let stored: SearchPreferences =
            toml::from_str(&std::fs::read_to_string(data.path().join("search.toml")).unwrap()).unwrap();
        assert!(!stored.allow_model_downloads, "closing must not lose a pending privacy revocation");
    }

    #[test]
    fn indexing_yields_to_activity_control_and_source_changes() {
        let path = VaultPath::new("a.md").unwrap();
        let shared = Shared {
            control: Mutex::new(Control {
                preferences: SearchPreferences { semantic_enabled: true, ..Default::default() },
                save_preferences: false,
                download: false,
                rescan: false,
                changed: BTreeSet::new(),
                query: None,
            }),
            status: Mutex::new(SemanticStatus::default()),
            wake: Condvar::new(),
            stopped: AtomicBool::new(false),
            download_cancelled: AtomicBool::new(false),
            activity: AtomicU64::new(0),
        };
        assert!(!shared.index_interrupted(Some(&path)));
        shared.activity.store(now_millis(), Ordering::Release);
        assert!(shared.index_interrupted(Some(&path)));
        shared.activity.store(0, Ordering::Release);
        shared.control.lock().unwrap().preferences.indexing_paused = true;
        assert!(shared.index_interrupted(Some(&path)));
        shared.control.lock().unwrap().preferences.indexing_paused = false;
        shared.control.lock().unwrap().changed.insert(path.clone());
        assert!(shared.index_interrupted(Some(&path)));
        shared.control.lock().unwrap().changed.clear();
        shared.control.lock().unwrap().query = Some(Query {
            text: "urgent".into(),
            limit: 1,
            reply: Arc::new(Mutex::new(None)),
            cancelled: Arc::new(AtomicBool::new(false)),
        });
        assert!(shared.index_interrupted(Some(&path)));
        shared.control.lock().unwrap().query = None;
        shared.control.lock().unwrap().rescan = true;
        assert!(shared.index_interrupted(Some(&path)));
        shared.control.lock().unwrap().rescan = false;
        shared.control.lock().unwrap().preferences.semantic_enabled = false;
        assert!(shared.index_interrupted(Some(&path)));
        shared.control.lock().unwrap().preferences.semantic_enabled = true;
        shared.stopped.store(true, Ordering::Release);
        assert!(shared.index_interrupted(Some(&path)));
    }

    fn wait_for(shared: &Shared, condition: impl Fn(&SemanticStatus) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let status = shared.status.lock().unwrap().clone();
            if condition(&status) {
                return;
            }
            assert!(Instant::now() < deadline, "worker did not reach expected state: {status:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    struct WorkerTest {
        shared: Arc<Shared>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for WorkerTest {
        fn drop(&mut self) {
            self.shared.stopped.store(true, Ordering::Release);
            self.shared.wake.notify_all();
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }

    fn test_query(shared: &Shared, text: &str) -> Vec<SearchHit> {
        let reply = Arc::new(Mutex::new(None));
        shared.control.lock().unwrap().query = Some(Query {
            text: text.into(),
            limit: 20,
            reply: reply.clone(),
            cancelled: Arc::new(AtomicBool::new(false)),
        });
        shared.wake.notify_all();
        let start = Instant::now();
        loop {
            if let Some(result) = reply.lock().unwrap().take() {
                eprintln!("semantic query {:?}: {:?}", text, start.elapsed());
                return result.unwrap();
            }
            assert!(start.elapsed() < Duration::from_secs(10), "semantic query timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    #[ignore = "requires AERO_EMBEDDING_TEST_ROOT with the already downloaded pinned model"]
    fn real_worker_indexes_queries_updates_and_stops_offline() {
        let source =
            PathBuf::from(std::env::var_os("AERO_EMBEDDING_TEST_ROOT").unwrap()).canonicalize().unwrap();
        let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target").canonicalize().unwrap();
        assert!(source.starts_with(target));
        assert!(aero_embedding::model_available(&source));
        let vault_dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let data_path = data.path().canonicalize().unwrap();
        let model = aero_embedding::model_dir(&data_path.join("models"));
        std::fs::create_dir_all(&model).unwrap();
        for file in std::fs::read_dir(aero_embedding::model_dir(&source)).unwrap() {
            let file = file.unwrap();
            std::fs::hard_link(file.path(), model.join(file.file_name())).unwrap();
        }
        std::fs::write(vault_dir.path().join("empty.md"), "---\ntags: [空白]\n---\n").unwrap();
        let vault = Arc::new(Vault::open(vault_dir.path()).unwrap());
        let shared = Arc::new(Shared {
            control: Mutex::new(Control {
                preferences: SearchPreferences {
                    semantic_enabled: true,
                    indexing_paused: true,
                    ..Default::default()
                },
                save_preferences: false,
                download: false,
                rescan: true,
                changed: BTreeSet::new(),
                query: None,
            }),
            status: Mutex::new(SemanticStatus::default()),
            wake: Condvar::new(),
            stopped: AtomicBool::new(false),
            download_cancelled: AtomicBool::new(false),
            activity: AtomicU64::new(0),
        });
        let worker_shared = shared.clone();
        let worker_path = data_path.clone();
        let thread = std::thread::spawn(move || worker_loop(vault, worker_path, worker_shared));
        let worker = WorkerTest { shared: shared.clone(), thread: Some(thread) };
        wait_for(&shared, |s| s.phase == SemanticPhase::Paused);
        assert!(test_query(&shared, "喵星人").is_empty());
        shared.control.lock().unwrap().preferences.indexing_paused = false;
        shared.wake.notify_all();
        wait_for(&shared, |s| matches!(s.phase, SemanticPhase::Ready { notes: 1, chunks: 0 }));
        for (name, text) in [
            (
                "cats.md",
                "---\ntags: [动物/宠物]\n---\n\n给猫咪准备干净的饮水和猫粮，定期接种疫苗，陪它玩耍。\n",
            ),
            ("login.md", "忘记登录密码时，可以通过绑定的邮箱重置密码，恢复账户访问。\n"),
            ("travel.md", "乘坐高速铁路去旅行，提前购买车票，记得带身份证进站。\n"),
            ("backup.md", "定期把电脑中的重要文件复制到移动硬盘，避免数据意外丢失。\n"),
        ] {
            std::fs::write(vault_dir.path().join(name), text).unwrap();
        }
        shared.control.lock().unwrap().rescan = true;
        shared.wake.notify_all();
        let start = Instant::now();
        wait_for(&shared, |s| matches!(s.phase, SemanticPhase::Ready { notes: 5, chunks: 4 }));
        eprintln!("local worker four-note indexing: {:?}", start.elapsed());
        assert_eq!(test_query(&shared, "怎样照顾喵星人？")[0].path.as_str(), "cats.md");
        assert_eq!(test_query(&shared, "忘了口令如何找回账号？")[0].path.as_str(), "login.md");
        let filtered = test_query(&shared, "怎样照顾喵星人？ #动物");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].path.as_str(), "cats.md");
        assert!(test_query(&shared, "怎样照顾喵星人？ -#动物").iter().all(|h| h.path.as_str() != "cats.md"));
        shared.control.lock().unwrap().preferences.indexing_paused = true;
        shared.wake.notify_all();
        wait_for(&shared, |s| s.phase == SemanticPhase::Paused);
        let before_revision = shared.status.lock().unwrap().revision;
        std::fs::write(vault_dir.path().join("cats.md"), "猫咪需要定期体检，更新记录。\n").unwrap();
        shared.control.lock().unwrap().changed.insert(VaultPath::new("cats.md").unwrap());
        assert!(
            test_query(&shared, "喵星人").iter().all(|h| h.path.as_str() != "cats.md"),
            "stale source must not be returned"
        );
        shared.control.lock().unwrap().preferences.indexing_paused = false;
        shared.wake.notify_all();
        wait_for(&shared, |s| matches!(s.phase, SemanticPhase::Ready { notes: 5, chunks: 4 }));
        assert!(test_query(&shared, "喵星人").iter().any(|h| h.body.contains("更新记录")));
        assert!(
            shared.status.lock().unwrap().revision > before_revision,
            "same-count edits must advance the revision"
        );
        shared.control.lock().unwrap().preferences.indexing_paused = true;
        shared.wake.notify_all();
        wait_for(&shared, |s| s.phase == SemanticPhase::Paused);
        std::fs::write(vault_dir.path().join(".aero/ignore"), "cats.md\n").unwrap();
        shared.control.lock().unwrap().rescan = true;
        assert!(
            test_query(&shared, "喵星人").iter().all(|h| h.path.as_str() != "cats.md"),
            "ignore changes apply before queries even while indexing is paused"
        );
        shared.control.lock().unwrap().preferences.indexing_paused = false;
        shared.wake.notify_all();
        wait_for(&shared, |s| matches!(s.phase, SemanticPhase::Ready { notes: 4, chunks: 3 }));
        std::fs::write(vault_dir.path().join(".aero/ignore"), "").unwrap();
        shared.control.lock().unwrap().rescan = true;
        shared.wake.notify_all();
        wait_for(&shared, |s| matches!(s.phase, SemanticPhase::Ready { notes: 5, chunks: 4 }));
        std::fs::remove_file(vault_dir.path().join("cats.md")).unwrap();
        shared.control.lock().unwrap().changed.insert(VaultPath::new("cats.md").unwrap());
        shared.wake.notify_all();
        wait_for(&shared, |s| matches!(s.phase, SemanticPhase::Ready { notes: 4, chunks: 3 }));
        assert!(test_query(&shared, "喵星人").iter().all(|h| h.path.as_str() != "cats.md"));
        std::fs::write(
            vault_dir.path().join("long.md"),
            "很长的笔记会拆分成小批次，输入和查询应该优先。\n\n".repeat(400),
        )
        .unwrap();
        shared.control.lock().unwrap().changed.insert(VaultPath::new("long.md").unwrap());
        shared.wake.notify_all();
        wait_for(&shared, |s| matches!(s.phase, SemanticPhase::Indexing { .. }));
        let start = Instant::now();
        assert_eq!(test_query(&shared, "忘了口令如何找回账号？")[0].path.as_str(), "login.md");
        assert!(start.elapsed() < Duration::from_secs(2));
        shared.control.lock().unwrap().preferences.indexing_paused = true;
        shared.wake.notify_all();
        wait_for(&shared, |s| s.phase == SemanticPhase::Paused);
        let start = Instant::now();
        drop(worker);
        eprintln!("worker stopped with pending long note: {:?}", start.elapsed());
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(
            !data_path.join("search.toml").exists(),
            "test never authorized network or changed persisted settings"
        );
    }

    #[test]
    fn worker_reports_missing_model_without_downloading() {
        let vault_dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(vault_dir.path()).unwrap());
        let shared = Arc::new(Shared {
            control: Mutex::new(Control {
                preferences: SearchPreferences { semantic_enabled: true, ..Default::default() },
                save_preferences: false,
                download: false,
                rescan: true,
                changed: BTreeSet::new(),
                query: None,
            }),
            status: Mutex::new(SemanticStatus::default()),
            wake: Condvar::new(),
            stopped: AtomicBool::new(false),
            download_cancelled: AtomicBool::new(false),
            activity: AtomicU64::new(0),
        });
        let thread_shared = shared.clone();
        let path = data.path().to_path_buf();
        let thread = std::thread::spawn(move || worker_loop(vault, path, thread_shared));
        let deadline = Instant::now() + Duration::from_secs(2);
        while shared.status.lock().unwrap().phase == SemanticPhase::Disabled && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let status = shared.status.lock().unwrap().clone();
        shared.stopped.store(true, Ordering::Release);
        shared.wake.notify_all();
        thread.join().unwrap();
        assert_eq!(status.phase, SemanticPhase::WaitingForModel);
        assert_eq!(status.model, ModelStatus::Missing);
        assert!(!data.path().join("models").exists(), "enabling cannot silently download a model");
    }
}
