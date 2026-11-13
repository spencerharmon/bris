//! `auto-upload-collection-acceptance` (ROI rule 1b) — CLI half of the
//! automatic-upload acceptance seam. The Android half lives in
//! `bris-android/app/src/androidTest`.
//!
//! Every upload here is produced by the REAL `bris` CLI frontend's
//! session-completion hook (`bris session complete`) and its bounded
//! queue driver (`bris uploads retry`) — never by direct
//! `SubmissionReview::approve` / transport calls — and lands in a REAL
//! `bris-collector` (axum router on a loopback TCP port) with temporary
//! durable storage and a per-device token issued by the collector's own
//! `register_device`. The collector is stopped and restarted on the same
//! port and data-root to exercise offline retry and durable reopen.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use bris_collector::routes::AppState;
use bris_collector::store::Store;
use bris_collector::{build_app, Config};
use sha2::{Digest, Sha256};

const DEVICE: &str = "6f1d2c1e-6a43-4f0e-9a39-6c2b9c1f0a11";
const TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------- collector

/// A real collector bound to a fixed loopback address over a durable
/// data-root. Dropping it shuts the server down (so the same port and
/// data-root can be reopened to simulate a restart).
struct Collector {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Collector {
    fn start(data_root: &Path, addr: SocketAddr) -> Self {
        let cfg = Config {
            data_root: data_root.to_path_buf(),
            bind: addr.to_string(),
            bearer_token: "admin-bootstrap-token".to_owned(),
            max_submission_bytes: 32 * 1024 * 1024,
            retention_days: 30,
        };
        let store = Store::open(&cfg.data_root).expect("open durable store");
        let app = build_app(Arc::new(AppState { config: cfg, store }));
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = bind_retry(addr).await;
                ready_tx.send(()).unwrap();
                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = rx.await;
                    })
                    .await
                    .expect("serve");
            });
        });
        ready_rx.recv_timeout(TIMEOUT).expect("collector bound");
        Self {
            shutdown: Some(tx),
            worker: Some(worker),
        }
    }
}

async fn bind_retry(addr: SocketAddr) -> tokio::net::TcpListener {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => return l,
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("bind {addr}: {e}"),
        }
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(w) = self.worker.take() {
            let r = w.join();
            if !thread::panicking() {
                r.expect("collector thread");
            }
        }
    }
}

fn free_addr() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap()
}

/// Issue a per-device token through the collector's own registration path.
fn register(data_root: &Path) -> String {
    Store::open(data_root)
        .unwrap()
        .register_device(DEVICE)
        .unwrap()
}

/// Every stored submission directory (`submissions/yyyy/mm/dd/<ulid>`).
fn stored(data_root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() || p.file_name().is_some_and(|n| n == ".staging") {
                continue;
            }
            if depth == 3 {
                out.push(p);
            } else {
                walk(&p, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(&data_root.join("submissions"), 0, &mut out);
    out.sort();
    out
}

// ---------------------------------------------------------------- bris CLI

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf()
}

/// Build (once) and locate the real `bris` frontend binary in this target dir.
fn bris_bin() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let status = Command::new(env!("CARGO"))
            .args(["build", "--quiet", "-p", "bris-cli", "--bin", "bris"])
            .arg("--manifest-path")
            .arg(workspace_root().join("Cargo.toml"))
            .status()
            .expect("cargo build bris");
        assert!(status.success(), "building bris CLI failed");
        // tests run from <target>/<profile>/deps/<test-exe>
        let exe = std::env::current_exe().unwrap();
        let bin = exe.parent().unwrap().parent().unwrap().join("bris");
        assert!(bin.is_file(), "bris binary at {}", bin.display());
        bin
    })
}

struct Fixture {
    temp: tempfile::TempDir,
    config: PathBuf,
    corpus: PathBuf,
    queue: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        Self {
            config: temp.path().join("bris.toml"),
            corpus: temp.path().join("corpus"),
            queue: temp.path().join("queue"),
            temp,
        }
    }

    /// `automatic = None` leaves the toggle at its default.
    fn configure(&self, url: &str, token: &str, automatic: Option<bool>) {
        let mut text = format!(
            "[upload]\ndrain_seconds = 3\ncollector_url = {url:?}\ntoken = {token:?}\n\
             device_uuid = {DEVICE:?}\nqueue_root = {:?}\n",
            self.queue.to_str().unwrap()
        );
        if let Some(a) = automatic {
            text.push_str(&format!("automatic_upload = {a}\n"));
        }
        let tmp = self.config.with_extension("tmp");
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(tmp, &self.config).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(bris_bin());
        c.arg("--config")
            .arg(&self.config)
            .args(args)
            .current_dir(self.temp.path())
            .env("HOME", self.temp.path())
            .env("XDG_CONFIG_HOME", self.temp.path())
            .env("XDG_DATA_HOME", self.temp.path())
            .env("NO_COLOR", "1")
            .env("NO_PROXY", "127.0.0.1")
            .env("no_proxy", "127.0.0.1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("BRIS_") {
                c.env_remove(k);
            }
        }
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        finish(self.command(args).spawn().unwrap())
    }

    fn ok(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(o.status.success(), "{args:?}: {}", text(&o));
        String::from_utf8(o.stdout).unwrap()
    }

    /// Create a session with `n` finalized captures via the real CLI.
    fn session(&self, tag: &str, n: usize) -> String {
        let corpus = self.corpus.to_str().unwrap().to_owned();
        let id = self
            .ok(&["session", "new", "--title", tag, "--corpus", &corpus])
            .trim()
            .to_owned();
        for i in 0..n {
            let cid = format!("{tag}-cap-{i}");
            let src = self.temp.path().join(&cid);
            write_capture(&src, &cid, i);
            self.ok(&[
                "session",
                "attach",
                "--session",
                &id,
                "--bundle",
                src.to_str().unwrap(),
                "--corpus",
                &corpus,
            ]);
        }
        id
    }

    fn complete_cmd(&self, id: &str) -> Command {
        let corpus = self.corpus.to_str().unwrap().to_owned();
        self.command(&["session", "complete", "--session", id, "--corpus", &corpus])
    }

    /// (automatic, pending, failed) from the operator-visible status line.
    fn status(&self) -> (String, usize, usize) {
        let out = self.ok(&["uploads", "status"]);
        let field = |k: &str| {
            out.split_whitespace()
                .find_map(|w| w.strip_prefix(&format!("{k}=")))
                .unwrap_or_else(|| panic!("missing {k}: {out}"))
                .to_owned()
        };
        (
            field("automatic_upload"),
            field("pending").parse().unwrap(),
            field("failed").parse().unwrap(),
        )
    }
}

fn text(o: &Output) -> String {
    format!(
        "status={} stdout={} stderr={}",
        o.status,
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn finish(mut child: Child) -> Output {
    let deadline = Instant::now() + TIMEOUT;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!(
                "bris timed out: {}",
                text(&child.wait_with_output().unwrap())
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

fn write_pgm(path: &Path, fill: u8) {
    let mut bytes = b"P5\n4 3\n255\n".to_vec();
    bytes.extend(std::iter::repeat_n(fill, 12));
    std::fs::write(path, bytes).unwrap();
}

/// A real bris-bundle v1 capture with frames, honest `ap_input` and a
/// DISTINCT `gps_truth`.
fn write_capture(dir: &Path, id: &str, salt: usize) {
    let frames = dir.join("frames");
    std::fs::create_dir_all(&frames).unwrap();
    for seq in 0..2u32 {
        let stem = format!("{seq:08}");
        write_pgm(
            &frames.join(format!("{stem}.pgm")),
            u8::try_from(seq * 40 + u32::try_from(salt).unwrap()).unwrap(),
        );
        std::fs::write(
            frames.join(format!("{stem}.json")),
            serde_json::to_vec(&serde_json::json!({
                "seq": seq,
                "captured_unix_ms": 1_700_000_000_000i64 + i64::from(seq) * 1000,
                "width": 4, "height": 3,
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let bundle = serde_json::json!({
        "schema_version": 1,
        "bundle_id": id,
        "device": { "model": "auto-upload-e2e" },
        "capture": {
            "source_rotation_deg": 0, "frame_count": 2,
            "started_unix_ms": 1_700_000_000_000i64,
            "ended_unix_ms": 1_700_000_001_000i64,
        },
        "intrinsics": {
            "source": { "kind": "placeholder" }, "width": 4, "height": 3,
            "fx": 1000.0, "fy": 1000.0, "cx": 2.0, "cy": 1.5,
            "distortion": { "model": "none" },
        },
        "ap_input": { "lat": 12.34, "lon": -56.78, "eye_height_m": 2.0,
                      "provenance": "operator_entered" },
        "gps_truth": { "lat": 41.5, "lon": -70.25, "lat_sigma_m": 5.0, "lon_sigma_m": 7.0,
                       "captured_unix_ms": 1_700_000_000_500i64, "source": "phone_gnss" },
    });
    std::fs::write(
        dir.join("bundle.json"),
        serde_json::to_vec_pretty(&bundle).unwrap(),
    )
    .unwrap();
}

// ---------------------------------------------------------------- assertions

/// Stored artifact validates (schema, checksum, manifest identity) and is
/// the unchanged engine bundle the session holds.
fn assert_stored_matches_session(sub: &Path, corpus: &Path, session: &str) -> PathBuf {
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(sub.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(
        manifest["device"]["uuid"], DEVICE,
        "manifest device identity"
    );
    let media = sub.join("media");
    let stored_bundle = std::fs::read(media.join("bundle.json")).unwrap();
    let bundle: serde_json::Value = serde_json::from_slice(&stored_bundle).unwrap();
    let bid = bundle["bundle_id"].as_str().unwrap();
    let original = std::fs::read(
        corpus
            .join("sessions")
            .join(session)
            .join("captures")
            .join(bid)
            .join("bundle.json"),
    )
    .unwrap();
    assert_eq!(stored_bundle, original, "engine bundle byte-identical");
    assert!((bundle["ap_input"]["lat"].as_f64().unwrap() - 12.34).abs() < 1e-9);
    assert!((bundle["gps_truth"]["lat"].as_f64().unwrap() - 41.5).abs() < 1e-9);
    assert!((manifest["gps"]["lat_deg"].as_f64().unwrap() - 41.5).abs() < 1e-9);
    let items = manifest["media"].as_array().unwrap();
    assert!(
        items.len() >= 5,
        "bundle + 2 frames + 2 sidecars: {items:?}"
    );
    for item in items {
        let name = item["filename"].as_str().unwrap();
        let bytes = std::fs::read(media.join(name)).unwrap();
        assert_eq!(
            item["checksum_sha256"].as_str().unwrap(),
            hex::encode(Sha256::digest(&bytes)),
            "checksum {name}"
        );
    }
    media
}

fn assert_replay_in_place(media: &Path) {
    let o = Command::new(bris_bin())
        .args([
            "replay",
            "--bundle",
            media.to_str().unwrap(),
            "--disable-store",
        ])
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(o.status.success(), "replay: {all}");
    assert!(all.contains("frames_pushed=2"), "replay: {all}");
}

// ---------------------------------------------------------------- tests

#[test]
fn default_off_completion_sends_nothing() {
    let data = tempfile::tempdir().unwrap();
    let token = register(data.path());
    let addr = free_addr();
    let _c = Collector::start(data.path(), addr);
    let f = Fixture::new();
    f.configure(&format!("http://{addr}"), &token, None);
    let id = f.session("off", 2);
    let o = finish(f.complete_cmd(&id).spawn().unwrap());
    assert!(o.status.success(), "{}", text(&o));
    f.ok(&["uploads", "retry"]);
    assert_eq!(f.status(), ("off".into(), 0, 0));
    assert!(
        stored(data.path()).is_empty(),
        "toggle OFF must send nothing"
    );
}

#[test]
fn toggle_on_silently_uploads_every_completed_session_and_replays_in_place() {
    let data = tempfile::tempdir().unwrap();
    let token = register(data.path());
    let addr = free_addr();
    let _c = Collector::start(data.path(), addr);
    let f = Fixture::new();
    f.configure(&format!("http://{addr}"), &token, Some(true));
    let mut sessions = Vec::new();
    for tag in ["s1", "s2"] {
        let id = f.session(tag, 2);
        let o = finish(f.complete_cmd(&id).spawn().unwrap());
        assert!(o.status.success(), "{}", text(&o));
        let out = String::from_utf8_lossy(&o.stdout).to_lowercase();
        assert!(
            !out.contains("review") && !out.contains("confirm"),
            "auto path must not show review: {out}"
        );
        sessions.push(id);
    }
    assert_eq!(f.status(), ("on".into(), 0, 0));
    let subs = stored(data.path());
    assert_eq!(subs.len(), 4, "every capture of every session uploaded");
    for sub in &subs {
        let ok = sessions.iter().any(|s| {
            std::panic::catch_unwind(|| assert_stored_matches_session(sub, &f.corpus, s)).is_ok()
        });
        assert!(ok, "{} does not match any completed session", sub.display());
    }
    let media = sub_media(&subs[0]);
    assert_replay_in_place(&media);
}

fn sub_media(sub: &Path) -> PathBuf {
    sub.join("media")
}

#[test]
fn offline_collector_leaves_visible_pending_and_restart_drains_durable_queue() {
    let data = tempfile::tempdir().unwrap();
    let token = register(data.path());
    let addr = free_addr(); // nothing listening: collector offline
    let f = Fixture::new();
    f.configure(&format!("http://{addr}"), &token, Some(true));
    let id = f.session("offline", 2);
    let _ = finish(f.complete_cmd(&id).spawn().unwrap());
    let (auto, pending, failed) = f.status();
    assert_eq!(auto, "on");
    assert_eq!(pending + failed, 2, "queued state visible while offline");
    assert_eq!(pending, 2, "connection refusal is retryable, not dead");

    // Collector comes up on the same address over the same durable root;
    // a fresh CLI process (restart) drains the persisted queue.
    {
        let _c = Collector::start(data.path(), addr);
        // The queue's backoff gate (5s base) applies; retry like the
        // bounded driver's next invocation would until drained.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            f.ok(&["uploads", "retry"]);
            if f.status() == ("on".into(), 0, 0) {
                break;
            }
            assert!(Instant::now() < deadline, "durable queue never drained");
            thread::sleep(Duration::from_millis(500));
        }
    }
    let subs = stored(data.path());
    assert_eq!(subs.len(), 2);
    // Collector restart: reopen the durable store and re-validate.
    let _c = Collector::start(data.path(), addr);
    for sub in &subs {
        assert_stored_matches_session(sub, &f.corpus, &id);
    }
    let store = Store::open(data.path()).unwrap();
    assert_eq!(store.rebuild_index().unwrap(), 2);
}

#[test]
fn toggle_off_revokes_queued_retry() {
    let data = tempfile::tempdir().unwrap();
    let token = register(data.path());
    let addr = free_addr();
    let f = Fixture::new();
    f.configure(&format!("http://{addr}"), &token, Some(true));
    let id = f.session("revoke", 1);
    let _ = finish(f.complete_cmd(&id).spawn().unwrap());
    assert_eq!(f.status().1, 1, "retry pending");
    f.ok(&["uploads", "set", "--automatic-upload", "false"]);
    let _c = Collector::start(data.path(), addr);
    // Wait past the backoff gate so the retry would have been eligible.
    thread::sleep(Duration::from_secs(6));
    f.ok(&["uploads", "retry"]);
    let again = f.session("revoke-later", 1);
    let o = finish(f.complete_cmd(&again).spawn().unwrap());
    assert!(o.status.success(), "{}", text(&o));
    assert_eq!(f.status().0, "off");
    assert!(
        stored(data.path()).is_empty(),
        "no request after toggle-off"
    );
}

/// Loopback relay in front of the real collector that holds each request
/// until released, so a transfer is genuinely in flight when the toggle
/// flips. Counts connections it observes.
struct HoldingRelay {
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
    forwarded: Arc<AtomicUsize>,
    release: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl HoldingRelay {
    fn new(upstream: SocketAddr) -> Self {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        l.set_nonblocking(true).unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let forwarded = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (c, fw, r, s) = (
            connections.clone(),
            forwarded.clone(),
            release.clone(),
            stop.clone(),
        );
        let handles = Arc::new(Mutex::new(Vec::new()));
        let worker = thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                match l.accept() {
                    Ok((client, _)) => {
                        c.fetch_add(1, Ordering::SeqCst);
                        let (fw, r, s) = (fw.clone(), r.clone(), s.clone());
                        handles.lock().unwrap().push(thread::spawn(move || {
                            relay(client, upstream, &fw, &r, &s);
                        }));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("relay accept: {e}"),
                }
            }
            for h in handles.lock().unwrap().drain(..) {
                let _ = h.join();
            }
        });
        Self {
            addr,
            connections,
            forwarded,
            release,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for HoldingRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.worker.take().unwrap().join();
    }
}

fn relay(
    mut client: TcpStream,
    upstream: SocketAddr,
    forwarded: &AtomicUsize,
    release: &AtomicBool,
    stop: &AtomicBool,
) {
    client.set_nonblocking(true).unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    // Hold: buffer everything; a client close/reset means it cancelled.
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if release.load(Ordering::SeqCst) {
            break;
        }
        match client.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return,
        }
    }
    forwarded.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut up) = TcpStream::connect(upstream) {
        let _ = up.write_all(&buf);
    }
}

#[test]
fn toggle_off_cancels_active_transfer_and_nothing_reaches_collector() {
    let data = tempfile::tempdir().unwrap();
    let token = register(data.path());
    let caddr = free_addr();
    let _c = Collector::start(data.path(), caddr);
    let relay = HoldingRelay::new(caddr);
    let f = Fixture::new();
    f.configure(&format!("http://{}", relay.addr), &token, Some(true));
    let id = f.session("active", 1);
    let child = f.complete_cmd(&id).spawn().unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while relay.connections.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "transfer never started");
        thread::sleep(Duration::from_millis(10));
    }
    // Transfer in flight: flip the toggle off. `set` acknowledges only after
    // the active owner has closed its socket.
    f.ok(&["uploads", "set", "--automatic-upload", "false"]);
    let _ = finish(child);
    let seen = relay.connections.load(Ordering::SeqCst);
    relay.release.store(true, Ordering::SeqCst);
    f.ok(&["uploads", "retry"]);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(f.status().0, "off");
    assert_eq!(
        relay.connections.load(Ordering::SeqCst),
        seen,
        "no new request after toggle-off"
    );
    assert_eq!(
        relay.forwarded.load(Ordering::SeqCst),
        0,
        "cancelled transfer"
    );
    assert!(stored(data.path()).is_empty());
}
