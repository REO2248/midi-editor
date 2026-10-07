//! Single-instance guard (#197, Windows only).
//!
//! Without the guard a second `midi-editor.exe` starts fully: it fails to
//! bind the MCP port (127.0.0.1:7878) with a noisy error, and two instances
//! editing the same `.mid` race on the `.editor.json`/`.editor.state`
//! sidecars. This module makes the first launch the *primary* and every
//! later launch a *secondary* that forwards its file arguments to the
//! primary through a named pipe and exits — unless the user passes
//! `--new-instance`, which skips the guard entirely (genuinely parallel
//! instances remain possible, at their own risk).
//!
//! Protocol:
//! 1. the primary owns the named mutex `Local\midi-editor-single-instance`
//!    for the whole process lifetime (the OS releases it on process death,
//!    so a crashed primary cannot wedge the next launch);
//! 2. a secondary opens the same mutex, sees `ERROR_ALREADY_EXISTS`, writes
//!    one UTF-8 JSON line `{"paths": [...]}` to the byte-mode named pipe
//!    `\\.\pipe\midi-editor-instance` (duplex: the primary answers with a
//!    one-byte ack once the paths are queued), and exits 0;
//! 3. the primary's listener thread (only alive while the mutex is held)
//!    accepts connections, decodes the line, and parks the paths in
//!    `pending_opens`; the doc-watch tick drains them through the same
//!    discard-guarded open the drag/drop path uses and foregrounds the
//!    window.
//!
//! Failure handling: the OS cannot leave a stale mutex, but the pipe server
//! may not be up yet while the primary is still starting, so the secondary
//! waits a bounded time for the pipe; forwarding reports success only after
//! the primary acknowledges the hand-off (one byte written once the paths
//! are queued). If forwarding fails the user's file argument must never be
//! lost — the process falls back to launching as a normal (unguarded)
//! instance and opens the file locally.
//!
//! Everything Windows-specific is inside `imp`; the pure parts (argument
//! classification, message encode/decode) compile on every platform so
//! their tests run in CI on any host.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Hand-off queue between the pipe listener thread and the doc-watch tick
/// (`spawn_doc_watch`). `Some` only in the primary; a secondary never
/// reaches the GUI.
pub(crate) type PendingOpens = Arc<Mutex<Vec<PathBuf>>>;

/// Pure CLI classification: `--new-instance` is the escape hatch, every
/// other non-flag argument is a file path (the old `file_arg()` rule —
/// Explorer always quotes "%1"; flags are never paths). Unlike the old
/// first-arg-only pick, every file argument is collected: forwarding hands
/// the whole set to the running instance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LaunchArgs {
    /// `--new-instance` was given — skip the guard entirely
    pub new_instance: bool,
    /// file arguments, in command-line order
    pub files: Vec<PathBuf>,
}

pub(crate) fn classify_args(args: impl Iterator<Item = std::ffi::OsString>) -> LaunchArgs {
    let mut out = LaunchArgs::default();
    for a in args {
        if a.as_os_str() == "--new-instance" {
            out.new_instance = true;
        } else if !a.to_string_lossy().starts_with('-') {
            out.files.push(PathBuf::from(a));
        }
    }
    out
}

/// One UTF-8 JSON line. JSON quoting keeps the payload a single line even
/// for paths that contain newlines, quotes, or backslashes.
pub(crate) fn encode_message(paths: &[PathBuf]) -> String {
    #[derive(serde::Serialize)]
    struct PipeMessage<'a> {
        paths: &'a [PathBuf],
    }
    let mut s = serde_json::to_string(&PipeMessage { paths }).unwrap_or_default();
    s.push('\n');
    s
}

/// Inverse of `encode_message`; `None` for anything that is not a JSON
/// object of the expected shape.
pub(crate) fn decode_message(line: &str) -> Option<Vec<PathBuf>> {
    #[derive(serde::Deserialize)]
    struct PipeMessage {
        paths: Vec<PathBuf>,
    }
    match serde_json::from_str::<PipeMessage>(line.trim()) {
        Ok(m) => Some(m.paths),
        Err(_) => None,
    }
}

/// Startup-time single-instance decision. Kept alive by `main` for the
/// process lifetime so the primary's mutex handle stays held.
pub(crate) struct Startup {
    /// primary only: where forwarded paths arrive
    pub(crate) pending_opens: Option<PendingOpens>,
    #[cfg(windows)]
    /// holds `Local\midi-editor-single-instance`; dropping it releases the
    /// guard (which only happens at process exit)
    _guard: Option<imp::PrimaryGuard>,
}

/// Entry point called once from `main` before the GUI is created. On
/// Windows this either (a) claims the primary role and starts the pipe
/// server, (b) forwards this launch's file arguments to the running
/// primary and exits the process, or (c) falls back to an unguarded launch
/// when the guard itself is unusable. Non-Windows platforms are a no-op —
/// today's multi-instance behavior stays exactly as it is.
pub(crate) fn startup(cli: &LaunchArgs) -> Startup {
    #[cfg(not(windows))]
    {
        let _ = cli;
        Startup {
            pending_opens: None,
        }
    }
    #[cfg(windows)]
    {
        // Escape hatch (#197): `--new-instance` skips the mutex, the pipe
        // and the forwarding entirely — the user asked for a genuinely
        // parallel instance and gets one, sidecar races and MCP-port
        // collision included.
        if cli.new_instance {
            tracing::info!("--new-instance given: skipping the single-instance guard");
            return Startup {
                pending_opens: None,
                _guard: None,
            };
        }
        match imp::try_claim_mutex(imp::MUTEX_NAME) {
            Ok(imp::MutexClaim::Acquired(guard)) => {
                let pending_opens: PendingOpens = Arc::new(Mutex::new(Vec::new()));
                // the listener thread outlives nothing: it is a daemon the
                // OS reaps at process exit (it blocks in ConnectNamedPipe
                // and cannot be interrupted, so it is deliberately not in
                // the bounded-shutdown coordinator)
                if imp::spawn_server(imp::PIPE_NAME.to_string(), pending_opens.clone()).is_none() {
                    // extremely unlikely (thread spawn failure): the
                    // guard stays held but forwarding is unavailable
                    tracing::warn!("failed to spawn the hand-off pipe server");
                }
                Startup {
                    pending_opens: Some(pending_opens),
                    _guard: Some(guard),
                }
            }
            Ok(imp::MutexClaim::AlreadyExists) => {
                // A primary is running. Forward the file arguments and exit;
                // with no files to forward there is nothing to hand over.
                if cli.files.is_empty() {
                    tracing::info!("midi-editor is already running; exiting this instance");
                    std::process::exit(0);
                }
                match imp::forward_files(imp::PIPE_NAME, &cli.files, imp::FORWARD_TIMEOUT) {
                    Ok(()) => {
                        tracing::info!(
                            count = cli.files.len(),
                            "file arguments forwarded to the running instance"
                        );
                        std::process::exit(0);
                    }
                    Err(e) => {
                        // Never lose the user's file argument: fall back to
                        // a normal unguarded instance and open locally. The
                        // MCP port may then collide with the running
                        // primary — accepted, and logged.
                        tracing::warn!(
                            error = %e,
                            "forwarding to the running instance failed; opening locally"
                        );
                        Startup {
                            pending_opens: None,
                            _guard: None,
                        }
                    }
                }
            }
            Err(e) => {
                // Mutex creation failed — the guard is unusable, so behave
                // like the pre-#197 app instead of blocking the launch.
                tracing::warn!(error = %e, "single-instance mutex unavailable; guard disabled");
                Startup {
                    pending_opens: None,
                    _guard: None,
                }
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::{decode_message, encode_message, PendingOpens};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use windows::core::{HRESULT, HSTRING};
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND,
        ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE,
        INVALID_HANDLE_VALUE, WIN32_ERROR,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_FIRST_PIPE_INSTANCE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW,
        PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows::Win32::System::Threading::CreateMutexW;

    /// Per-session namespace (`Local\`): one guard per interactive session,
    /// not machine-global.
    pub(super) const MUTEX_NAME: &str = "Local\\midi-editor-single-instance";
    pub(super) const PIPE_NAME: &str = "\\\\.\\pipe\\midi-editor-instance";
    /// Bounded wait for the primary's pipe: the pipe server starts before
    /// the GUI, but on a cold start there is a window where the mutex is
    /// held and the pipe does not exist yet.
    pub(super) const FORWARD_TIMEOUT: Duration = Duration::from_secs(2);
    /// Wire cap — a legitimate hand-off is a handful of paths.
    const MAX_MESSAGE_BYTES: usize = 64 * 1024;
    /// Poll interval while the secondary waits for the pipe to appear.
    const CONNECT_RETRY: Duration = Duration::from_millis(50);
    /// Consecutive pipe-creation failures before the listener gives up
    /// (the mutex guard stays held either way).
    const MAX_PIPE_FAILURES: u32 = 5;

    /// Outcome of trying to become the primary.
    pub(super) enum MutexClaim {
        /// this process created the mutex first — it is the primary
        Acquired(PrimaryGuard),
        /// the mutex already exists — a primary is running
        AlreadyExists,
    }

    /// Owns the primary's mutex handle for the process lifetime. The OS
    /// releases the mutex when the process dies, so a crash can never leave
    /// a stale guard behind.
    pub(super) struct PrimaryGuard(HANDLE);

    impl Drop for PrimaryGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// Create (or open) the single-instance mutex and classify this launch.
    pub(super) fn try_claim_mutex(name: &str) -> windows::core::Result<MutexClaim> {
        let wide = HSTRING::from(name);
        let h = unsafe { CreateMutexW(None, false, &wide) }?;
        // ERROR_ALREADY_EXISTS accompanies a *successful* open of an
        // existing mutex — including from within the same process.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                let _ = CloseHandle(h);
            }
            return Ok(MutexClaim::AlreadyExists);
        }
        Ok(MutexClaim::Acquired(PrimaryGuard(h)))
    }

    fn err_is(e: &windows::core::Error, code: WIN32_ERROR) -> bool {
        e.code() == HRESULT::from_win32(code.0)
    }

    /// One pipe instance in byte mode, duplex (the client writes its
    /// hand-off line and waits for a one-byte ack). `PIPE_ACCESS_DUPLEX`
    /// lets the server acknowledge after the paths are queued, so a
    /// secondary only reports success when delivery actually happened.
    /// `FILE_FLAG_FIRST_PIPE_INSTANCE` asserts nobody else created the
    /// pipe — redundant with the mutex, but it turns a future bug into a
    /// loud error instead of silent misdelivery.
    fn create_pipe_instance(name: &str) -> Result<HANDLE, String> {
        let wide = HSTRING::from(name);
        let h = unsafe {
            CreateNamedPipeW(
                &wide,
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                None,
            )
        };
        // CreateNamedPipeW reports failure through INVALID_HANDLE_VALUE,
        // not a Result.
        if h == INVALID_HANDLE_VALUE {
            let code = unsafe { GetLastError() };
            Err(format!(
                "CreateNamedPipeW failed: {}",
                windows::core::Error::from_hresult(HRESULT::from_win32(code.0))
            ))
        } else {
            Ok(h)
        }
    }

    /// Accept one connection, read one line, park the decoded paths.
    /// `Err` means the listener could not serve at all (create/connect);
    /// a malformed message is logged and swallowed.
    fn serve_once(name: &str, pending: &PendingOpens) -> Result<(), String> {
        let pipe = create_pipe_instance(name)?;
        match unsafe { ConnectNamedPipe(pipe, None) } {
            Ok(()) => {}
            // the client connected between CreateNamedPipeW and
            // ConnectNamedPipe — that is success for our purposes
            Err(e) if err_is(&e, ERROR_PIPE_CONNECTED) => {}
            Err(e) => {
                unsafe {
                    let _ = CloseHandle(pipe);
                }
                return Err(format!("ConnectNamedPipe failed: {e}"));
            }
        }
        if let Some(line) = read_line(pipe) {
            match decode_message(&line) {
                Some(paths) if !paths.is_empty() => {
                    {
                        let mut q = pending.lock().unwrap_or_else(|e| e.into_inner());
                        q.extend(paths);
                    }
                    ack(pipe);
                }
                // empty list: nothing to queue, but the message was
                // understood — ack so the secondary can exit cleanly
                Some(_) => ack(pipe),
                // malformed: no ack. The client sees a broken pipe, reports
                // failure, and falls back to opening the files locally —
                // its file arguments are never silently lost.
                None => tracing::warn!("discarded a malformed single-instance hand-off message"),
            }
        }
        unsafe {
            let _ = DisconnectNamedPipe(pipe);
            let _ = CloseHandle(pipe);
        }
        Ok(())
    }

    /// Tell the client its message is queued. A failed write cannot be
    /// helped from here — the client's ack read fails and it falls back to
    /// a local launch.
    fn ack(pipe: HANDLE) {
        let mut written = 0u32;
        let _ = unsafe { WriteFile(pipe, Some(b"1".as_slice()), Some(&mut written), None) };
    }

    /// Read up to one newline-terminated line (or until the client closes
    /// the pipe). Byte-mode pipe: reads are plain ReadFile calls.
    fn read_line(pipe: HANDLE) -> Option<String> {
        let mut buf = [0u8; 4096];
        let mut acc: Vec<u8> = Vec::new();
        loop {
            let mut n = 0u32;
            match unsafe { ReadFile(pipe, Some(&mut buf), Some(&mut n), None) } {
                Ok(()) => {}
                // client closed its end — whatever we have is the message
                Err(e) if err_is(&e, ERROR_BROKEN_PIPE) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "single-instance pipe read failed");
                    return None;
                }
            }
            if n == 0 {
                break;
            }
            let chunk = &buf[..n as usize];
            if acc.len() + chunk.len() > MAX_MESSAGE_BYTES {
                tracing::warn!("single-instance hand-off message too large; dropped");
                return None;
            }
            if let Some(pos) = chunk.iter().position(|&b| b == b'\n') {
                acc.extend_from_slice(&chunk[..pos]);
                break;
            }
            acc.extend_from_slice(chunk);
        }
        String::from_utf8(acc).ok()
    }

    /// Listener loop: accept hand-off connections forever. Returns `None`
    /// when the thread could not be spawned.
    pub(super) fn spawn_server(
        pipe_name: String,
        pending: PendingOpens,
    ) -> Option<std::thread::JoinHandle<()>> {
        std::thread::Builder::new()
            .name("single-instance-pipe".into())
            .spawn(move || {
                let mut failures = 0u32;
                loop {
                    match serve_once(&pipe_name, &pending) {
                        Ok(()) => failures = 0,
                        Err(e) => {
                            failures += 1;
                            tracing::warn!(error = %e, "single-instance pipe server error");
                            if failures >= MAX_PIPE_FAILURES {
                                tracing::error!("single-instance pipe server giving up");
                                return;
                            }
                            std::thread::sleep(Duration::from_secs(1));
                        }
                    }
                }
            })
            .ok()
    }

    /// Secondary side: connect to the primary's pipe within `timeout`
    /// (the primary may still be starting up), write one JSON line, and
    /// wait for the primary's one-byte ack — success means the paths are
    /// actually queued in the running instance, not merely written into a
    /// pipe whose reader may have vanished.
    pub(super) fn forward_files(
        pipe_name: &str,
        files: &[PathBuf],
        timeout: Duration,
    ) -> Result<(), String> {
        let fail = |pipe: HANDLE, msg: String| -> String {
            unsafe {
                let _ = CloseHandle(pipe);
            }
            msg
        };
        let pipe = connect_client(pipe_name, timeout)?;
        let message = encode_message(files);
        let bytes = message.as_bytes();
        let mut off = 0usize;
        while off < bytes.len() {
            let mut written = 0u32;
            if let Err(e) =
                unsafe { WriteFile(pipe, Some(&bytes[off..]), Some(&mut written), None) }
            {
                return Err(fail(pipe, format!("WriteFile failed: {e}")));
            }
            if written == 0 {
                return Err(fail(pipe, "WriteFile made no progress".into()));
            }
            off += written as usize;
        }
        // the ack is the delivery receipt: the primary queued the paths.
        // Anything else (broken pipe, EOF, wrong byte) means the hand-off
        // did not land — the caller falls back to a local launch.
        let mut ack = [0u8; 1];
        let mut n = 0u32;
        match unsafe { ReadFile(pipe, Some(&mut ack), Some(&mut n), None) } {
            Ok(()) if n == 1 && ack[0] == b'1' => {}
            Ok(_) => return Err(fail(pipe, "unexpected hand-off ack".into())),
            Err(e) => return Err(fail(pipe, format!("hand-off ack failed: {e}"))),
        }
        unsafe {
            let _ = CloseHandle(pipe);
        }
        Ok(())
    }

    /// CreateFileW loop with a bounded wait: `ERROR_FILE_NOT_FOUND` means
    /// the primary has not created the pipe yet (startup race),
    /// `ERROR_PIPE_BUSY` means all instances are mid-hand-off — wait for
    /// one to free up. Anything else, or the deadline, is a failure.
    fn connect_client(pipe_name: &str, timeout: Duration) -> Result<HANDLE, String> {
        let deadline = Instant::now() + timeout;
        let wide = HSTRING::from(pipe_name);
        loop {
            match unsafe {
                CreateFileW(
                    &wide,
                    GENERIC_READ.0 | GENERIC_WRITE.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                )
            } {
                Ok(h) => return Ok(h),
                Err(e) => {
                    let busy = err_is(&e, ERROR_PIPE_BUSY);
                    if !busy && !err_is(&e, ERROR_FILE_NOT_FOUND) {
                        return Err(format!("pipe connect failed: {e}"));
                    }
                    if Instant::now() >= deadline {
                        return Err(format!(
                            "gave up waiting for the running instance's pipe after {timeout:?}"
                        ));
                    }
                    if busy {
                        // WaitNamedPipeW sleeps until an instance frees up
                        // (or its own timeout) — retry either way; the
                        // deadline above bounds the whole loop
                        let ms = deadline
                            .saturating_duration_since(Instant::now())
                            .as_millis()
                            .min(u32::MAX as u128) as u32;
                        unsafe {
                            let _ = WaitNamedPipeW(&wide, ms.max(1));
                        }
                    } else {
                        // pipe not created yet (primary still starting up)
                        std::thread::sleep(CONNECT_RETRY);
                    }
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::mpsc;
        use std::sync::Arc;

        /// Unique per-session names so tests never meet a real instance or
        /// each other.
        fn test_mutex_name(tag: &str) -> String {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            format!("Local\\midi-editor-test-{}-{}-{tag}", std::process::id(), n)
        }

        fn test_pipe_name(tag: &str) -> String {
            format!("\\\\.\\pipe\\midi-editor-test-{}-{tag}", std::process::id())
        }

        /// Opening the same named mutex twice in-process must detect the
        /// second open as ERROR_ALREADY_EXISTS (CreateMutexW still returns
        /// a valid handle), and the first owner keeps the guard.
        #[test]
        fn mutex_claim_detects_second_acquire_in_process() {
            let name = test_mutex_name("claim");
            let first = try_claim_mutex(&name).expect("first claim succeeds");
            let guard = match first {
                MutexClaim::Acquired(g) => g,
                MutexClaim::AlreadyExists => panic!("first claim saw an existing mutex"),
            };
            let second = try_claim_mutex(&name).expect("second claim opens");
            assert!(matches!(second, MutexClaim::AlreadyExists));
            drop(guard);
            // released — a third claim acquires again
            let third = try_claim_mutex(&name).expect("third claim after release");
            assert!(matches!(third, MutexClaim::Acquired(_)));
        }

        /// Full client→server hand-off over a real (test-named) pipe: the
        /// server parks the decoded paths in the pending queue.
        #[test]
        fn pipe_server_receives_forwarded_paths() {
            let name = test_pipe_name("handoff");
            let pending: PendingOpens = Arc::new(std::sync::Mutex::new(Vec::new()));
            let (done_tx, done_rx) = mpsc::channel();
            let p = pending.clone();
            let n = name.clone();
            std::thread::spawn(move || {
                let r = serve_once(&n, &p);
                let _ = done_tx.send(r);
            });
            let files = vec![
                PathBuf::from(r"C:\My Music\ünïcodé song.mid"),
                PathBuf::from("D:\\quo\"ted\\back\\slash.mid"),
            ];
            forward_files(&name, &files, Duration::from_secs(5))
                .expect("client forwarded successfully");
            done_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("server served the connection")
                .expect("server had no errors");
            let q = pending.lock().unwrap();
            assert_eq!(*q, files);
        }

        /// A secondary whose primary is slow must time out rather than
        /// hang: no pipe exists under this name, so forwarding fails.
        #[test]
        fn forward_times_out_when_no_primary_pipe() {
            let name = test_pipe_name("absent");
            let t0 = Instant::now();
            let r = forward_files(&name, &[PathBuf::from("x.mid")], Duration::from_millis(150));
            assert!(r.is_err());
            assert!(t0.elapsed() < Duration::from_secs(5));
        }

        /// A message the primary cannot parse gets no ack: the client sees
        /// a failed/broken read, `forward_files` reports failure, and the
        /// fallback opens the files locally instead of losing them.
        #[test]
        fn malformed_handoff_gets_no_ack_and_fails_forwarding() {
            let name = test_pipe_name("garbage");
            let pending: PendingOpens = Arc::new(std::sync::Mutex::new(Vec::new()));
            let (done_tx, done_rx) = mpsc::channel();
            let p = pending.clone();
            let n = name.clone();
            std::thread::spawn(move || {
                let r = serve_once(&n, &p);
                let _ = done_tx.send(r);
            });
            let pipe = connect_client(&name, Duration::from_secs(5)).expect("connect");
            let mut written = 0u32;
            unsafe {
                WriteFile(
                    pipe,
                    Some("not json\n".as_bytes()),
                    Some(&mut written),
                    None,
                )
            }
            .expect("write");
            let mut ack = [0u8; 1];
            let mut nread = 0u32;
            let r = unsafe { ReadFile(pipe, Some(&mut ack), Some(&mut nread), None) };
            // broken pipe or EOF — anything a client treats as failure
            assert!(
                r.is_err() || nread == 0,
                "malformed message must not be acked"
            );
            assert!(pending.lock().unwrap().is_empty());
            // release our end (the server has already disconnected)
            unsafe {
                let _ = CloseHandle(pipe);
            }
            done_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("server served the connection")
                .expect("server had no errors");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_args, decode_message, encode_message};
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn os<'a>(args: &'a [&'a str]) -> impl Iterator<Item = OsString> + 'a {
        args.iter().map(|s| OsString::from(*s))
    }

    /// `--new-instance` is recognized anywhere on the command line and is
    /// never taken for a file path.
    #[test]
    fn classify_args_detects_new_instance_escape_hatch() {
        let a = classify_args(os(&["--new-instance"]));
        assert!(a.new_instance);
        assert!(a.files.is_empty());

        let a = classify_args(os(&["--fullscreen", "--new-instance", r"C:\a.mid"]));
        assert!(a.new_instance);
        assert_eq!(a.files, vec![PathBuf::from(r"C:\a.mid")]);
    }

    /// The old `file_arg()` semantics are preserved: flags are never paths
    /// and the first path is the one a plain launch opens.
    #[test]
    fn classify_args_skips_flags_and_picks_first_path() {
        let a = classify_args(os(&[
            "--fullscreen",
            r"C:\Music\my song.mid",
            r"D:\other.mid",
        ]));
        assert!(!a.new_instance);
        assert_eq!(
            a.files.first(),
            Some(&PathBuf::from(r"C:\Music\my song.mid"))
        );
        assert_eq!(classify_args(os(&[])), super::LaunchArgs::default());
        let a = classify_args(os(&["--only-flags"]));
        assert!(a.files.is_empty());
    }

    /// The wire format survives spaces, unicode, quotes, backslashes — and
    /// even a newline *inside* a path (JSON escaping keeps it one line).
    #[test]
    fn message_roundtrip_with_weird_paths() {
        let paths = vec![
            PathBuf::from(r"C:\Music\my song.mid"),
            PathBuf::from("D:\\ünïcodé\\日本語の曲.mid"),
            PathBuf::from("E:\\quo\"ted\\back\\slash.mid"),
            PathBuf::from("F:/forward/slash.mid"),
            PathBuf::from("G:\\tricky\nnewline.mid"),
            PathBuf::from("emoji 🎹.mid"),
        ];
        let wire = encode_message(&paths);
        // one line: exactly one trailing newline
        assert_eq!(wire.matches('\n').count(), 1);
        assert!(wire.ends_with('\n'));
        assert_eq!(decode_message(&wire), Some(paths));
    }

    /// Empty path lists round-trip (nothing to open), and garbage never
    /// decodes into paths.
    #[test]
    fn message_decode_rejects_garbage_and_accepts_empty() {
        assert_eq!(decode_message(&encode_message(&[])), Some(Vec::new()));
        assert_eq!(decode_message("not json"), None);
        assert_eq!(decode_message("{\"paths\": 42}"), None);
        assert_eq!(decode_message("[1, 2, 3]"), None);
        assert_eq!(decode_message(""), None);
    }
}
