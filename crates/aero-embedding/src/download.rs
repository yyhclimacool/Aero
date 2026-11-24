use crate::{DOWNLOAD_BYTES, MODEL_ID};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use ureq::unversioned::{
    resolver::DefaultResolver,
    transport::{Buffers, ConnectionDetails, Connector, DefaultConnector, NextTimeout, Transport},
};

const VERSION_DIR: &str = "multilingual-e5-small-761b726dd34fb83930e26aab4e9ac3899aa1fa78-int8";
const CHUNK: usize = 64 * 1024;
const TOTAL_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const IO_TIMEOUT: Duration = Duration::from_secs(3);
// E5's upstream MIT license, fetched verbatim from microsoft/unilm/LICENSE.
// https://raw.githubusercontent.com/microsoft/unilm/master/LICENSE
// HF intfloat/multilingual-e5-small metadata declares license: mit.
const LICENSE: &str = include_str!("../MODEL-LICENSE");
static DOWNLOADING: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy)]
struct Asset {
    name: &'static str,
    size: u64,
    sha256: &'static str,
    url: &'static str,
}
// Sizes and SHA-256 from the pinned HF /api/models/.../revision/...?blobs=true API.
const ASSETS: [Asset; 2] = [
    Asset {
        name: "model_int8.onnx",
        size: 118_054_593,
        sha256: "4d24e2bc01a447951524466ef533e52944bf48509e6552810bcee1a2711cb02c",
        url: "https://huggingface.co/Xenova/multilingual-e5-small/resolve/761b726dd34fb83930e26aab4e9ac3899aa1fa78/onnx/model_int8.onnx",
    },
    Asset {
        name: "tokenizer.json",
        size: 17_082_730,
        sha256: "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
        url: "https://huggingface.co/Xenova/multilingual-e5-small/resolve/761b726dd34fb83930e26aab4e9ac3899aa1fa78/tokenizer.json",
    },
];

/// The sole versioned directory this crate installs. The cache root must be absolute.
pub fn model_dir(root: &Path) -> PathBuf {
    root.join(VERSION_DIR)
}

/// Read-only, bounded manifest/license and file-size check, NOT a large-file hash.
/// Load rechecks SHA-256 once on the worker before constructing the session.
pub fn model_available(root: &Path) -> bool {
    installed(root, &ASSETS)
}

/// Explicit, synchronous installation; call only from a background worker.
/// No credentials, cookies, proxy discovery, user text or caller-supplied URLs are used.
/// Existing damaged installs are left untouched and reported, never overwritten.
/// Cancellation is checked between reads/redirects and before atomic publication;
/// socket stalls time out after 3s, DNS/connect/header phases have short timeouts.
/// Progress is bytes received, at most once per 64 KiB (plus file boundaries).
///
/// # Errors
///
/// Returns an error for cancellation, a concurrent download, failed asset verification,
/// or failures fetching or publishing the pinned files.
pub fn download_model(root: &Path, cancel: &AtomicBool, progress: impl Fn(u64, u64)) -> Result<(), String> {
    let _guard = DOWNLOADING.try_lock().map_err(|_| "model download already in progress")?;
    check_cancel(cancel)?;
    if model_available(root) {
        progress(DOWNLOAD_BYTES, DOWNLOAD_BYTES);
        return Ok(());
    }
    let deadline = Instant::now() + TOTAL_TIMEOUT;
    let agent = http_agent(true);
    let completed = Cell::new(0);
    progress(0, DOWNLOAD_BYTES);
    install_with(root, cancel, &ASSETS, |asset, file| {
        let mut response = fetch(&agent, asset, cancel, deadline)?;
        copy_verified(response.body_mut().as_reader(), file, asset, cancel, deadline, |n| {
            progress(completed.get() + n, DOWNLOAD_BYTES);
        })?;
        completed.set(completed.get() + asset.size);
        Ok(())
    })?;
    progress(DOWNLOAD_BYTES, DOWNLOAD_BYTES);
    Ok(())
}

fn allowed_url(url: &url::Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && matches!(
            url.host_str(),
            Some(
                "huggingface.co"
                    | "cdn-lfs.huggingface.co"
                    | "cdn-lfs.hf.co"
                    | "cdn-lfs-us-1.hf.co"
                    | "cdn-lfs-eu-1.hf.co"
                    | "cas-bridge.xethub.hf.co"
                    | "us.aws.cdn.hf.co"
                    | "eu.aws.cdn.hf.co"
            )
        )
}

// ureq's recv_body timeout covers the WHOLE body, not each socket read. Cap the
// transport timeout as well, so a stalled 118 MB transfer is promptly cancellable.
#[derive(Debug)]
struct ShortReads;
#[derive(Debug)]
struct ShortTransport(Box<dyn Transport>);
impl Connector<Box<dyn Transport>> for ShortReads {
    type Out = ShortTransport;
    fn connect(
        &self,
        _: &ConnectionDetails,
        chained: Option<Box<dyn Transport>>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        Ok(chained.map(ShortTransport))
    }
}
fn short_timeout(mut timeout: NextTimeout) -> NextTimeout {
    timeout.after = timeout.after.min(IO_TIMEOUT.into());
    timeout
}
impl Transport for ShortTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.0.buffers()
    }
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.0.transmit_output(amount, short_timeout(timeout))
    }
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.0.await_input(short_timeout(timeout))
    }
    fn is_open(&mut self) -> bool {
        self.0.is_open()
    }
    fn is_tls(&self) -> bool {
        self.0.is_tls()
    }
}
fn http_agent(https_only: bool) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .https_only(https_only)
        .proxy(None)
        .max_redirects(0)
        .timeout_global(Some(TOTAL_TIMEOUT))
        .timeout_resolve(Some(IO_TIMEOUT))
        .timeout_connect(Some(IO_TIMEOUT))
        .timeout_send_request(Some(IO_TIMEOUT))
        .timeout_recv_response(Some(IO_TIMEOUT))
        .build();
    ureq::Agent::with_parts(config, DefaultConnector::default().chain(ShortReads), DefaultResolver::default())
}
fn validate_headers(
    status: u16,
    length: Option<&str>,
    encoding: Option<&str>,
    asset: &Asset,
) -> Result<(), String> {
    if status != 200 {
        return Err(format!("model download HTTP status {status}"));
    }
    if let Some(length) = length {
        let size = length.parse::<u64>().map_err(|_| "invalid model Content-Length")?;
        if size != asset.size {
            return Err("model Content-Length differs from pinned size".into());
        }
    }
    if encoding.is_some_and(|v| !v.eq_ignore_ascii_case("identity")) {
        return Err("encoded model responses are not allowed".into());
    }
    Ok(())
}
fn fetch(
    agent: &ureq::Agent,
    asset: &Asset,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    let mut url = url::Url::parse(asset.url).map_err(|_| "invalid fixed model URL")?;
    for _ in 0..=5 {
        check_running(cancel, deadline)?;
        if !allowed_url(&url) {
            return Err("untrusted model redirect rejected".into());
        }
        let result = agent.get(url.as_str()).header("Accept-Encoding", "identity").call();
        check_running(cancel, deadline)?;
        // Do not include signed redirect URLs or response bodies in errors/logs.
        let response = result.map_err(|_| "model HTTP request failed or timed out")?;
        if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            let location = response
                .headers()
                .get("location")
                .and_then(|h| h.to_str().ok())
                .ok_or("missing redirect location")?;
            url = url.join(location).map_err(|_| "invalid redirect location")?;
            continue;
        }
        let header = |name| {
            response.headers().get(name).map(|h| h.to_str().map_err(|_| "invalid HTTP header")).transpose()
        };
        validate_headers(
            response.status().as_u16(),
            header("content-length")?,
            header("content-encoding")?,
            asset,
        )?;
        return Ok(response);
    }
    Err("too many model redirects".into())
}
fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) { Err("model download cancelled".into()) } else { Ok(()) }
}
fn check_running(cancel: &AtomicBool, deadline: Instant) -> Result<(), String> {
    check_cancel(cancel)?;
    if Instant::now() >= deadline {
        return Err("model operation timed out".into());
    }
    Ok(())
}
fn copy_verified(
    mut read: impl Read,
    mut write: impl Write,
    asset: &Asset,
    cancel: &AtomicBool,
    deadline: Instant,
    progress: impl Fn(u64),
) -> Result<(), String> {
    let mut buffer = [0; CHUNK];
    let mut hash = Sha256::new();
    let (mut received, mut reported) = (0_u64, 0_u64);
    loop {
        check_running(cancel, deadline)?;
        // At most one excess byte is read; none beyond the pinned size is written.
        let max_read = ((asset.size - received).saturating_add(1)).min(CHUNK as u64) as usize;
        let result = read.read(&mut buffer[..max_read]);
        check_running(cancel, deadline)?;
        let n = match result {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("model read failed or timed out".into()),
        };
        if n == 0 {
            break;
        }
        received += n as u64;
        if received > asset.size {
            return Err("model exceeds pinned size".into());
        }
        hash.update(&buffer[..n]);
        write.write_all(&buffer[..n]).map_err(|e| format!("write model: {e}"))?;
        if received - reported >= CHUNK as u64 {
            progress(received);
            reported = received;
        }
    }
    if received != asset.size {
        return Err("model ended before pinned size".into());
    }
    if format!("{:x}", hash.finalize()) != asset.sha256 {
        return Err(format!("SHA-256 mismatch for {}", asset.name));
    }
    if received != reported {
        progress(received);
    }
    check_running(cancel, deadline)
}

fn manifest(assets: &[Asset]) -> String {
    let mut result = format!("aero-embedding-v1\n{MODEL_ID}\n");
    for a in assets {
        result.push_str(&format!("{} {} {}\n", a.name, a.size, a.sha256));
    }
    result
}
fn checked_directory(path: &Path, create: bool) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("model cache root must be absolute".into());
    }
    let mut current = PathBuf::new();
    for part in path.components() {
        if matches!(part, Component::ParentDir) {
            return Err("parent traversal in model cache path".into());
        }
        current.push(part);
        if create && matches!(fs::symlink_metadata(&current), Err(e) if e.kind() == io::ErrorKind::NotFound) {
            match fs::create_dir(&current) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("create model cache: {e}")),
            }
        }
        let meta = fs::symlink_metadata(&current).map_err(|e| format!("model directory: {e}"))?;
        if !meta.file_type().is_dir() {
            return Err("model cache contains a symlink or non-directory".into());
        }
    }
    Ok(())
}
pub(crate) fn open_local(path: &Path) -> Result<File, String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("model file: {e}"))?;
    if !meta.file_type().is_file() {
        return Err("model file is not a regular file".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|e| format!("open model: {e}"))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("model file changed type".into());
    }
    Ok(file)
}
fn small_matches(path: &Path, expected: &[u8]) -> bool {
    let Ok(file) = open_local(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    file.take(expected.len() as u64 + 1).read_to_end(&mut bytes).is_ok() && bytes == expected
}
fn installed(root: &Path, assets: &[Asset]) -> bool {
    let dir = model_dir(root);
    if checked_directory(&dir, false).is_err() {
        return false;
    }
    if !small_matches(&dir.join("manifest"), manifest(assets).as_bytes())
        || !small_matches(&dir.join("LICENSE"), LICENSE.as_bytes())
    {
        return false;
    }
    assets.iter().all(|a| {
        fs::symlink_metadata(dir.join(a.name)).is_ok_and(|m| m.file_type().is_file() && m.len() == a.size)
    })
}
/// Hash verification belongs on the background load path, not availability probes.
pub(crate) fn verify_local(root: &Path) -> Result<PathBuf, String> {
    if !model_available(root) {
        return Err("local model is missing or damaged; explicit download required".into());
    }
    let dir = model_dir(root);
    for a in &ASSETS {
        copy_verified(
            open_local(&dir.join(a.name))?,
            io::sink(),
            a,
            &AtomicBool::new(false),
            Instant::now() + TOTAL_TIMEOUT,
            |_| {},
        )?;
    }
    Ok(dir)
}
fn sync_directory(path: &Path) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x02000000); // FILE_FLAG_BACKUP_SEMANTICS
    }
    options.open(path).and_then(|f| f.sync_all()).map_err(|e| format!("sync model directory: {e}"))
}
fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| e.to_string())?;
    file.write_all(bytes).and_then(|()| file.sync_all()).map_err(|e| e.to_string())
}
// Unlike std::fs::rename on Unix, exclusive rename cannot replace even an empty
// corrupt directory created by another process between our check and publication.
fn publish(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let source = CString::new(source.as_os_str().as_bytes())?;
        let destination = CString::new(destination.as_os_str().as_bytes())?;
        #[cfg(target_os = "macos")]
        // SAFETY: The C strings remain valid through the call. RENAME_EXCL forbids replacement.
        let result = unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
        #[cfg(target_os = "linux")]
        // SAFETY: The C strings remain valid through the call. AT_FDCWD and RENAME_NOREPLACE are valid arguments.
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        fs::rename(source, destination)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = (source, destination);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "exclusive model publication unsupported on this platform",
        ))
    }
}
fn install_with(
    root: &Path,
    cancel: &AtomicBool,
    assets: &[Asset],
    fetch: impl Fn(&Asset, &mut File) -> Result<(), String>,
) -> Result<(), String> {
    check_cancel(cancel)?;
    checked_directory(root, true)?;
    let destination = model_dir(root);
    match fs::symlink_metadata(&destination) {
        Ok(_) if installed(root, assets) => return Ok(()),
        Ok(_) => return Err("existing model is damaged or a symlink; refusing to overwrite it".into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    // The RAII directory only removes this invocation's own files on any error/cancel.
    let stage =
        tempfile::Builder::new().prefix(".aero-e5-download-").tempdir_in(root).map_err(|e| e.to_string())?;
    for a in assets {
        check_cancel(cancel)?;
        let mut components = Path::new(a.name).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err("invalid asset name".into());
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(stage.path().join(a.name))
            .map_err(|e| e.to_string())?;
        fetch(a, &mut file)?;
        file.sync_all().map_err(|e| e.to_string())?;
    }
    write_synced(&stage.path().join("LICENSE"), LICENSE.as_bytes())?;
    write_synced(&stage.path().join("manifest"), manifest(assets).as_bytes())?;
    sync_directory(stage.path())?;
    checked_directory(root, false)?;
    check_cancel(cancel)?;
    publish(stage.path(), &destination).map_err(|e| format!("publish model without replacement: {e}"))?;
    // Its old temporary path no longer exists; dropping stage cannot remove the install.
    sync_directory(root)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{fs, io::Cursor, sync::atomic::Ordering, time::Duration};
    const SMALL: Asset = Asset {
        name: "test.bin",
        size: 3,
        sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        url: "https://huggingface.co/test",
    };
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }
    fn root() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().canonicalize().unwrap();
        (d, p)
    }
    fn install(root: &Path, cancel: &AtomicBool) -> Result<(), String> {
        install_with(root, cancel, &[SMALL], |a, f| {
            copy_verified(Cursor::new(b"abc"), f, a, cancel, deadline(), |_| {})
        })
    }
    #[test]
    fn fixed_public_identity_and_download_size() {
        assert!(crate::MODEL_ID.contains("761b726dd34fb83930e26aab4e9ac3899aa1fa78"));
        assert!(crate::MODEL_ID.contains("int8"));
        assert_eq!(crate::DOWNLOAD_BYTES, 135_137_323);
        let (_d, root) = root();
        assert!(model_dir(&root).starts_with(&root));
        assert!(model_dir(&root).file_name().unwrap().to_str().unwrap().contains("761b726"));
        assert!(!model_available(&root));
    }
    #[test]
    fn sha_and_size_are_both_enforced() {
        let c = AtomicBool::new(false);
        let mut out = Vec::new();
        copy_verified(Cursor::new(b"abc"), &mut out, &SMALL, &c, deadline(), |_| {}).unwrap();
        assert_eq!(out, b"abc");
        for data in [b"ab".as_slice(), b"abcd", b"abd"] {
            assert!(copy_verified(Cursor::new(data), Vec::new(), &SMALL, &c, deadline(), |_| {}).is_err());
        }
    }
    #[test]
    fn cancellation_and_deadline_stop_streams() {
        let c = AtomicBool::new(true);
        assert!(
            copy_verified(Cursor::new(b"abc"), Vec::new(), &SMALL, &c, deadline(), |_| {})
                .unwrap_err()
                .contains("cancel")
        );
        c.store(false, Ordering::Relaxed);
        assert!(copy_verified(Cursor::new(b"abc"), Vec::new(), &SMALL, &c, Instant::now(), |_| {}).is_err());
        let data = vec![7; 200_000];
        let hash = format!("{:x}", Sha256::digest(&data));
        let a = Asset { size: data.len() as u64, sha256: Box::leak(hash.into_boxed_str()), ..SMALL };
        let mut out = Vec::new();
        assert!(
            copy_verified(Cursor::new(data), &mut out, &a, &c, deadline(), |n| {
                assert!(n <= 65_536);
                c.store(true, Ordering::Relaxed);
            })
            .is_err()
        );
        assert!(out.len() <= 65_536);
    }
    #[test]
    fn interrupted_and_failed_reads_do_not_publish() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::TimedOut))
            }
        }
        let c = AtomicBool::new(false);
        let (_d, r) = root();
        assert!(
            install_with(&r, &c, &[SMALL], |a, f| copy_verified(Broken, f, a, &c, deadline(), |_| {}))
                .is_err()
        );
        assert_eq!(fs::read_dir(&r).unwrap().count(), 0);
    }
    #[test]
    fn manifest_publication_and_availability_only_check_sizes() {
        let c = AtomicBool::new(false);
        let (_d, r) = root();
        install(&r, &c).unwrap();
        assert!(installed(&r, &[SMALL]));
        assert_eq!(fs::read_to_string(model_dir(&r).join("manifest")).unwrap(), manifest(&[SMALL]));
        fs::write(model_dir(&r).join(SMALL.name), b"xyz").unwrap();
        assert!(installed(&r, &[SMALL])); // no large-file hash on UI availability probes
        fs::write(model_dir(&r).join(SMALL.name), b"xy").unwrap();
        assert!(!installed(&r, &[SMALL]));
        assert!(install(&r, &c).is_err());
        assert_eq!(fs::read(model_dir(&r).join(SMALL.name)).unwrap(), b"xy");
    }
    #[test]
    fn cancel_before_publish_cleans_only_own_temp_and_preserves_install() {
        let c = AtomicBool::new(false);
        let (_d, r) = root();
        fs::write(r.join("unrelated"), b"keep").unwrap();
        assert!(
            install_with(&r, &c, &[SMALL], |_, f| {
                f.write_all(b"abc").unwrap();
                c.store(true, Ordering::Relaxed);
                Ok(())
            })
            .is_err()
        );
        assert!(!model_dir(&r).exists());
        assert_eq!(fs::read_dir(&r).unwrap().count(), 1);
        c.store(false, Ordering::Relaxed);
        install(&r, &c).unwrap();
        c.store(true, Ordering::Relaxed);
        let _ = install(&r, &c);
        assert!(installed(&r, &[SMALL]));
        assert_eq!(fs::read(r.join("unrelated")).unwrap(), b"keep");
    }
    #[test]
    fn changed_manifest_is_not_available_or_overwritten() {
        let c = AtomicBool::new(false);
        let (_d, r) = root();
        install(&r, &c).unwrap();
        fs::write(model_dir(&r).join("manifest"), "wrong revision").unwrap();
        assert!(!installed(&r, &[SMALL]));
        assert!(install(&r, &c).is_err());
    }
    #[test]
    fn response_headers_reject_unbounded_declared_lengths_and_encoding() {
        assert!(validate_headers(200, Some("3"), None, &SMALL).is_ok());
        assert!(validate_headers(200, None, None, &SMALL).is_ok()); // actual bytes still bounded
        for length in ["2", "4", "nonsense", "18446744073709551616"] {
            assert!(validate_headers(200, Some(length), None, &SMALL).is_err());
        }
        assert!(validate_headers(206, Some("3"), None, &SMALL).is_err());
        assert!(validate_headers(200, Some("3"), Some("gzip"), &SMALL).is_err());
    }
    #[test]
    fn stalled_http_body_has_a_short_read_timeout() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (done, wait) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).unwrap() > 0);
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n").unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(5));
        });
        let start = Instant::now();
        let mut response = http_agent(false).get(format!("http://{address}/")).call().unwrap();
        let result = response.body_mut().as_reader().read_exact(&mut [0; 3]);
        let _ = done.send(());
        server.join().unwrap();
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_secs(5));
    }
    #[test]
    fn progressing_body_may_outlive_the_response_header_timeout() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (done, wait) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).unwrap() > 0);
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n").unwrap();
            for _ in 0..7 {
                if socket.write_all(b"a").is_err() {
                    break;
                }
                if wait.recv_timeout(Duration::from_secs(1)).is_ok() {
                    break;
                }
            }
        });
        let mut response = http_agent(false).get(format!("http://{address}/")).call().unwrap();
        let mut bytes = Vec::new();
        let result = response.body_mut().as_reader().read_to_end(&mut bytes);
        let _ = done.send(());
        server.join().unwrap();
        assert!(result.is_ok(), "progressing body wrongly timed out: {result:?}");
        assert_eq!(bytes, b"aaaaaaa");
    }
    #[test]
    fn redirects_require_https_exact_hf_cdn_hosts_and_no_credentials() {
        for u in [
            "https://huggingface.co/a",
            "https://cdn-lfs.huggingface.co/a",
            "https://cdn-lfs-us-1.hf.co/a",
            "https://cas-bridge.xethub.hf.co/a",
            "https://us.aws.cdn.hf.co/a",
        ] {
            assert!(allowed_url(&url::Url::parse(u).unwrap()), "{u}");
        }
        for u in [
            "http://huggingface.co/a",
            "https://huggingface.co.evil.com/a",
            "https://evil.hf.co/a",
            "https://hf.co@evil.com/a",
            "https://user:secret@huggingface.co/a",
            "https://huggingface.co:444/a",
            "file:///tmp/a",
        ] {
            assert!(!allowed_url(&url::Url::parse(u).unwrap()), "{u}");
        }
    }
    #[test]
    fn publication_never_replaces_a_racing_destination() {
        let c = AtomicBool::new(false);
        let (_d, r) = root();
        assert!(
            install_with(&r, &c, &[SMALL], |a, f| {
                copy_verified(Cursor::new(b"abc"), f, a, &c, deadline(), |_| {})?;
                fs::create_dir(model_dir(&r)).unwrap();
                Ok(())
            })
            .is_err()
        );
        assert_eq!(fs::read_dir(model_dir(&r)).unwrap().count(), 0);
        assert_eq!(fs::read_dir(&r).unwrap().count(), 1);
    }
    #[test]
    fn interrupted_and_fragmented_reads_are_retried_with_throttled_progress() {
        struct Fragmented {
            calls: usize,
            data: Cursor<Vec<u8>>,
        }
        impl Read for Fragmented {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                self.calls += 1;
                if self.calls == 1 {
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                let n = b.len().min(7);
                self.data.read(&mut b[..n])
            }
        }
        let c = AtomicBool::new(false);
        let data = vec![42; 140_000];
        let hash = format!("{:x}", Sha256::digest(&data));
        let a = Asset { size: data.len() as u64, sha256: Box::leak(hash.into_boxed_str()), ..SMALL };
        let progress = std::cell::RefCell::new(Vec::new());
        let mut output = Vec::new();
        copy_verified(
            Fragmented { calls: 0, data: Cursor::new(data.clone()) },
            &mut output,
            &a,
            &c,
            deadline(),
            |n| progress.borrow_mut().push(n),
        )
        .unwrap();
        assert_eq!(output, data);
        assert_eq!(progress.borrow().len(), 3);
        assert_eq!(*progress.borrow().last().unwrap(), a.size);
    }
    #[cfg(unix)]
    #[test]
    fn rejects_symlink_roots_model_directories_and_files() {
        use std::os::unix::fs::symlink;
        let c = AtomicBool::new(false);
        let (_d, r) = root();
        let (_elsewhere, other) = root();
        let link = r.join("link");
        symlink(&other, &link).unwrap();
        assert!(install(&link, &c).is_err());
        symlink(&other, model_dir(&r)).unwrap();
        assert!(install(&r, &c).is_err());
        fs::remove_file(model_dir(&r)).unwrap();
        install(&r, &c).unwrap();
        fs::remove_file(model_dir(&r).join(SMALL.name)).unwrap();
        fs::write(other.join("data"), b"abc").unwrap();
        symlink(other.join("data"), model_dir(&r).join(SMALL.name)).unwrap();
        assert!(!installed(&r, &[SMALL]));
        assert!(install(&r, &c).is_err());
    }
}
