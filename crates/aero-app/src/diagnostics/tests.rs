use super::*;
use std::fs;

fn recorded(root: &Path, limits: storage::Limits, work: impl FnOnce(Arc<Recorder>)) -> PathBuf {
    let (recorder, worker) = Recorder::start(root, limits, "test").unwrap();
    let path = recorder.session_dir.clone();
    let subscriber = tracing_subscriber::registry().with(DiagnosticLayer(recorder.clone()));
    tracing::subscriber::with_default(subscriber, || work(recorder.clone()));
    drop(Guard { recorder, worker: Some(worker) });
    path
}

fn records(path: &Path) -> Vec<serde_json::Value> {
    let mut files = fs::read_dir(path)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("events-"))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    files.sort();
    files
        .into_iter()
        .flat_map(|path| {
            fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn persistent_fields_exclude_payloads_and_debug_objects() {
    let dir = tempfile::tempdir().unwrap();
    let path = recorded(dir.path(), Default::default(), |_| {
        tracing::info!(target: "aero::diagnostics", event = "test", count = 7u64,
            document = "PRIVATE NOTE", query = "SECRET QUERY", path = "/Users/private", message = "SECRET ERROR", object = ?vec!["PRIVATE DEBUG"]);
        tracing::warn!(target: "aero_index", "PRIVATE LEGACY ERROR");
    });
    let body = records(&path);
    let text = serde_json::to_string(&body).unwrap();
    for forbidden in ["PRIVATE", "SECRET", "/Users/"] {
        assert!(!text.contains(forbidden), "{text}");
    }
    assert!(body.iter().any(|row| row["count"] == 7));
    assert!(body.iter().any(|row| row["event"] == "legacy_log"));
}

#[test]
fn operations_cross_threads_and_cancel_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = recorded(dir.path(), Default::default(), |_| {
        let parent = Operation::new("open_note", Default::default());
        let child = parent.scope(|| Operation::new("read_note", Default::default()));
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let worker = std::thread::spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                child.scope(|| child.stage("running", Default::default()));
                child.finish(Outcome::Ok);
                child.finish(Outcome::Failed);
            })
        });
        worker.join().unwrap();
        parent.finish(Outcome::Ok);
        drop(Operation::new("cancelled_job", Default::default()));
    });
    let rows = records(&path);
    let root = rows.iter().find(|r| r["kind"] == "open_note").unwrap()["operation_id"].clone();
    let child = rows.iter().filter(|r| r["kind"] == "read_note").collect::<Vec<_>>();
    assert!(child.iter().all(|r| r["parent_id"] == root));
    assert_eq!(child.iter().filter(|r| r["phase"] == "finished").count(), 1);
    assert!(rows.iter().any(|r| r["kind"] == "cancelled_job" && r["outcome"] == "cancelled"));
    assert!(path.join("complete.json").is_file());
}

#[test]
fn detail_expiry_and_anonymous_identity_are_session_local() {
    let dir = tempfile::tempdir().unwrap();
    recorded(dir.path(), Default::default(), |r| {
        assert_eq!(r.object_id(&"private/path"), r.object_id(&"private/path"));
        assert_ne!(r.object_id(&"private/path"), r.object_id(&"other/path"));
        let operation = Operation::new("test_detail", Default::default());
        operation.stage("hidden_phase", Default::default());
        tracing::debug!(target: "aero::diagnostics", event = "hidden_before");
        r.detail_until.store(r.seconds() + 10, Ordering::Relaxed);
        tracing::debug!(target: "aero::diagnostics", event = "visible");
        operation.stage("visible_phase", Default::default());
        r.detail_until.store(0, Ordering::Relaxed);
        operation.finish(Outcome::Ok);
        tracing::debug!(target: "aero::diagnostics", event = "hidden_after");
        r.flush();
        let rows = records(&r.session_dir);
        assert!(rows.iter().any(|r| r["event"] == "visible"));
        assert!(rows.iter().any(|r| r["phase"] == "visible_phase"));
        assert!(!rows.iter().any(|r| r["phase"] == "hidden_phase"));
        assert!(!rows.iter().any(|r| r["event"] == "hidden_before" || r["event"] == "hidden_after"));
    });
}

#[test]
fn final_metrics_and_legacy_errors_preserve_safe_context() {
    let dir = tempfile::tempdir().unwrap();
    let path = recorded(dir.path(), Default::default(), |r| {
        r.metrics.lock().unwrap().insert(
            "edit_input",
            Metric {
                count: 3,
                total_us: 15,
                max_us: 8,
                worst: Facts { doc_id: 7, ..Default::default() },
                ..Default::default()
            },
        );
        let operation = Operation::new("save", Facts { doc_id: 7, ..Default::default() });
        operation.scope(
            || tracing::error!(target: "aero::legacy", event = "legacy_log", message = "PRIVATE ERROR"),
        );
        operation.finish(Outcome::Failed);
    });
    let rows = records(&path);
    let metric = rows.iter().find(|r| r["event"] == "measurement").unwrap();
    assert_eq!(metric["count"], 3);
    assert_eq!(metric["total_us"], 15);
    assert_eq!(metric["max_us"], 8);
    let error = rows.iter().find(|r| r["event"] == "legacy_log").unwrap();
    assert!(error["operation_id"].as_u64().unwrap() > 0);
    assert_eq!(error["doc_id"], 7);
    assert!(!serde_json::to_string(&rows).unwrap().contains("PRIVATE"));
}

#[test]
fn rotation_and_retention_preserve_live_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let limits = storage::Limits { file_bytes: 512, session_files: 2, ..Default::default() };
    let old = recorded(dir.path(), limits, |_| {
        for count in 0..100u64 {
            tracing::info!(target: "aero::diagnostics", event = "test", count);
        }
    });
    let files = fs::read_dir(&old)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("events-"))
        .count();
    assert_eq!(files, 2);
    let (active, worker) = Recorder::start(dir.path(), limits, "test").unwrap();
    storage::cleanup(dir.path(), storage::Limits { total_bytes: 0, age: Duration::ZERO, ..limits });
    assert!(!old.exists());
    assert!(active.session_dir.exists());
    drop(Guard { recorder: active, worker: Some(worker) });
}

#[test]
fn full_queue_is_nonblocking_and_counted() {
    let (sender, _receiver) = mpsc::sync_channel(1);
    let dir = tempfile::tempdir().unwrap();
    let dropped = Arc::new(AtomicU64::new(0));
    let (important, _urgent_receiver) = mpsc::sync_channel(64);
    let (control, _control_receiver) = mpsc::channel();
    let recorder = Recorder::with_sender(
        sender,
        important,
        control,
        dropped.clone(),
        Arc::new(AtomicU64::new(0)),
        dir.path().to_owned(),
    );
    // Receiver deliberately never drains: producer must return and report every lost line.
    for _ in 0..100 {
        recorder.send(Record::default());
    }
    assert_eq!(dropped.load(Ordering::Relaxed), 99);
    recorder.send(Record { level: "WARN".into(), ..Default::default() });
    assert_eq!(dropped.load(Ordering::Relaxed), 99, "warnings have reserved bounded capacity");
    assert!(_urgent_receiver.try_recv().is_ok());
}

#[test]
fn export_is_create_only_and_excludes_unmanaged_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = recorded(dir.path(), Default::default(), |_| {
        tracing::info!(target: "aero::diagnostics", event = "test", count = 3u64);
    });
    fs::write(path.join("state.toml"), "PRIVATE STATE").unwrap();
    fs::write(
        path.join("events-99999999.jsonl"),
        "{\"schema\":1,\"body\":\"PRIVATE CONTENT\"}\n{\"schema\":1",
    )
    .unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(path.join("state.toml"), path.join("events-99999998.jsonl")).unwrap();
    let output = dir.path().join("report.aerodiag.json");
    storage::export(dir.path(), &output).unwrap();
    let text = fs::read_to_string(&output).unwrap();
    assert!(!text.contains("PRIVATE"));
    let bundle: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(bundle["sessions"][0]["status"], "normal_exit");
    assert!(bundle["sessions"][0]["records"].as_array().unwrap().iter().any(|r| r["count"] == 3));
    assert!(storage::export(dir.path(), &output).is_err());
    assert_eq!(fs::read_to_string(&output).unwrap(), text);
}

#[test]
fn unwritable_root_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-directory");
    fs::write(&file, "keep").unwrap();
    assert!(Recorder::start(&file, Default::default(), "test").is_err());
    assert_eq!(fs::read_to_string(file).unwrap(), "keep");
}

// Spawn the real test executable to exercise global hooks/atexit without modifying
// the parent test process's logger, environment or panic hook.
#[test]
fn process_probe() {
    let Ok(mode) = std::env::var("AERO_DIAGNOSTIC_PROBE") else { return };
    if let Some(fd) = mode.strip_prefix("descriptor-") {
        let fd: i32 = fd.parse().unwrap();
        // SAFETY: F_GETFD takes only a descriptor and rejects an invalid one.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_eq!(flags, -1);
        return;
    }
    let guard = init("probe").expect("probe diagnostics available");
    let operation = Operation::new("probe", Default::default());
    if mode == "caught" {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            operation.scope(|| panic!("PRIVATE PANIC CONTENT"))
        }));
        assert!(result.is_err());
        operation.finish(Outcome::Failed);
        drop(guard);
    } else if mode == "uncaught" {
        operation.scope(|| panic!("PRIVATE UNCAUGHT PANIC"));
    } else if mode == "abrupt" {
        RECORDER.get().unwrap().flush();
        #[cfg(unix)]
        // SAFETY: This deliberately terminates the child process after flushing its recorder.
        unsafe {
            libc::_exit(19);
        }
    } else if mode == "native_exit" {
        operation.finish(Outcome::Ok);
        std::process::exit(0);
    }
}

#[cfg(not(target_os = "macos"))]
fn probe(data: &Path, mode: &str) -> std::process::Output {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "diagnostics::tests::process_probe", "--nocapture"])
        .env("AERO_DIAGNOSTIC_PROBE", mode)
        .env("AERO_DATA_DIR", data)
        .output()
        .unwrap()
}

#[cfg(target_os = "macos")]
fn probe(data: &Path, mode: &str) -> std::process::Output {
    use std::ffi::CString;
    use std::os::unix::{ffi::OsStrExt, process::ExitStatusExt};
    // Native zvec can open descriptors without FD_CLOEXEC. Rust Command inherits
    // those on exec, keeping another parallel test's collection lock alive.
    // Apple's spawn flag closes them atomically, without unsafe post-fork Rust work.
    let executable = CString::new(std::env::current_exe().unwrap().as_os_str().as_bytes()).unwrap();
    let args = [
        executable.clone(),
        CString::new("--exact").unwrap(),
        CString::new("diagnostics::tests::process_probe").unwrap(),
        CString::new("--nocapture").unwrap(),
    ];
    let mut env = std::env::vars_os()
        .filter(|(key, _)| key != "AERO_DIAGNOSTIC_PROBE" && key != "AERO_DATA_DIR")
        .map(|(key, value)| {
            let mut pair = key.as_bytes().to_vec();
            pair.push(b'=');
            pair.extend_from_slice(value.as_bytes());
            CString::new(pair).unwrap()
        })
        .collect::<Vec<_>>();
    env.push(CString::new(format!("AERO_DIAGNOSTIC_PROBE={mode}")).unwrap());
    let mut data_env = b"AERO_DATA_DIR=".to_vec();
    data_env.extend_from_slice(data.as_os_str().as_bytes());
    env.push(CString::new(data_env).unwrap());
    let mut argv = args.iter().map(|s| s.as_ptr().cast_mut()).collect::<Vec<_>>();
    argv.push(std::ptr::null_mut());
    let mut envp = env.iter().map(|s| s.as_ptr().cast_mut()).collect::<Vec<_>>();
    envp.push(std::ptr::null_mut());
    let null = CString::new("/dev/null").unwrap();
    let mut pid = 0;
    let mut status = 0;
    // SAFETY: Initialization is checked before using the spawn structs. All C strings and
    // sentinel-terminated pointer arrays remain live through spawn, and waitpid writes to status.
    unsafe {
        let mut attributes = std::mem::MaybeUninit::uninit();
        let mut actions = std::mem::MaybeUninit::uninit();
        assert_eq!(libc::posix_spawnattr_init(attributes.as_mut_ptr()), 0);
        assert_eq!(libc::posix_spawn_file_actions_init(actions.as_mut_ptr()), 0);
        let mut attributes = attributes.assume_init();
        let mut actions = actions.assume_init();
        assert_eq!(
            libc::posix_spawnattr_setflags(&mut attributes, libc::POSIX_SPAWN_CLOEXEC_DEFAULT as i16),
            0
        );
        for fd in 0..=2 {
            let flags = if fd == 0 { libc::O_RDONLY } else { libc::O_WRONLY };
            assert_eq!(libc::posix_spawn_file_actions_addopen(&mut actions, fd, null.as_ptr(), flags, 0), 0);
        }
        let result = libc::posix_spawn(
            &mut pid,
            executable.as_ptr(),
            &actions,
            &attributes,
            argv.as_ptr(),
            envp.as_ptr(),
        );
        libc::posix_spawn_file_actions_destroy(&mut actions);
        libc::posix_spawnattr_destroy(&mut attributes);
        assert_eq!(result, 0);
        loop {
            let result = libc::waitpid(pid, &mut status, 0);
            if result == pid {
                break;
            }
            assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EINTR));
        }
    }
    std::process::Output {
        status: std::process::ExitStatus::from_raw(status),
        stdout: Vec::new(),
        stderr: Vec::new(),
    }
}

#[cfg(target_os = "macos")]
#[test]
fn process_probes_do_not_inherit_native_descriptors() {
    use std::os::fd::AsRawFd;
    let dir = tempfile::tempdir().unwrap();
    let file = fs::File::create(dir.path().join("lock")).unwrap();
    // SAFETY: The File keeps its descriptor open and F_DUPFD takes an integer lower bound.
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 512) };
    assert!(fd >= 512);
    let result = probe(dir.path(), &format!("descriptor-{fd}"));
    // SAFETY: F_DUPFD returned a new descriptor that this test owns.
    unsafe { libc::close(fd) };
    assert!(result.status.success());
}

#[test]
fn real_process_panic_and_native_exit_preserve_evidence() {
    let dir = tempfile::tempdir().unwrap();
    assert!(probe(dir.path(), "caught").status.success());
    let logs = dir.path().join("logs");
    let session = fs::read_dir(&logs).unwrap().next().unwrap().unwrap().path();
    let reports = fs::read_dir(&session)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("panic-"))
        .collect::<Vec<_>>();
    assert_eq!(reports.len(), 1);
    let report = fs::read_to_string(reports[0].path()).unwrap();
    assert!(!report.contains("PRIVATE PANIC CONTENT"));
    assert!(!report.contains("/Users/"));
    let report: serde_json::Value = serde_json::from_str(&report).unwrap();
    assert!(report["operation_id"].as_u64().unwrap() > 0);
    assert_eq!(report["payload_type"], "string");
    assert!(session.join("complete.json").is_file(), "caught panic did not terminate the app");
    assert!(!probe(dir.path(), "uncaught").status.success());
    assert_eq!(probe(dir.path(), "abrupt").status.code(), Some(19));
    assert!(probe(dir.path(), "native_exit").status.success());
    let output = dir.path().join("report.json");
    storage::export(&logs, &output).unwrap();
    let bundle: serde_json::Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
    let sessions = bundle["sessions"].as_array().unwrap();
    assert_eq!(sessions.iter().filter(|s| s["status"] == "incomplete").count(), 2);
    assert_eq!(sessions.iter().filter(|s| s["status"] == "normal_exit").count(), 2);
    assert!(sessions.iter().any(|s| s["manifest"]["previous_incomplete"] == 2));
}

#[cfg(unix)]
#[test]
fn writer_failure_is_counted_without_crashing() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = recorded(dir.path(), storage::Limits { file_bytes: 1, ..Default::default() }, |r| {
        tracing::info!(target: "aero::diagnostics", event = "first");
        r.flush();
        fs::set_permissions(&r.session_dir, fs::Permissions::from_mode(0o500)).unwrap();
        tracing::info!(target: "aero::diagnostics", event = "rotation_fails");
        r.flush();
        assert!(r.write_errors.load(Ordering::Relaxed) > 0);
        fs::set_permissions(&r.session_dir, fs::Permissions::from_mode(0o700)).unwrap();
    });
    assert!(path.join("complete.json").exists());
}

#[test]
#[ignore = "explicit native performance measurement"]
fn writer_overhead_measurement() {
    let dir = tempfile::tempdir().unwrap();
    let mut samples = Vec::new();
    let started = Instant::now();
    recorded(dir.path(), Default::default(), |r| {
        r.detail_until.store(r.seconds() + DETAIL_SECONDS, Ordering::Relaxed);
        for _ in 0..500 {
            let now = Instant::now();
            let op = Operation::new("benchmark", Facts { bytes: 1024, ..Default::default() });
            op.stage("running", Default::default());
            op.finish(Outcome::Ok);
            samples.push(now.elapsed().as_micros());
        }
        r.flush();
        assert_eq!(r.dropped.load(Ordering::Relaxed), 0);
    });
    samples.sort();
    eprintln!(
        "500 operations / 1500 records: producer p50={} us, p95={} us; including flush={:.2} ms",
        samples[250],
        samples[475],
        started.elapsed().as_secs_f64() * 1000.0
    );
}
