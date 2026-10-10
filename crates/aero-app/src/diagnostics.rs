//! Local, bounded diagnostics. Never send arbitrary text to the persistent sink.
//! Operation ownership crosses async/thread boundaries explicitly; scope guards are synchronous only.

mod storage;
#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, RandomState};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};

static RECORDER: OnceLock<Arc<Recorder>> = OnceLock::new();
static NEXT_OPERATION: AtomicU64 = AtomicU64::new(1);
const DETAIL_SECONDS: u64 = 15 * 60;
thread_local! { static CURRENT: RefCell<Option<Operation>> = const { RefCell::new(None) }; }

#[derive(Default, Clone, Copy)]
pub struct Facts {
    pub doc_id: u64,
    pub version: u64,
    pub epoch: u64,
    pub bytes: u64,
    pub count: u64,
    pub queue_us: u64,
}

#[derive(Clone, Copy)]
pub enum Outcome {
    Ok,
    Failed,
    Cancelled,
    Stale,
    Noop,
}
impl Outcome {
    fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Stale => "stale",
            Self::Noop => "noop",
        }
    }
}

struct OperationInner {
    id: u64,
    parent: u64,
    kind: &'static str,
    facts: Facts,
    started: Instant,
    finished: AtomicBool,
    span: tracing::Span,
}

/// Last owner dropping an unfinished operation records cancellation, including dropped futures.
#[derive(Clone)]
pub struct Operation(Arc<OperationInner>);

/// Synchronous only. An owned span guard must never be held across an await.
pub struct Scope {
    previous: Option<Operation>,
    _span: tracing::span::EnteredSpan,
}
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = self.previous.take());
    }
}

impl Operation {
    pub fn new(kind: &'static str, mut facts: Facts) -> Self {
        let parent = CURRENT.with(|current| {
            let current = current.borrow();
            if let Some(op) = current.as_ref() {
                if facts.doc_id == 0 {
                    facts.doc_id = op.0.facts.doc_id;
                }
                op.0.id
            } else {
                0
            }
        });
        let id = NEXT_OPERATION.fetch_add(1, Ordering::Relaxed);
        let span = tracing::info_span!(target: "aero::diagnostics", "operation", operation_id = id);
        let operation = Self(Arc::new(OperationInner {
            id,
            parent,
            kind,
            facts,
            started: Instant::now(),
            finished: AtomicBool::new(false),
            span,
        }));
        operation.record("accepted", "pending", facts);
        operation
    }

    pub fn scope<T>(&self, work: impl FnOnce() -> T) -> T {
        let _scope = self.enter();
        work()
    }

    pub fn enter(&self) -> Scope {
        Scope {
            previous: CURRENT.with(|current| current.replace(Some(self.clone()))),
            _span: self.0.span.clone().entered(),
        }
    }

    pub fn stage(&self, phase: &'static str, facts: Facts) {
        self.record(phase, "pending", facts);
    }

    pub fn finish(&self, outcome: Outcome) {
        if !self.0.finished.swap(true, Ordering::Relaxed) {
            self.record("finished", outcome.name(), self.0.facts);
        }
    }

    fn record(&self, phase: &'static str, outcome: &'static str, facts: Facts) {
        if self.0.id == 0 {
            return;
        }
        macro_rules! record {
            ($level:ident) => {
                tracing::$level!(target: "aero::diagnostics", parent: &self.0.span,
                    event = "operation", operation_id = self.0.id, parent_id = self.0.parent,
                    kind = self.0.kind, phase, outcome,
                    duration_us = self.0.started.elapsed().as_micros() as u64,
                    doc_id = if facts.doc_id == 0 { self.0.facts.doc_id } else { facts.doc_id },
                    version = facts.version, epoch = facts.epoch, bytes = facts.bytes,
                    count = facts.count, queue_us = facts.queue_us);
            };
        }
        if phase == "finished" && outcome == "failed" {
            record!(warn);
        } else if matches!(phase, "accepted" | "finished") {
            record!(info);
        } else {
            record!(debug);
        }
    }
}

pub fn stage_current(phase: &'static str, facts: Facts) {
    CURRENT.with(|current| {
        if let Some(operation) = current.borrow().as_ref() {
            operation.stage(phase, facts);
        }
    });
}

impl Drop for OperationInner {
    fn drop(&mut self) {
        if self.id != 0 && !self.finished.load(Ordering::Relaxed) {
            tracing::info!(target: "aero::diagnostics", parent: &self.span,
                event = "operation", operation_id = self.id, parent_id = self.parent,
                kind = self.kind, phase = "finished", outcome = "cancelled",
                duration_us = self.started.elapsed().as_micros() as u64, doc_id = self.facts.doc_id);
        }
    }
}

/// The keyed fingerprint is never written. Only sequential, per-process IDs leave memory.
pub fn document_id(vault: &aero_core::Vault, path: &aero_core::VaultPath) -> u64 {
    RECORDER.get().map_or(0, |r| r.object_id(&(vault.root(), path.as_str())))
}

/// Fast paths are aggregated at most once per five seconds per category.
pub fn measure<T>(kind: &'static str, facts: Facts, work: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let result = work();
    if let Some(recorder) = RECORDER.get() {
        let duration = started.elapsed().as_micros() as u64;
        let summary = {
            let mut metrics = recorder.metrics.lock().unwrap_or_else(|p| p.into_inner());
            let metric = metrics.entry(kind).or_default();
            metric.count += 1;
            metric.total_us += duration;
            if duration >= metric.max_us {
                metric.max_us = duration;
                metric.worst = facts;
            }
            if duration >= 50_000 { Some(std::mem::take(metric)) } else { None }
        };
        if let Some(summary) = summary {
            recorder.emit_metric(kind, summary);
        }
    }
    result
}

struct Metric {
    since: Instant,
    count: u64,
    total_us: u64,
    max_us: u64,
    worst: Facts,
}
impl Default for Metric {
    fn default() -> Self {
        Self { since: Instant::now(), count: 0, total_us: 0, max_us: 0, worst: Facts::default() }
    }
}

pub fn health() -> Option<(u64, u64)> {
    RECORDER.get().map(|r| (r.dropped.load(Ordering::Relaxed), r.write_errors.load(Ordering::Relaxed)))
}

pub fn detail_remaining() -> u64 {
    RECORDER.get().map_or(0, |r| r.detail_until.load(Ordering::Relaxed).saturating_sub(r.seconds()))
}

pub fn set_detail(enabled: bool) {
    if let Some(recorder) = RECORDER.get() {
        recorder
            .detail_until
            .store(if enabled { recorder.seconds() + DETAIL_SECONDS } else { 0 }, Ordering::Relaxed);
        tracing::info!(target: "aero::diagnostics", event = "detail_mode", count = u64::from(enabled));
    }
}

pub fn log_directory() -> PathBuf {
    // Keep UI automation and all debug fixtures away from real user diagnostic history.
    if cfg!(debug_assertions) && std::env::var_os("AERO_DATA_DIR").is_some() {
        return crate::paths::app_data_dir().join("logs");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "macos") {
        home.join("Library/Logs/Aero")
    } else {
        crate::paths::app_data_dir().join("logs")
    }
}

pub fn export(destination: &Path) -> Result<(), &'static str> {
    if let Some(recorder) = RECORDER.get() {
        recorder.flush();
    }
    storage::export(&log_directory(), destination)
        .map_err(|_| "无法导出诊断记录，请选择可写入且尚不存在的文件。")
}

/// Kept until the GUI run loop returns (or explicitly closed for the CLI exit path).
pub struct Guard {
    recorder: Arc<Recorder>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Guard {
    fn drop(&mut self) {
        let stopped = self.recorder.stop(!std::thread::panicking());
        if let Some(worker) = self.worker.take() {
            // A stalled filesystem must not hold application termination indefinitely.
            if stopped || worker.is_finished() {
                let _ = worker.join();
            }
        }
    }
}

pub fn init(mode: &'static str) -> Option<Guard> {
    let (recorder, worker) = match Recorder::start(&log_directory(), storage::Limits::default(), mode) {
        Ok(started) => started,
        Err(_) => {
            // Diagnostic failures must never stop application startup.
            eprintln!("Aero: local diagnostics unavailable");
            return None;
        }
    };
    let subscriber = tracing_subscriber::registry().with(DiagnosticLayer(recorder.clone()));
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        return Some(Guard { recorder, worker: Some(worker) });
    }
    let _ = RECORDER.set(recorder.clone());
    #[cfg(unix)]
    unsafe {
        // NSApplication terminate calls exit(), so main's Rust guard is not dropped.
        libc::atexit(native_exit);
    }
    let _ = log::set_boxed_logger(Box::new(LegacyLogger));
    log::set_max_level(log::LevelFilter::Trace);
    let previous = std::panic::take_hook();
    let panic_recorder = recorder.clone();
    std::panic::set_hook(Box::new(move |info| {
        let operation_id = CURRENT
            .try_with(|current| current.try_borrow().ok().and_then(|op| op.as_ref().map(|op| op.0.id)))
            .ok()
            .flatten()
            .unwrap_or(0);
        panic_recorder.panic_report(info, operation_id);
        previous(info);
    }));
    tracing::info!(target: "aero::diagnostics", event = "session_start");
    Some(Guard { recorder, worker: Some(worker) })
}

#[cfg(unix)]
extern "C" fn native_exit() {
    if let Some(recorder) = RECORDER.get() {
        recorder.stop(true);
    }
}

pub fn start_heartbeat(cx: &mut gpui_kit::App) {
    cx.spawn(async move |cx| {
        let interval = Duration::from_secs(1);
        loop {
            let started = Instant::now();
            cx.background_executor().timer(interval).await;
            if let Some(recorder) = RECORDER.get() { recorder.flush_metrics(false); }
            let elapsed = started.elapsed();
            // A large gap may be suspend/resume; never diagnose that as UI blocking.
            if elapsed > Duration::from_millis(1500) && elapsed < Duration::from_secs(10) {
                let active = cx.update(|cx| cx.windows().into_iter().any(|window| window.update(cx, |_, window, _| window.is_window_active()).unwrap_or(false)));
                if active {
                    tracing::warn!(target: "aero::diagnostics", event = "ui_stall", duration_us = elapsed.saturating_sub(interval).as_micros() as u64);
                }
            }
        }
    }).detach();
}

struct Recorder {
    sender: mpsc::SyncSender<storage::Command>,
    important: mpsc::SyncSender<Record>,
    control: mpsc::Sender<storage::Command>,
    dropped: Arc<AtomicU64>,
    write_errors: Arc<AtomicU64>,
    started: Instant,
    detail_until: AtomicU64,
    sequence: AtomicU64,
    objects: Mutex<HashMap<u64, u64>>,
    metrics: Mutex<HashMap<&'static str, Metric>>,
    hash: RandomState,
    session_dir: PathBuf,
    panics: AtomicUsize,
    stopped: AtomicBool,
}

impl Recorder {
    fn start(
        root: &Path,
        limits: storage::Limits,
        mode: &'static str,
    ) -> std::io::Result<(Arc<Self>, std::thread::JoinHandle<()>)> {
        let (sender, receiver) = mpsc::sync_channel(limits.queue);
        let (important, urgent_receiver) = mpsc::sync_channel(64);
        let (control, control_receiver) = mpsc::channel();
        let dropped = Arc::new(AtomicU64::new(0));
        let write_errors = Arc::new(AtomicU64::new(0));
        let writer = storage::Writer::new(root, limits, mode, dropped.clone(), write_errors.clone())?;
        let recorder = Arc::new(Self::with_sender(
            sender,
            important,
            control,
            dropped,
            write_errors,
            writer.directory().to_owned(),
        ));
        let worker = std::thread::Builder::new()
            .name("aero-diagnostics".into())
            .spawn(move || writer.run(receiver, urgent_receiver, control_receiver))?;
        Ok((recorder, worker))
    }

    fn with_sender(
        sender: mpsc::SyncSender<storage::Command>,
        important: mpsc::SyncSender<Record>,
        control: mpsc::Sender<storage::Command>,
        dropped: Arc<AtomicU64>,
        write_errors: Arc<AtomicU64>,
        session_dir: PathBuf,
    ) -> Self {
        Self {
            sender,
            important,
            control,
            dropped,
            write_errors,
            started: Instant::now(),
            detail_until: AtomicU64::new(0),
            sequence: AtomicU64::new(1),
            objects: Mutex::new(HashMap::new()),
            metrics: Mutex::new(HashMap::new()),
            hash: RandomState::new(),
            session_dir,
            panics: AtomicUsize::new(0),
            stopped: AtomicBool::new(false),
        }
    }

    fn stop(&self, normal_exit: bool) -> bool {
        self.flush_metrics(true);
        if self.stopped.swap(true, Ordering::Relaxed) {
            return false;
        }
        let (tx, rx) = mpsc::channel();
        self.control.send(storage::Command::Stop(tx, normal_exit)).is_ok()
            && rx.recv_timeout(Duration::from_secs(3)).is_ok()
    }

    fn seconds(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    fn emit_metric(&self, kind: &'static str, metric: Metric) {
        self.send(Record {
            schema: 1,
            level: "INFO".into(),
            module: "aero::diagnostics".into(),
            fields: std::collections::BTreeMap::from([
                ("event".into(), "measurement".into()),
                ("kind".into(), kind.into()),
                ("count".into(), metric.count.into()),
                ("total_us".into(), metric.total_us.into()),
                ("max_us".into(), metric.max_us.into()),
                ("doc_id".into(), metric.worst.doc_id.into()),
                ("version".into(), metric.worst.version.into()),
                ("bytes".into(), metric.worst.bytes.into()),
            ]),
            ..Default::default()
        });
    }

    fn flush_metrics(&self, force: bool) {
        let summaries = {
            let mut metrics = self.metrics.lock().unwrap_or_else(|p| p.into_inner());
            metrics
                .iter_mut()
                .filter_map(|(kind, metric)| {
                    (metric.count > 0 && (force || metric.since.elapsed() >= Duration::from_secs(5)))
                        .then(|| (*kind, std::mem::take(metric)))
                })
                .collect::<Vec<_>>()
        };
        for (kind, metric) in summaries {
            self.emit_metric(kind, metric);
        }
    }

    fn object_id(&self, value: &impl Hash) -> u64 {
        let key = self.hash.hash_one(value);
        let mut objects = self.objects.lock().unwrap_or_else(|p| p.into_inner());
        let next = objects.len() as u64 + 1;
        // Bounded memory in very long sessions. Zero means identity was not retained.
        if objects.len() >= 100_000 {
            return objects.get(&key).copied().unwrap_or(0);
        }
        *objects.entry(key).or_insert(next)
    }

    fn send(&self, mut record: Record) {
        if self.stopped.load(Ordering::Relaxed) {
            return;
        }
        record.seq = self.sequence.fetch_add(1, Ordering::Relaxed);
        record.time = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        record.uptime_us = self.started.elapsed().as_micros() as u64;
        record.thread = format!("{:?}", std::thread::current().id());
        let lost = if record.level == "ERROR" || record.level == "WARN" {
            self.important.try_send(record).is_err()
        } else {
            self.sender.try_send(storage::Command::Record(record)).is_err()
        };
        if lost {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn flush(&self) {
        let (tx, rx) = mpsc::channel();
        if self.control.send(storage::Command::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(3));
        }
    }

    fn panic_report(&self, info: &std::panic::PanicHookInfo<'_>, operation_id: u64) {
        if self.panics.fetch_add(1, Ordering::Relaxed) >= 4 {
            return;
        }
        let location = info.location();
        let stack = std::backtrace::Backtrace::force_capture()
            .to_string()
            .lines()
            .filter(|line| !line.trim_start().starts_with("at "))
            .take(128)
            .collect::<Vec<_>>()
            .join("\n");
        let report = storage::PanicReport {
            schema: 1,
            time: chrono::Utc::now().to_rfc3339(),
            operation_id,
            thread: format!("{:?}", std::thread::current().id()),
            file: location.map(|l| source_name(l.file()).to_owned()),
            line: location.map(|l| l.line()),
            payload_type: if info.payload().is::<&str>() || info.payload().is::<String>() {
                "string".into()
            } else {
                "opaque".into()
            },
            backtrace: stack,
        };
        if storage::write_panic(&self.session_dir, &report).is_err() {
            self.write_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Persistent fields are deliberately closed. Unknown strings and all Debug payloads are ignored.
#[derive(Default, Debug, Serialize, Deserialize)]
struct Record {
    schema: u8,
    seq: u64,
    time: String,
    uptime_us: u64,
    thread: String,
    level: String,
    module: String,
    file: String,
    line: u32,
    #[serde(flatten)]
    fields: std::collections::BTreeMap<String, serde_json::Value>,
}

fn source_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or("unknown")
}
fn safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"_:. -".contains(&b))
}
fn numeric_field(name: &str) -> bool {
    matches!(
        name,
        "operation_id"
            | "parent_id"
            | "duration_us"
            | "doc_id"
            | "version"
            | "epoch"
            | "bytes"
            | "count"
            | "queue_us"
            | "dropped"
            | "write_errors"
            | "total_us"
            | "max_us"
    )
}
fn text_field(name: &str) -> bool {
    matches!(name, "event" | "kind" | "phase" | "outcome" | "source")
}

struct SafeVisitor<'a>(&'a mut std::collections::BTreeMap<String, serde_json::Value>);
impl Visit for SafeVisitor<'_> {
    fn record_u64(&mut self, field: &Field, value: u64) {
        if numeric_field(field.name()) {
            self.0.insert(field.name().into(), value.into());
        }
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        if value >= 0 {
            self.record_u64(field, value as u64);
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if text_field(field.name()) && safe_identifier(value) {
            self.0.insert(field.name().into(), value.into());
        }
    }
    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

struct DiagnosticLayer(Arc<Recorder>);
impl<S: Subscriber> Layer<S> for DiagnosticLayer {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let metadata = event.metadata();
        if !metadata.target().starts_with("aero") {
            return;
        }
        let detailed = self.0.detail_until.load(Ordering::Relaxed) > self.0.seconds();
        if *metadata.level() > tracing::Level::INFO && !detailed {
            return;
        }
        let mut record = Record {
            schema: 1,
            level: metadata.level().as_str().into(),
            module: metadata.module_path().unwrap_or("aero").into(),
            file: metadata.file().map(source_name).unwrap_or("unknown").into(),
            line: metadata.line().unwrap_or(0),
            ..Default::default()
        };
        if metadata.target() == "aero::diagnostics" || metadata.target() == "aero::legacy" {
            event.record(&mut SafeVisitor(&mut record.fields));
        }
        // Legacy errors inside a synchronous operation retain correlation without their text.
        if let Some((id, parent, doc)) = CURRENT
            .try_with(|current| {
                current
                    .try_borrow()
                    .ok()
                    .and_then(|op| op.as_ref().map(|op| (op.0.id, op.0.parent, op.0.facts.doc_id)))
            })
            .ok()
            .flatten()
        {
            record.fields.entry("operation_id".into()).or_insert(id.into());
            record.fields.entry("parent_id".into()).or_insert(parent.into());
            record.fields.entry("doc_id".into()).or_insert(doc.into());
        }
        if !record.fields.contains_key("event") {
            record.fields.insert("event".into(), "legacy_log".into());
        }
        self.0.send(record);
    }
}

struct LegacyLogger;
impl log::Log for LegacyLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.target().starts_with("aero")
    }
    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Explicit terminal debugging retains the old output; no raw message enters our files.
        let terminal = match std::env::var("AERO_LOG").as_deref() {
            Ok("trace") => Some(log::Level::Trace),
            Ok("debug") => Some(log::Level::Debug),
            Ok("info") => Some(log::Level::Info),
            Ok("warn") => Some(log::Level::Warn),
            Ok("error") => Some(log::Level::Error),
            _ => None,
        };
        if terminal.is_some_and(|level| record.level() <= level) {
            eprintln!("[{} {}] {}", record.level(), record.target(), record.args());
        }
        if record.level() > log::Level::Warn && detail_remaining() == 0 {
            return;
        }
        let source = record.target();
        let line = record.line().unwrap_or(0) as u64;
        match record.level() {
            log::Level::Error => {
                tracing::error!(target: "aero::legacy", event = "legacy_log", source, count = line)
            }
            log::Level::Warn => {
                tracing::warn!(target: "aero::legacy", event = "legacy_log", source, count = line)
            }
            _ => tracing::debug!(target: "aero::legacy", event = "legacy_log", source, count = line),
        }
    }
    fn flush(&self) {}
}
