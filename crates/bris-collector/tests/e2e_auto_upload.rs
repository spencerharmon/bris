//! `auto-upload-collection-acceptance` — ROI rule 1b end to end.
//!
//! Unlike `e2e_collection_path` (which hands an approved review to the
//! transport directly), this test never calls a submit/approve API. It drives
//! the SHIPPING CLI frontend's real completion hook (`bris session complete`)
//! and its real toggle / retry / status verbs as subprocesses, against a REAL
//! `bris-collector` (axum router on a loopback TCP port) backed by a temporary
//! durable store with a per-device token issued by the collector itself:
//!
//!   settings toggle -> `bris session complete` -> persistent submit queue ->
//!   HTTP POST (per-device bearer) -> collector durable store + index ->
//!   `bris replay` IN PLACE on the stored media.
//!
//! It asserts: toggle OFF (default) sends nothing; toggle ON sends every
//! completed capture silently (no review UI, no Debug flag) as the unchanged
//! bris-bundle v1 artifact (schema, checksums, manifest identity, byte-equal
//! bundle.json, replayable in place); an offline collector leaves the entry
//! visibly pending and a later retry after BOTH the CLI and the collector
//! restart delivers it; a permanent rejection is visibly failed; toggle OFF
//! revokes a queued retry (no new requests) and cancels an active transfer.

use std::net::{SocketAddr, TcpListener as StdListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

use bris_collector::routes::AppState;
use bris_collector::store::Store;
use bris_collector::{build_app, Config};
use sha2::{Digest, Sha256};

const DEVICE: &str = "6f1d2b9e-1c55-4f3b-9a47-3e0c5d7a2b11";
/// Queue base backoff is 5 s; wait just past it before an explicit retry.
const BACKOFF: Duration = Duration::from_millis(5300);

// ---------------------------------------------------------------- helpers

/// Build (once) and locate the real `bris` CLI binary via cargo's own
/// artifact report, honouring whatever target dir cargo is configured with.
fn bris_bin() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../Cargo.toml")
            .canonicalize()
            .unwrap();
        let out = Command::new(env!("CARGO"))
            .args(["build", "--quiet", "--message-format=json", "-p", "bris-cli"])
            .args(["--bin", "bris", "--manifest-path"])
            .arg(&manifest)
            .stderr(Stdio::inherit())
            .output()
            .expect("cargo build bris");
        assert!(out.status.success(), "building bris failed");
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["reason"] == "compiler-artifact" && v["target"]["name"] == "bris")
            .find_map(|v| v["executable"].as_str().map(PathBuf::from))
            .expect("bris executable artifact")
    })
}

fn text(o: &Output) -> String {
    format!(
        "status={}\nstdout={}\nstderr={}",
        o.status,
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// A real collector over a durable data root, restartable on the same port.
struct LiveCollector {
    data_root: PathBuf,
    addr: SocketAddr,
    posts: Arc<AtomicUsize>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    state: Option<Arc<AppState>>,
}

impl LiveCollector {
    fn new(data_root: &Path) -> Self {
        // Reserve a port; the collector is started (and restarted) on it later.
        let addr = StdListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        Self {
            data_root: data_root.to_path_buf(),
            addr,
            posts: Arc::new(AtomicUsize::new(0)),
            shutdown: None,
            task: None,
            state: None,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Issue a per-device token from the collector's own durable registry.
    fn register(&self, device: &str) -> String {
        Store::open(&self.data_root)
            .unwrap()
            .register_device(device)
            .unwrap()
    }

    async fn start(&mut self) {
        assert!(self.task.is_none());
        let cfg = Config {
            data_root: self.data_root.clone(),
            bind: self.addr.to_string(),
            bearer_token: "admin-only-never-given-to-the-device".to_owned(),
            max_submission_bytes: 32 * 1024 * 1024,
            retention_days: 30,
        };
        let store = Store::open(&cfg.data_root).unwrap();
        let state = Arc::new(AppState { config: cfg, store });
        let posts = Arc::clone(&self.posts);
        let app = build_app(Arc::clone(&state)).layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                if req.uri().path() == "/v1/submissions" && req.method() == "POST" {
                    posts.fetch_add(1, Ordering::SeqCst);
                }
                async move { next.run(req).await }
            },
        ));
        let listener = tokio::net::TcpListener::bind(self.addr).await.unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        self.task = Some(tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        }));
        self.shutdown = Some(tx);
        self.state = Some(state);
    }

    async fn stop(&mut self) {
        let _ = self.shutdown.take().unwrap().send(());
        self.task.take().unwrap().await.unwrap();
        self.state = None;
    }

    fn posts(&self) -> usize {
        self.posts.load(Ordering::SeqCst)
    }

    /// Submission ids in the durable index (fresh connection: reflects disk).
    fn stored_ids(&self) -> Vec<String> {
        let store = Store::open(&self.data_root).unwrap();
        let conn = store.lock_index();
        let mut stmt = conn
            .prepare("SELECT id FROM submissions ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }
}

/// One CLI install: config file, corpus, queue — all temporary.
struct Cli {
    temp: tempfile::TempDir,
}

impl Cli {
    fn new() -> Self {
        Self {
            temp: tempfile::tempdir().unwrap(),
        }
    }
    fn config(&self) -> PathBuf {
        self.temp.path().join("bris.toml")
    }
    fn corpus(&self) -> PathBuf {
        self.temp.path().join("corpus")
    }

    /// Write `[upload]`. `automatic` None = key absent (the default).
    fn configure(&self, url: &str, token: &str, automatic: Option<bool>) {
        let mut up = toml::map::Map::new();
        up.insert("collector_url".into(), url.into());
        up.insert("token".into(), token.into());
        up.insert("device_uuid".into(), DEVICE.into());
        up.insert(
            "queue_root".into(),
            self.temp.path().join("queue").to_str().unwrap().into(),
        );
        up.insert("drain_seconds".into(), toml::Value::Integer(3));
        if let Some(a) = automatic {
            up.insert("automatic_upload".into(), a.into());
        }
        let mut root = toml::map::Map::new();
        root.insert("upload".into(), toml::Value::Table(up));
        let tmp = self.config().with_extension("tmp");
        std::fs::write(&tmp, toml::to_string(&root).unwrap()).unwrap();
        std::fs::rename(tmp, self.config()).unwrap();
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(bris_bin());
        c.arg("--config")
            .arg(self.config())
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

    fn ok(&self, args: &[&str]) -> Output {
        let out = self.cmd(args).output().unwrap();
        assert!(out.status.success(), "bris {args:?}: {}", text(&out));
        out
    }

    /// Create a session and attach `n` real two-frame captures through the
    /// CLI. Returns (session id, attached capture dirs).
    fn session(&self, tag: &str, n: usize) -> (String, Vec<PathBuf>) {
        let corpus = self.corpus();
        let c = corpus.to_str().unwrap();
        let out = self.ok(&["session", "new", "--title", tag, "--corpus", c]);
        let id = String::from_utf8(out.stdout).unwrap().trim().to_owned();
        let mut dirs = Vec::new();
        for i in 0..n {
            let cap = format!("{tag}-cap-{i}");
            let src = self.temp.path().join("incoming").join(&cap);
            make_capture(&src, &cap);
            let s = src.to_str().unwrap();
            self.ok(&["session", "attach", "--session", &id, "--bundle", s, "--corpus", c]);
            dirs.push(corpus.join("sessions").join(&id).join("captures").join(cap));
        }
        (id, dirs)
    }

    /// The frontend's completion hook — the ONLY path that enqueues.
    fn complete(&self, id: &str) -> Output {
        let c = self.corpus();
        let out = self.ok(&["session", "complete", "--session", id, "--corpus", c.to_str().unwrap()]);
        let all = text(&out);
        assert!(
            !all.contains("Pre-upload review"),
            "automatic path must never show the review UI: {all}"
        );
        out
    }

    /// Visible queue state through the CLI's status verb.
    fn status(&self) -> (bool, usize, usize) {
        let out = self.ok(&["uploads", "status"]);
        let s = String::from_utf8(out.stdout).unwrap();
        let field = |k: &str| {
            s.split_whitespace()
                .find_map(|w| w.strip_prefix(&format!("{k}=")).map(str::to_owned))
                .unwrap_or_else(|| panic!("status missing {k}: {s}"))
        };
        (
            field("automatic_upload") == "on",
            field("pending").parse().unwrap(),
            field("failed").parse().unwrap(),
        )
    }
}

fn write_pgm(path: &Path, fill: u8) {
    let mut bytes = b"P5\n4 3\n255\n".to_vec();
    bytes.extend(std::iter::repeat_n(fill, 12));
    std::fs::write(path, bytes).unwrap();
}

/// A real bris-bundle v1 capture: two frames with sidecars, honest
/// `ap_input` and a distinct `gps_truth`.
fn make_capture(dir: &Path, id: &str) {
    let frames = dir.join("frames");
    std::fs::create_dir_all(&frames).unwrap();
    for seq in 0..2u8 {
        let stem = format!("{seq:08}");
        write_pgm(&frames.join(format!("{stem}.pgm")), seq * 40);
        let side = serde_json::json!({
            "seq": seq, "width": 4, "height": 3,
            "captured_unix_ms": 1_700_000_000_000i64 + i64::from(seq) * 1000,
        });
        std::fs::write(frames.join(format!("{stem}.json")), side.to_string()).unwrap();
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
        "gps_truth": { "lat": 41.5, "lon": -70.25, "lat_sigma_m": 5.0,
                       "lon_sigma_m": 7.0, "captured_unix_ms": 1_700_000_000_500i64,
                       "source": "phone_gnss" },
    });
    std::fs::write(dir.join("bundle.json"), bundle.to_string()).unwrap();
}

/// Locate a stored submission dir by id under `submissions/<y>/<m>/<d>/`.
fn submission_dir(data_root: &Path, id: &str) -> PathBuf {
    let mut stack = vec![data_root.join("submissions")];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if !p.is_dir() || p.file_name().unwrap() == ".staging" {
                continue;
            }
            if p.file_name().unwrap() == id {
                return p;
            }
            stack.push(p);
        }
    }
    panic!("submission {id} not on disk");
}

/// Schema, manifest identity, byte-identical bundle.json, every declared
/// checksum, and in-place replay of the collector's stored copy.
fn assert_stored_artifact(data_root: &Path, id: &str, capture: &Path) {
    let sub = submission_dir(data_root, id);
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(sub.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(manifest["device"]["uuid"], DEVICE, "manifest identity");
    let media = sub.join("media");
    let shipped = std::fs::read(capture.join("bundle.json")).unwrap();
    assert_eq!(
        std::fs::read(media.join("bundle.json")).unwrap(),
        shipped,
        "engine bundle.json must arrive unchanged"
    );
    let bundle: serde_json::Value = serde_json::from_slice(&shipped).unwrap();
    assert_eq!(bundle["schema_version"], 1);
    assert_eq!(manifest["gps"]["lat_deg"], bundle["gps_truth"]["lat"]);
    assert_ne!(bundle["ap_input"]["lat"], bundle["gps_truth"]["lat"]);
    let items = manifest["media"].as_array().unwrap();
    let names: Vec<_> = items.iter().map(|m| m["filename"].as_str().unwrap()).collect();
    for f in ["bundle.json", "00000000.pgm", "00000001.pgm"] {
        assert!(
            names.iter().any(|n| n.ends_with(f)),
            "{f} missing from {names:?}"
        );
    }
    for m in items {
        let name = m["filename"].as_str().unwrap();
        let bytes = std::fs::read(media.join(name)).unwrap();
        assert_eq!(m["checksum_sha256"], hex::encode(Sha256::digest(&bytes)));
    }
    let out = Command::new(bris_bin())
        .args(["replay", "--bundle"])
        .arg(&media)
        .arg("--disable-store")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let all = text(&out);
    assert!(out.status.success(), "replay in place failed: {all}");
    assert!(all.contains("frames_pushed=2"), "{all}");
    assert!(all.contains("mode complete"), "{all}");
}

// ------------------------------------------------------------------ tests

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn toggle_off_by_default_sends_nothing_on_completion() {
    let store = tempfile::tempdir().unwrap();
    let mut col = LiveCollector::new(store.path());
    let token = col.register(DEVICE);
    col.start().await;
    let cli = Cli::new();
    cli.configure(&col.url(), &token, None);
    let (id, _) = cli.session("off", 2);
    cli.complete(&id);
    cli.ok(&["uploads", "retry"]);
    assert_eq!(cli.status(), (false, 0, 0));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(col.posts(), 0, "default OFF must never POST");
    assert!(col.stored_ids().is_empty());
    col.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn toggle_on_uploads_every_completed_capture_and_survives_offline_restart() {
    let store = tempfile::tempdir().unwrap();
    let mut col = LiveCollector::new(store.path());
    let token = col.register(DEVICE);
    col.start().await;
    let cli = Cli::new();
    // ON with no Debug key and no review/approval step.
    cli.configure(&col.url(), &token, Some(true));

    // ---- 1. Online: completion silently delivers every capture. ----
    let (id, caps) = cli.session("online", 2);
    cli.complete(&id);
    assert_eq!(cli.status(), (true, 0, 0));
    assert_eq!(col.posts(), 2);
    let ids = col.stored_ids();
    assert_eq!(ids.len(), 2, "both captures received over HTTP");
    for cap in &caps {
        let want = std::fs::read(cap.join("bundle.json")).unwrap();
        let id = ids
            .iter()
            .find(|sid| {
                std::fs::read(submission_dir(store.path(), sid).join("media/bundle.json"))
                    .unwrap()
                    == want
            })
            .expect("capture stored");
        assert_stored_artifact(store.path(), id, cap);
    }
    // Re-completing the same session is idempotent.
    cli.complete(&id);
    assert_eq!(col.posts(), 2);

    // ---- 2. Collector offline: entry is visibly pending, nothing lost. ----
    col.stop().await;
    let (id2, caps2) = cli.session("offline", 1);
    cli.complete(&id2);
    assert_eq!(cli.status(), (true, 1, 0), "offline capture visibly pending");

    // ---- 3. Restart the collector over the SAME durable store; the
    // device token must still authenticate. A fresh CLI process retries. ----
    col.start().await;
    std::thread::sleep(BACKOFF);
    cli.ok(&["uploads", "retry"]);
    assert_eq!(cli.status(), (true, 0, 0));
    assert_eq!(col.posts(), 3);
    let ids = col.stored_ids();
    assert_eq!(ids.len(), 3, "durable store kept prior submissions + retry");
    let want = std::fs::read(caps2[0].join("bundle.json")).unwrap();
    let rid = ids
        .iter()
        .find(|sid| {
            std::fs::read(submission_dir(store.path(), sid).join("media/bundle.json")).unwrap()
                == want
        })
        .expect("retried capture stored");
    assert_stored_artifact(store.path(), rid, &caps2[0]);
    // The index rebuilt from disk agrees with the live view.
    assert_eq!(Store::open(store.path()).unwrap().rebuild_index().unwrap(), 3);
    col.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthenticated_device_is_visibly_failed_not_stored() {
    let store = tempfile::tempdir().unwrap();
    let mut col = LiveCollector::new(store.path());
    col.register(DEVICE);
    col.start().await;
    let cli = Cli::new();
    cli.configure(&col.url(), "not-the-issued-device-token", Some(true));
    let (id, _) = cli.session("badtoken", 1);
    cli.complete(&id);
    assert_eq!(col.posts(), 1);
    assert!(col.stored_ids().is_empty(), "per-device auth must reject");
    assert_eq!(cli.status(), (true, 0, 1), "rejection visibly failed");
    col.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn toggle_off_revokes_queued_retry_no_new_requests() {
    let store = tempfile::tempdir().unwrap();
    let mut col = LiveCollector::new(store.path());
    let token = col.register(DEVICE);
    let cli = Cli::new();
    cli.configure(&col.url(), &token, Some(true));
    // Collector offline: completion queues a retryable entry.
    let (id, _) = cli.session("revoke", 1);
    cli.complete(&id);
    assert_eq!(cli.status(), (true, 1, 0));
    // Toggle OFF through the shipping settings verb.
    cli.ok(&["uploads", "set", "--automatic-upload", "false"]);
    let (on, pending, _) = cli.status();
    assert!(!on);
    assert_eq!(pending, 0, "revoked entry is no longer pending");
    // Collector comes up and the retry would be due — nothing may be sent.
    col.start().await;
    std::thread::sleep(BACKOFF);
    cli.ok(&["uploads", "retry"]);
    cli.complete(&id);
    let (id2, _) = cli.session("revoke-after", 1);
    cli.complete(&id2);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(col.posts(), 0, "no requests after toggle-off");
    assert!(col.stored_ids().is_empty());
    col.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn toggle_off_cancels_active_transfer() {
    let store = tempfile::tempdir().unwrap();
    let mut col = LiveCollector::new(store.path());
    let token = col.register(DEVICE);
    col.start().await;
    // A holding hop in front of the collector: accepts the CLI's
    // connection but never forwards, so the transfer is in flight.
    let hold = StdListener::bind("127.0.0.1:0").unwrap();
    let hold_url = format!("http://{}", hold.local_addr().unwrap());
    let (tx, rx) = mpsc::channel::<TcpStream>();
    std::thread::spawn(move || {
        while let Ok((s, _)) = hold.accept() {
            if tx.send(s).is_err() {
                break;
            }
        }
    });
    let cli = Cli::new();
    cli.configure(&hold_url, &token, Some(true));
    let (id, _) = cli.session("cancel", 2);
    let corpus = cli.corpus();
    let mut child = cli
        .cmd(&["session", "complete", "--session", &id, "--corpus"])
        .arg(&corpus)
        .spawn()
        .unwrap();
    let mut held = rx.recv_timeout(Duration::from_secs(10)).expect("transfer started");
    cli.ok(&["uploads", "set", "--automatic-upload", "false"]);
    held.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let deadline = Instant::now() + Duration::from_millis(1500);
    let mut buf = [0u8; 8192];
    loop {
        use std::io::Read;
        match held.read(&mut buf) {
            Ok(0) => break,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => panic!("{e}"),
        }
        assert!(Instant::now() < deadline, "active transfer not cancelled by toggle-off");
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "completion did not stop");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(cli.status(), (false, 0, 2));
    // Point back at the real collector: still nothing may be sent.
    cli.configure(&col.url(), &token, Some(false));
    cli.ok(&["uploads", "retry"]);
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "no new connection after toggle-off"
    );
    assert_eq!(col.posts(), 0);
    col.stop().await;
}
