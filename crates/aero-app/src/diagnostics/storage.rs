use super::*;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub queue: usize,
    pub file_bytes: u64,
    pub session_files: usize,
    pub total_bytes: u64,
    pub age: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            queue: 2048,
            file_bytes: 1024 * 1024,
            session_files: 5,
            total_bytes: 50 * 1024 * 1024,
            age: Duration::from_secs(7 * 86400),
        }
    }
}

pub(super) enum Command {
    Record(Record),
    Flush(mpsc::Sender<()>),
    Stop(mpsc::Sender<()>, bool),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    schema: u8,
    session: String,
    started: String,
    pid: u32,
    mode: String,
    version: String,
    commit: String,
    build_profile: String,
    target: String,
    rust: String,
    os: String,
    arch: String,
    kernel: String,
    binary_uuid: Option<String>,
    native_uuid: Option<String>,
    previous_incomplete: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PanicReport {
    pub schema: u8,
    pub time: String,
    pub operation_id: u64,
    pub thread: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub payload_type: String,
    pub backtrace: String,
}

pub(super) struct Writer {
    root: PathBuf,
    dir: PathBuf,
    _lease: File,
    file: BufWriter<File>,
    index: u64,
    bytes: u64,
    limits: Limits,
    dropped: Arc<AtomicU64>,
    errors: Arc<AtomicU64>,
}

fn private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::other("symlink directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn create_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).read(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

fn read_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    Ok(file)
}

fn lock(file: &File) -> bool {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // Each worker holds a separate descriptor for its entire session lifetime.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        false
    }
}

fn timestamp_ns() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
}

fn session_name(name: &str) -> bool {
    name.strip_prefix("session-")
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
}

fn sessions(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut directories = fs::read_dir(root)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            (entry.file_type().ok()?.is_dir() && session_name(entry.file_name().to_str()?))
                .then(|| entry.path())
        })
        .collect::<Vec<_>>();
    directories.sort();
    Ok(directories)
}

fn ended_lease(dir: &Path) -> Option<File> {
    let file = read_file(&dir.join("lease")).ok()?;
    lock(&file).then_some(file)
}

fn incomplete(root: &Path) -> usize {
    sessions(root)
        .unwrap_or_default()
        .iter()
        .filter(|dir| !dir.join("complete.json").exists() && ended_lease(dir).is_some())
        .count()
}

fn directory_bytes(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.metadata().ok())
        .map(|m| m.len())
        .sum()
}

pub(super) fn cleanup(root: &Path, limits: Limits) {
    let dirs = sessions(root).unwrap_or_default();
    let mut total = dirs.iter().map(|dir| directory_bytes(dir)).sum::<u64>();
    for dir in dirs {
        let Some(_lease) = ended_lease(&dir) else { continue };
        let old = fs::metadata(dir.join("manifest.json"))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|at| SystemTime::now().duration_since(at).ok())
            .is_some_and(|age| age >= limits.age);
        if old || total > limits.total_bytes {
            let size = directory_bytes(&dir);
            if fs::remove_dir_all(dir).is_ok() {
                total = total.saturating_sub(size);
            }
        }
    }
}

impl Writer {
    pub fn new(
        root: &Path,
        limits: Limits,
        mode: &'static str,
        dropped: Arc<AtomicU64>,
        errors: Arc<AtomicU64>,
    ) -> io::Result<Self> {
        private_dir(root)?;
        let previous_incomplete = incomplete(root);
        cleanup(root, limits);
        let session = format!(
            "session-{:032x}-{:x}-{:x}",
            timestamp_ns(),
            std::process::id(),
            RandomState::new().hash_one(timestamp_ns())
        );
        let dir = root.join(&session);
        fs::create_dir(&dir)?;
        private_dir(&dir)?;
        let lease = create_file(&dir.join("lease"))?;
        if !lock(&lease) {
            return Err(io::Error::other("session lease unavailable"));
        }
        let executable = std::env::current_exe().ok();
        let native = executable
            .as_ref()
            .and_then(|exe| exe.parent())
            .map(|dir| dir.join("../Frameworks/libzvec_c_api.dylib"))
            .filter(|path| path.is_file())
            .or_else(|| option_env!("ZVEC_LIB_DIR").map(|dir| Path::new(dir).join("libzvec_c_api.dylib")));
        let manifest = Manifest {
            schema: 1,
            session,
            started: chrono::Utc::now().to_rfc3339(),
            pid: std::process::id(),
            mode: mode.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            commit: env!("AERO_BUILD_COMMIT").into(),
            build_profile: env!("AERO_BUILD_PROFILE").into(),
            target: env!("AERO_BUILD_TARGET").into(),
            rust: env!("AERO_BUILD_RUST").into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            kernel: kernel_version(),
            binary_uuid: executable.as_deref().and_then(macho_uuid),
            native_uuid: native.as_deref().and_then(macho_uuid),
            previous_incomplete,
        };
        let mut metadata = create_file(&dir.join("manifest.json"))?;
        serde_json::to_writer_pretty(&mut metadata, &manifest)?;
        metadata.sync_all()?;
        let file = BufWriter::new(create_file(&dir.join("events-00000000.jsonl"))?);
        Ok(Self {
            root: root.to_owned(),
            dir,
            _lease: lease,
            file,
            index: 0,
            bytes: 0,
            limits,
            dropped,
            errors,
        })
    }

    pub fn directory(&self) -> &Path {
        &self.dir
    }

    fn write(&mut self, record: &Record) -> io::Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        if self.bytes > 0 && self.bytes + line.len() as u64 > self.limits.file_bytes {
            self.file.flush()?;
            let next_index = self.index + 1;
            let next = create_file(&self.dir.join(format!("events-{next_index:08}.jsonl")))?;
            self.index = next_index;
            self.file = BufWriter::new(next);
            self.bytes = 0;
            if self.index >= self.limits.session_files as u64 {
                let old = self.index - self.limits.session_files as u64;
                let _ = fs::remove_file(self.dir.join(format!("events-{old:08}.jsonl")));
            }
        }
        self.file.write_all(&line)?;
        self.bytes += line.len() as u64;
        Ok(())
    }

    fn health(&mut self, final_record: bool) {
        let record = Record {
            schema: 1,
            time: chrono::Utc::now().to_rfc3339(),
            level: "INFO".into(),
            module: "aero::diagnostics".into(),
            fields: std::collections::BTreeMap::from([
                ("event".into(), if final_record { "session_end" } else { "writer_health" }.into()),
                ("dropped".into(), self.dropped.load(Ordering::Relaxed).into()),
                ("write_errors".into(), self.errors.load(Ordering::Relaxed).into()),
            ]),
            ..Default::default()
        };
        if self.write(&record).is_err() {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn run(
        mut self,
        receiver: Receiver<Command>,
        important: Receiver<Record>,
        control: Receiver<Command>,
    ) {
        let mut maintenance = Instant::now();
        let mut last_flush = Instant::now();
        loop {
            let command = if let Ok(command) = control.try_recv() {
                // Flush/stop bypass a full event queue, then drain its bounded snapshot.
                for event in receiver.try_iter().take(self.limits.queue) {
                    if let Command::Record(record) = event {
                        if self.write(&record).is_err() {
                            self.errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                for record in important.try_iter().take(64) {
                    if self.write(&record).is_err() {
                        self.errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok(command)
            } else if let Ok(record) = important.try_recv() {
                Ok(Command::Record(record))
            } else {
                receiver.recv_timeout(Duration::from_millis(250))
            };
            match command {
                Ok(Command::Record(record)) => {
                    if self.write(&record).is_err() {
                        self.errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok(Command::Flush(done)) => {
                    self.health(false);
                    if self.file.flush().is_err() {
                        self.errors.fetch_add(1, Ordering::Relaxed);
                    }
                    let _ = done.send(());
                }
                Ok(Command::Stop(done, normal_exit)) => {
                    self.health(true);
                    let flushed = self.file.flush().and_then(|_| self.file.get_ref().sync_all()).is_ok();
                    if flushed && normal_exit {
                        if let Ok(mut file) = create_file(&self.dir.join("complete.json")) {
                            let body = serde_json::json!({"schema": 1, "time": chrono::Utc::now().to_rfc3339(),
                                "dropped": self.dropped.load(Ordering::Relaxed), "write_errors": self.errors.load(Ordering::Relaxed)});
                            let _ = serde_json::to_writer(&mut file, &body);
                            let _ = file.sync_all();
                        }
                    }
                    let _ = done.send(());
                    break;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = self.file.flush();
                    break;
                }
            }
            // Flush even with a continuously nonempty queue; idle-only flush would lose too much.
            if last_flush.elapsed() >= Duration::from_millis(250) {
                if self.file.flush().is_err() {
                    self.errors.fetch_add(1, Ordering::Relaxed);
                }
                last_flush = Instant::now();
            }
            if maintenance.elapsed() >= Duration::from_secs(30) {
                self.health(false);
                cleanup(&self.root, self.limits);
                maintenance = Instant::now();
            }
        }
    }
}

pub(super) fn write_panic(dir: &Path, report: &PanicReport) -> io::Result<()> {
    let mut file = create_file(&dir.join(format!("panic-{:032x}.json", timestamp_ns())))?;
    serde_json::to_writer(&mut file, report)?;
    file.sync_all()
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path, limit: u64) -> io::Result<T> {
    let file = read_file(path)?;
    if file.metadata()?.len() > limit {
        return Err(io::Error::other("oversized diagnostic"));
    }
    serde_json::from_reader(file.take(limit)).map_err(io::Error::other)
}

/// A single inspectable JSON package; allowlist managed names, reject links, omit partial final lines.
/// Tempfile + persist_noclobber avoids partial output and never replaces a user file.
pub(super) fn export(root: &Path, destination: &Path) -> io::Result<()> {
    let parent = destination.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary.as_file().set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let mut output = BufWriter::new(temporary.as_file_mut());
    output.write_all(b"{\"schema\":1,\"sessions\":[")?;
    let mut first = true;
    let mut budget = Limits::default().total_bytes;
    let mut truncated = false;
    let dirs = match sessions(root) {
        Ok(dirs) => dirs,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    for dir in dirs.into_iter().rev() {
        // Keep ended sessions leased throughout export so another process cannot prune them.
        let lease = ended_lease(&dir);
        let Ok(manifest) = read_json::<Manifest>(&dir.join("manifest.json"), 16 * 1024) else { continue };
        let metadata = serde_json::to_vec(&manifest)?;
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        let entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
        if metadata.len() as u64 > budget {
            truncated = true;
            continue;
        }
        budget -= metadata.len() as u64;
        let status = if dir.join("complete.json").is_file() {
            "normal_exit"
        } else if lease.is_some() {
            "incomplete"
        } else {
            "active"
        };
        if !first {
            output.write_all(b",")?;
        }
        first = false;
        output.write_all(b"{\"manifest\":")?;
        output.write_all(&metadata)?;
        output.write_all(b",\"status\":")?;
        serde_json::to_writer(&mut output, &status)?;
        output.write_all(b",\"records\":[")?;
        let mut files = entries
            .iter()
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_owned();
                let number = name.strip_prefix("events-")?.strip_suffix(".jsonl")?;
                (number.len() == 8 && number.bytes().all(|b| b.is_ascii_digit())).then(|| entry.path())
            })
            .collect::<Vec<_>>();
        files.sort();
        let mut first_record = true;
        for path in files {
            let Ok(file) = read_file(&path) else { continue };
            let length = file.metadata()?.len();
            if length > budget {
                truncated = true;
                continue;
            }
            budget -= length;
            // Read a bounded snapshot of an active file; rotation/appends cannot grow this read.
            let mut reader = BufReader::new(file.take(length));
            let mut line = Vec::new();
            while reader.read_until(b'\n', &mut line)? > 0 {
                if line.len() <= 16 * 1024 && line.last() == Some(&b'\n') {
                    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line) {
                        if valid_record(&value) {
                            if !first_record {
                                output.write_all(b",")?;
                            }
                            first_record = false;
                            serde_json::to_writer(&mut output, &value)?;
                        }
                    }
                }
                line.clear();
            }
        }
        output.write_all(b"],\"panics\":[")?;
        let mut first_panic = true;
        for entry in &entries {
            let name = entry.file_name();
            let Some(number) = name
                .to_str()
                .and_then(|name| name.strip_prefix("panic-"))
                .and_then(|name| name.strip_suffix(".json"))
            else {
                continue;
            };
            if number.len() != 32 || !number.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let Ok(report) = read_json::<PanicReport>(&entry.path(), 128 * 1024) else { continue };
            let report = serde_json::to_vec(&report)?;
            if report.len() as u64 > budget {
                truncated = true;
                continue;
            }
            budget -= report.len() as u64;
            if !first_panic {
                output.write_all(b",")?;
            }
            first_panic = false;
            output.write_all(&report)?;
        }
        output.write_all(b"]}")?;
    }
    output.write_all(b"],\"truncated\":")?;
    serde_json::to_writer(&mut output, &truncated)?;
    output.write_all(b"}")?;
    output.flush()?;
    drop(output);
    temporary.as_file().sync_all()?;
    temporary.persist_noclobber(destination).map_err(|error| error.error)?;
    Ok(())
}

fn valid_record(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else { return false };
    object.get("schema").and_then(|v| v.as_u64()) == Some(1)
        && object.iter().all(|(key, value)| {
            if numeric_field(key) || matches!(key.as_str(), "schema" | "seq" | "uptime_us" | "line") {
                value.as_u64().is_some()
            } else if text_field(key) || matches!(key.as_str(), "level" | "module") {
                value.as_str().is_some_and(safe_identifier)
            } else if matches!(key.as_str(), "time" | "thread" | "file") {
                value.as_str().is_some_and(|v| v.len() < 128 && !v.contains(['/', '\\', '\n']))
            } else {
                false
            }
        })
}

fn kernel_version() -> String {
    #[cfg(unix)]
    {
        let mut name = std::mem::MaybeUninit::<libc::utsname>::uninit();
        if unsafe { libc::uname(name.as_mut_ptr()) } == 0 {
            let name = unsafe { name.assume_init() };
            return unsafe { std::ffi::CStr::from_ptr(name.release.as_ptr()) }.to_string_lossy().into_owned();
        }
    }
    "unknown".into()
}

fn macho_uuid(path: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    read_file(path).ok()?.take(64 * 1024).read_to_end(&mut bytes).ok()?;
    let number = |offset| Some(u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?));
    let mut offset = match number(0)? {
        0xfeedfacf => 32,
        0xfeedface => 28,
        _ => return None,
    };
    for _ in 0..number(16)?.min(4096) {
        let command = number(offset)?;
        let length = number(offset + 4)? as usize;
        if length < 8 {
            return None;
        }
        if command == 0x1b && length >= 24 {
            let uuid = bytes.get(offset + 8..offset + 24)?;
            let hex = uuid.iter().map(|b| format!("{b:02X}")).collect::<String>();
            return Some(format!(
                "{}-{}-{}-{}-{}",
                &hex[..8],
                &hex[8..12],
                &hex[12..16],
                &hex[16..20],
                &hex[20..]
            ));
        }
        offset = offset.checked_add(length)?;
    }
    None
}
