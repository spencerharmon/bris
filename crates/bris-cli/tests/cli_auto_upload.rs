//! Real CLI consent, session-completion, and durable-upload integration tests.
//! Queue entries are produced only by CLI commands, never by queue APIs or fixtures.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const TOKEN: &str = "cli-integration-test-token";
const DEVICE: &str = "2abdd6ad-ae5a-4cc7-844e-bf14084d5607";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(8);
// The CLI drain budget is two seconds; allow process startup/scheduling overhead.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(4);

struct Running(Option<Child>);

impl Running {
    fn start(command: &mut Command) -> Self {
        Self(Some(command.spawn().expect("spawn bris")))
    }

    fn finish(mut self, timeout: Duration) -> Output {
        let deadline = Instant::now() + timeout;
        loop {
            if self.0.as_mut().unwrap().try_wait().unwrap().is_some() {
                return self.0.take().unwrap().wait_with_output().unwrap();
            }
            if Instant::now() >= deadline {
                self.0.as_mut().unwrap().kill().unwrap();
                let output = self.0.take().unwrap().wait_with_output().unwrap();
                panic!("bris exceeded {timeout:?}: {}", output_text(&output));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            // Reap only the subprocess owned by this test, including on assertion failure.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn output_text(output: &Output) -> String {
    format!(
        "status={}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn success(output: &Output) {
    assert!(output.status.success(), "{}", output_text(output));
    assert!(
        !output_text(output).contains(TOKEN),
        "token leaked to CLI output"
    );
}

struct Request {
    stream: TcpStream,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl Request {
    fn read(mut stream: TcpStream) -> Self {
        stream.set_read_timeout(Some(COMMAND_TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(COMMAND_TIMEOUT)).unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0; 4096];
        let header_end = loop {
            if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            let n = stream.read(&mut chunk).expect("read HTTP headers");
            assert_ne!(n, 0, "client closed before HTTP headers");
            bytes.extend_from_slice(&chunk[..n]);
            assert!(bytes.len() < 1024 * 1024, "unbounded HTTP headers");
        };
        let head = std::str::from_utf8(&bytes[..header_end]).unwrap();
        let mut lines = head.split("\r\n");
        assert_eq!(lines.next(), Some("POST /v1/submissions HTTP/1.1"));
        let headers: BTreeMap<_, _> = lines
            .filter(|line| !line.is_empty())
            .map(|line| {
                let (name, value) = line.split_once(':').expect("HTTP header");
                (name.to_ascii_lowercase(), value.trim().to_owned())
            })
            .collect();
        let length: usize = headers["content-length"].parse().unwrap();
        assert!(length < 1024 * 1024, "unexpectedly large synthetic payload");
        while bytes.len() < header_end + length {
            let n = stream.read(&mut chunk).expect("read complete HTTP body");
            assert_ne!(n, 0, "truncated HTTP body");
            bytes.extend_from_slice(&chunk[..n]);
        }
        assert_eq!(bytes.len(), header_end + length);
        Self {
            stream,
            headers,
            body: bytes[header_end..].to_vec(),
        }
    }

    fn respond(mut self, status: u16) {
        let body = if status == 201 {
            r#"{"id":"01JCLIINTEGRATIONACCEPTED"}"#
        } else {
            r#"{"error":"controlled collector failure"}"#
        };
        write!(
            self.stream,
            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ).expect("respond to CLI upload");
    }

    fn parts(&self) -> BTreeMap<String, Vec<u8>> {
        assert_eq!(self.headers["authorization"], format!("Bearer {TOKEN}"));
        assert_eq!(self.headers["x-bris-device-uuid"], DEVICE);
        let content_type = &self.headers["content-type"];
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .expect("multipart content type")
            .trim_matches('"');
        let delimiter = format!("--{boundary}");
        // These zero-frame fixtures contain only UTF-8 JSON and logs; comparing the
        // extracted bytes still detects any reserialization or truncation.
        let body = std::str::from_utf8(&self.body).unwrap();
        assert!(body.starts_with(&format!("{delimiter}\r\n")));
        assert!(body.ends_with(&format!("{delimiter}--\r\n")));
        let mut parts = BTreeMap::new();
        for part in body.split(&delimiter).skip(1) {
            if part == "--\r\n" {
                break;
            }
            let (headers, bytes) = part
                .strip_prefix("\r\n")
                .unwrap()
                .split_once("\r\n\r\n")
                .unwrap();
            let disposition = headers
                .lines()
                .find(|line| line.starts_with("Content-Disposition:"))
                .unwrap();
            let name = disposition
                .split("; ")
                .find_map(|field| {
                    field
                        .strip_prefix("name=\"")
                        .and_then(|s| s.strip_suffix('"'))
                })
                .expect("multipart field name");
            assert!(parts
                .insert(
                    name.to_owned(),
                    bytes.strip_suffix("\r\n").unwrap().as_bytes().to_vec()
                )
                .is_none());
        }
        parts
    }

    fn assert_capture(&self, bundle: &[u8], log: &[u8]) {
        let parts = self.parts();
        assert_eq!(
            parts.len(),
            3,
            "manifest, bundle, and complete log required"
        );
        assert_eq!(parts["bundle.json"], bundle);
        assert_eq!(parts["pbris.log"], log);
        let manifest: Value = serde_json::from_slice(&parts["manifest"]).unwrap();
        let bundle: Value = serde_json::from_slice(bundle).unwrap();
        assert_eq!(manifest["device"]["uuid"], DEVICE);
        assert_eq!(manifest["gps"]["lat_deg"], bundle["gps_truth"]["lat"]);
        assert_eq!(manifest["gps"]["lon_deg"], bundle["gps_truth"]["lon"]);
        assert_ne!(bundle["ap_input"]["lat"], bundle["gps_truth"]["lat"]);
        assert_eq!(bundle["ap_input"]["provenance"], "operator_entered");
        let media = manifest["media"].as_array().unwrap();
        assert_eq!(media.len(), 2);
        let mut filenames: Vec<_> = media
            .iter()
            .map(|item| item["filename"].as_str().unwrap())
            .collect();
        filenames.sort_unstable();
        assert_eq!(filenames, ["bundle.json", "pbris.log"]);
        for item in media {
            let name = item["filename"].as_str().unwrap();
            let bytes = &parts[name];
            assert_eq!(item["size_bytes"].as_u64(), Some(bytes.len() as u64));
            assert_eq!(item["checksum_sha256"], hex::encode(Sha256::digest(bytes)));
            assert_eq!(
                item["role"],
                if name == "bundle.json" {
                    "bundle_manifest"
                } else {
                    "pbris_log"
                }
            );
        }
    }
}

struct Collector {
    url: String,
    requests: mpsc::Receiver<Request>,
    connections: Arc<AtomicUsize>,
    received: AtomicUsize,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Collector {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let (sender, requests) = mpsc::channel();
        let connections = Arc::new(AtomicUsize::new(0));
        let accepted = Arc::clone(&connections);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        accepted.fetch_add(1, Ordering::SeqCst);
                        if sender.send(Request::read(stream)).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept test upload: {error}"),
                }
            }
        });
        Self {
            url,
            requests,
            connections,
            received: AtomicUsize::new(0),
            stop,
            worker: Some(worker),
        }
    }

    fn receive(&self) -> Request {
        let request = self
            .requests
            .recv_timeout(COMMAND_TIMEOUT)
            .expect("CLI must POST a complete request");
        self.received.fetch_add(1, Ordering::SeqCst);
        request
    }

    fn assert_quiet(&self) {
        match self.requests.recv_timeout(Duration::from_millis(150)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => panic!("collector stopped: {error}"),
            Ok(_) => panic!("unexpected upload"),
        }
        assert_eq!(
            self.connections.load(Ordering::SeqCst),
            self.received.load(Ordering::SeqCst),
            "unexpected connection, including an incomplete upload"
        );
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let result = self.worker.take().unwrap().join();
        if !thread::panicking() {
            result.expect("collector worker panicked");
        }
    }
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
            queue: temp.path().join("upload queue"),
            temp,
        }
    }

    fn configure(&self, url: &str, automatic: Option<bool>, debug: Option<bool>) {
        let mut upload = toml::map::Map::new();
        upload.insert("drain_seconds".into(), toml::Value::Integer(2));
        for (key, value) in [
            ("collector_url", url),
            ("token", TOKEN),
            ("device_uuid", DEVICE),
            ("queue_root", self.queue.to_str().unwrap()),
        ] {
            upload.insert(key.to_owned(), toml::Value::String(value.to_owned()));
        }
        for (key, value) in [("automatic_upload", automatic), ("debug", debug)] {
            if let Some(value) = value {
                upload.insert(key.to_owned(), toml::Value::Boolean(value));
            }
        }
        let mut root = toml::map::Map::new();
        root.insert("upload".to_owned(), toml::Value::Table(upload));
        self.replace_config(&toml::to_string(&root).unwrap());
    }

    fn replace_config(&self, text: &str) {
        let staging = self.config.with_extension("tmp");
        fs::write(&staging, text).unwrap();
        fs::rename(staging, &self.config).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bris"));
        command
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .current_dir(self.temp.path())
            .env("HOME", self.temp.path())
            .env("XDG_CONFIG_HOME", self.temp.path())
            .env("XDG_DATA_HOME", self.temp.path())
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "info")
            .env("NO_PROXY", "127.0.0.1")
            .env("no_proxy", "127.0.0.1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("BRIS_") {
                command.env_remove(key);
            }
        }
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        Running::start(&mut self.command(args)).finish(COMMAND_TIMEOUT)
    }

    fn session(&self, captures: usize) -> (String, Vec<PathBuf>) {
        let output = Running::start(
            self.command(&["session", "new", "--title", "Upload integration"])
                .arg("--corpus")
                .arg(&self.corpus),
        )
        .finish(COMMAND_TIMEOUT);
        success(&output);
        let id = String::from_utf8(output.stdout).unwrap().trim().to_owned();
        uuid::Uuid::parse_str(&id).expect("session new prints only a UUID");
        let mut paths = Vec::new();
        for index in 0..captures {
            let capture_id = format!("synthetic-capture-{index}");
            let source = self.temp.path().join(&capture_id);
            write_capture(&source, &capture_id);
            let output = Running::start(
                self.command(&["session", "attach", "--session", &id])
                    .arg("--bundle")
                    .arg(&source)
                    .arg("--corpus")
                    .arg(&self.corpus),
            )
            .finish(COMMAND_TIMEOUT);
            success(&output);
            paths.push(
                self.corpus
                    .join("sessions")
                    .join(&id)
                    .join("captures")
                    .join(capture_id),
            );
        }
        (id, paths)
    }

    fn complete(&self, id: &str) -> Running {
        Running::start(
            self.command(&["session", "complete", "--session", id])
                .arg("--corpus")
                .arg(&self.corpus),
        )
    }

    fn status(&self, automatic: bool, pending: usize, failed: usize) {
        let output = self.run(&["uploads", "status"]);
        success(&output);
        let text = String::from_utf8(output.stdout).unwrap();
        for field in [
            format!("automatic_upload={}", if automatic { "on" } else { "off" }),
            format!("pending={pending}"),
            format!("failed={failed}"),
        ] {
            assert!(
                text.split_whitespace().any(|word| word == field),
                "missing {field}: {text}"
            );
        }
    }

    fn submit(&self, bundle: &Path, flags: &[&str]) -> Running {
        Running::start(
            self.command(&["submit"])
                .arg("--bundle")
                .arg(bundle)
                .args(flags),
        )
    }
}

fn write_capture(path: &Path, id: &str) {
    fs::create_dir_all(path.join("frames")).unwrap();
    let bundle = json!({
        "schema_version": 1,
        "bundle_id": id,
        "device": { "model": "synthetic-cli-test", "os": "test" },
        "capture": {
            "source_rotation_deg": 0, "frame_count": 0,
            "started_unix_ms": 1_700_000_000_000_i64,
            "ended_unix_ms": 1_700_000_001_000_i64
        },
        "intrinsics": {
            "source": { "kind": "placeholder" }, "width": 320, "height": 240,
            "fx": 1000.0, "fy": 1000.0, "cx": 160.0, "cy": 120.0,
            "distortion": { "model": "none" }
        },
        "ap_input": {
            "lat": 12.25, "lon": -45.5, "eye_height_m": 2.75,
            "provenance": "operator_entered"
        },
        "gps_truth": {
            "lat": 13.125, "lon": -44.875, "lat_sigma_m": 3.0, "lon_sigma_m": 4.0,
            "captured_unix_ms": 1_700_000_000_500_i64, "source": "synthetic_gnss"
        }
    });
    fs::write(
        path.join("bundle.json"),
        serde_json::to_vec_pretty(&bundle).unwrap(),
    )
    .unwrap();
    let sentence = format!("PBRIS,TEST,{id}");
    let checksum = sentence.bytes().fold(0_u8, |sum, byte| sum ^ byte);
    fs::write(
        path.join("pbris.log"),
        format!("${sentence}*{checksum:02X}\r\n"),
    )
    .unwrap();
}

#[test]
fn automatic_upload_is_off_by_default_and_attach_never_sends() {
    for configured_endpoint in [false, true] {
        let server = Collector::new();
        let fixture = Fixture::new();
        if configured_endpoint {
            fixture.configure(&server.url, None, None);
        } else {
            fixture.replace_config("");
        }
        let (id, _) = fixture.session(2);
        fixture.status(false, 0, 0);
        success(&fixture.complete(&id).finish(DRAIN_TIMEOUT));
        success(&fixture.complete(&id).finish(DRAIN_TIMEOUT));
        success(&fixture.run(&["uploads", "retry"]));
        fixture.status(false, 0, 0);
        server.assert_quiet();
        // Consent is not retrospective: completing again after enabling must
        // not upload a session whose completion happened while OFF.
        fixture.configure(&server.url, Some(true), Some(false));
        fixture.status(true, 0, 0);
        success(&fixture.run(&["uploads", "retry"]));
        success(&fixture.complete(&id).finish(DRAIN_TIMEOUT));
        fixture.status(true, 0, 0);
        server.assert_quiet();
    }
}

#[test]
fn enabled_completion_uploads_every_capture_without_debug_or_review_once() {
    let server = Collector::new();
    let fixture = Fixture::new();
    fixture.configure(&server.url, Some(true), None);
    let (id, captures) = fixture.session(2);
    let (_other_session, _) = fixture.session(1);
    // Neither another session nor an unregistered directory beside the attached
    // captures is part of this session's completion submission.
    let orphan = captures[0].parent().unwrap().join("unattached-capture");
    write_capture(&orphan, "unattached-capture");
    fixture.status(true, 0, 0);
    server.assert_quiet();
    let mut expected: Vec<_> = captures
        .iter()
        .map(|path| {
            (
                fs::read(path.join("bundle.json")).unwrap(),
                fs::read(path.join("pbris.log")).unwrap(),
            )
        })
        .collect();
    let child = fixture.complete(&id);
    for _ in 0..2 {
        let request = server.receive();
        let parts = request.parts();
        let index = expected
            .iter()
            .position(|(bundle, _)| *bundle == parts["bundle.json"])
            .expect("each attached capture must be sent exactly once");
        let (bundle, log) = expected.remove(index);
        request.assert_capture(&bundle, &log);
        request.respond(201);
    }
    let output = child.finish(DRAIN_TIMEOUT);
    success(&output);
    assert!(!output_text(&output).contains("Pre-upload review"));
    fixture.status(true, 0, 0);
    success(&fixture.complete(&id).finish(DRAIN_TIMEOUT));
    success(&fixture.run(&["uploads", "retry"]));
    fixture.status(true, 0, 0);
    server.assert_quiet();
}

#[test]
fn enabled_completion_requires_collector_token_and_device_credentials() {
    for field in ["collector_url", "token", "device_uuid"] {
        for empty in [false, true] {
            let server = Collector::new();
            let fixture = Fixture::new();
            fixture.configure(&server.url, Some(true), Some(false));
            let (id, _) = fixture.session(1);
            let mut config: toml::Value =
                toml::from_str(&fs::read_to_string(&fixture.config).unwrap()).unwrap();
            let upload = config["upload"].as_table_mut().unwrap();
            if empty {
                upload.insert(field.to_owned(), toml::Value::String(String::new()));
            } else {
                upload.remove(field);
            }
            fixture.replace_config(&toml::to_string(&config).unwrap());
            let output = fixture.complete(&id).finish(DRAIN_TIMEOUT);
            assert!(
                !output.status.success(),
                "completion accepted {field} (empty={empty}): {}",
                output_text(&output)
            );
            assert!(!output_text(&output).contains(TOKEN), "token leaked");
            server.assert_quiet();
            // Restoring credentials alone must not create a queued submission.
            fixture.configure(&server.url, Some(true), Some(false));
            fixture.status(true, 0, 0);
            success(&fixture.run(&["uploads", "retry"]));
            server.assert_quiet();
        }
    }
}

#[test]
fn invalid_or_missing_configuration_fails_closed_at_completion() {
    for invalid in ["missing", "malformed", "missing-token", "wrong-type"] {
        let server = Collector::new();
        let fixture = Fixture::new();
        fixture.configure(&server.url, Some(true), None);
        let (id, _) = fixture.session(1);
        match invalid {
            "missing" => fs::remove_file(&fixture.config).unwrap(),
            "malformed" => fixture.replace_config("[upload\nautomatic_upload = true"),
            "missing-token" | "wrong-type" => {
                let mut config: toml::Value =
                    toml::from_str(&fs::read_to_string(&fixture.config).unwrap()).unwrap();
                if invalid == "missing-token" {
                    config["upload"].as_table_mut().unwrap().remove("token");
                } else {
                    config["upload"]["automatic_upload"] = toml::Value::String("true".into());
                }
                fixture.replace_config(&toml::to_string(&config).unwrap());
            }
            _ => unreachable!(),
        }
        // Fail-closed may report an error or complete locally, but must never send.
        fixture.complete(&id).finish(DRAIN_TIMEOUT);
        fixture.run(&["uploads", "retry"]);
        server.assert_quiet();
        fixture.configure(&server.url, Some(true), None);
        fixture.status(true, 0, 0);
        success(&fixture.run(&["uploads", "retry"]));
        server.assert_quiet();
    }
}

#[test]
fn retry_after_restart_uses_persisted_snapshot_and_does_not_drain_manual_queue() {
    let server = Collector::new();
    let fixture = Fixture::new();
    fixture.configure(&server.url, Some(true), None);
    let manual = fixture.temp.path().join("manual");
    write_capture(&manual, "manual-only");
    success(
        &fixture
            .submit(&manual, &["--debug", "--yes", "--enqueue-only"])
            .finish(COMMAND_TIMEOUT),
    );
    let (id, captures) = fixture.session(1);
    let bundle = fs::read(captures[0].join("bundle.json")).unwrap();
    let log = fs::read(captures[0].join("pbris.log")).unwrap();
    let child = fixture.complete(&id);
    let request = server.receive();
    request.assert_capture(&bundle, &log);
    request.respond(503);
    success(&child.finish(DRAIN_TIMEOUT));
    let retry_started = Instant::now();
    success(&Running::start(&mut fixture.command(&["uploads", "retry"])).finish(DRAIN_TIMEOUT));
    assert!(retry_started.elapsed() < DRAIN_TIMEOUT);
    server.assert_quiet();
    fixture.status(true, 1, 0);
    success(&fixture.complete(&id).finish(DRAIN_TIMEOUT));
    fixture.status(true, 1, 0);
    // The restart must not rebuild its payload from mutable capture artifacts.
    fs::remove_dir_all(&fixture.corpus).unwrap();
    fixture.status(true, 1, 0);
    server.assert_quiet();
    thread::sleep(Duration::from_millis(5200));
    fixture.status(true, 1, 0); // Status must not send even when the retry is due.
    server.assert_quiet();
    let child = Running::start(&mut fixture.command(&["uploads", "retry"]));
    let request = server.receive();
    request.assert_capture(&bundle, &log);
    request.respond(201);
    success(&child.finish(DRAIN_TIMEOUT));
    fixture.status(true, 0, 0);
    success(&fixture.run(&["uploads", "retry"]));
    server.assert_quiet();
    // Reading the documented shared queue layout is observational only.
    assert_eq!(
        fs::read_dir(fixture.queue.join("pending")).unwrap().count(),
        1
    );
    assert!(fixture.queue.join("automatic").is_dir());
}

#[test]
fn permanent_failure_survives_restart_and_is_not_reenqueued() {
    let server = Collector::new();
    let fixture = Fixture::new();
    fixture.configure(&server.url, Some(true), Some(false));
    let (id, _) = fixture.session(1);
    let child = fixture.complete(&id);
    server.receive().respond(400);
    child.finish(DRAIN_TIMEOUT);
    fixture.status(true, 0, 1);
    fixture.complete(&id).finish(DRAIN_TIMEOUT);
    fixture.run(&["uploads", "retry"]);
    fixture.status(true, 0, 1);
    server.assert_quiet();
}

#[test]
fn config_changes_cancel_active_http_and_revoke_remaining_captures() {
    for change in [
        "off",
        "removed",
        "invalid",
        "destination",
        "token",
        "device",
        "queue-root",
    ] {
        let server = Collector::new();
        let other_server = Collector::new();
        let fixture = Fixture::new();
        fixture.configure(&server.url, Some(true), None);
        let (id, _) = fixture.session(2);
        let child = fixture.complete(&id);
        let mut held = server.receive();
        // No response is sent: cancellation must close the actual HTTP connection,
        // rather than merely prevent the next queue entry after a response arrives.
        let changed_at = Instant::now();
        match change {
            "off" => fixture.configure(&server.url, Some(false), None),
            "removed" => fs::remove_file(&fixture.config).unwrap(),
            "invalid" => fixture.replace_config("[upload\n"),
            "destination" => fixture.configure(&other_server.url, Some(true), None),
            "token" | "device" => {
                let text = fs::read_to_string(&fixture.config).unwrap();
                let text = if change == "token" {
                    text.replace(TOKEN, "replacement-test-token")
                } else {
                    text.replace(DEVICE, "75defe26-255e-4915-a35d-53eb35cd5264")
                };
                fixture.replace_config(&text);
            }
            "queue-root" => {
                let mut config: toml::Value =
                    toml::from_str(&fs::read_to_string(&fixture.config).unwrap()).unwrap();
                config["upload"]["queue_root"] = toml::Value::String(
                    fixture
                        .temp
                        .path()
                        .join("other queue")
                        .to_str()
                        .unwrap()
                        .to_owned(),
                );
                fixture.replace_config(&toml::to_string(&config).unwrap());
            }
            _ => unreachable!(),
        }
        held.stream
            .set_read_timeout(Some(Duration::from_millis(1500)))
            .unwrap();
        let mut byte = [0];
        match held.stream.read(&mut byte) {
            Ok(0) => {}
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            result => panic!("{change}: active HTTP was not cancelled: {result:?}"),
        }
        assert!(
            changed_at.elapsed() < Duration::from_millis(1500),
            "{change}: cancellation waited for drain deadline"
        );
        child.finish(DRAIN_TIMEOUT);
        server.assert_quiet();
        other_server.assert_quiet();
        // A later explicit ON must not resurrect the revoked generation.
        fixture.configure(&server.url, Some(true), None);
        fixture.run(&["uploads", "retry"]);
        fixture.complete(&id).finish(DRAIN_TIMEOUT);
        server.assert_quiet();
        other_server.assert_quiet();
    }
}

#[test]
fn completion_and_retry_are_bounded_while_collector_holds_response() {
    let server = Collector::new();
    let fixture = Fixture::new();
    fixture.configure(&server.url, Some(true), None);
    let (id, _) = fixture.session(1);
    let started = Instant::now();
    let child = fixture.complete(&id);
    let held = server.receive();
    success(&child.finish(DRAIN_TIMEOUT));
    assert!(started.elapsed() < DRAIN_TIMEOUT);
    drop(held);
    fixture.status(true, 1, 0);
    let started = Instant::now();
    let child = Running::start(&mut fixture.command(&["uploads", "retry"]));
    let held = server.receive();
    success(&child.finish(DRAIN_TIMEOUT));
    assert!(started.elapsed() < DRAIN_TIMEOUT);
    drop(held);
    fixture.status(true, 1, 0);
}

#[test]
fn persisted_toggle_revokes_idle_retries_and_can_be_reenabled() {
    let server = Collector::new();
    let fixture = Fixture::new();
    fixture.configure(&server.url, Some(true), None);
    let (id, _) = fixture.session(1);
    let child = fixture.complete(&id);
    server.receive().respond(503);
    success(&child.finish(DRAIN_TIMEOUT));
    fixture.status(true, 1, 0);
    success(&fixture.run(&["uploads", "set", "--automatic-upload", "false"]));
    fixture.status(false, 0, 1);
    success(&fixture.run(&["uploads", "set", "--automatic-upload", "true"]));
    success(&fixture.run(&["uploads", "retry"]));
    success(&fixture.complete(&id).finish(DRAIN_TIMEOUT));
    server.assert_quiet();
    let (new_id, _) = fixture.session(1);
    let child = fixture.complete(&new_id);
    server.receive().respond(201);
    success(&child.finish(DRAIN_TIMEOUT));
    fixture.status(true, 0, 1);
}

#[test]
fn toggle_off_acknowledges_active_cancellation_and_concurrent_retry_cannot_send() {
    let server = Collector::new();
    let fixture = Fixture::new();
    fixture.configure(&server.url, Some(true), None);
    let (id, _) = fixture.session(2);
    let child = fixture.complete(&id);
    let mut held = server.receive();
    let concurrent = fixture.run(&["uploads", "retry"]);
    assert!(!concurrent.status.success());
    success(&fixture.run(&["uploads", "set", "--automatic-upload", "false"]));
    held.stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut byte = [0];
    match held.stream.read(&mut byte) {
        Ok(0) => {}
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => panic!("OFF returned without cancelling HTTP: {other:?}"),
    }
    success(&child.finish(DRAIN_TIMEOUT));
    fixture.status(false, 0, 2);
    server.assert_quiet();
}

#[test]
fn configurable_budget_allows_slow_collector_and_crash_receipt_recovery() {
    let server = Collector::new();
    let fixture = Fixture::new();
    fixture.configure(&server.url, Some(true), None);
    let config = fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("drain_seconds = 2", "drain_seconds = 5");
    fixture.replace_config(&config);
    let (id, _) = fixture.session(1);
    let child = fixture.complete(&id);
    let request = server.receive();
    thread::sleep(Duration::from_millis(2200));
    request.respond(201);
    success(&child.finish(COMMAND_TIMEOUT));
    // Simulate interruption between queue persistence and completion receipt:
    // the committed queue audit remains authoritative, not a second enqueue.
    fs::remove_file(
        fixture
            .corpus
            .join("sessions")
            .join(&id)
            .join("completed-captures.json"),
    )
    .unwrap();
    success(&fixture.complete(&id).finish(DRAIN_TIMEOUT));
    fixture.status(true, 0, 0);
    server.assert_quiet();
}

#[test]
fn manual_submit_requires_both_debug_and_explicit_review_confirmation() {
    for debug_from_config in [false, true] {
        let server = Collector::new();
        let fixture = Fixture::new();
        fixture.configure(&server.url, Some(!debug_from_config), None);
        let bundle = fixture.temp.path().join("manual");
        write_capture(&bundle, "manual-debug-gate");
        let denied = fixture.submit(&bundle, &["--yes"]).finish(COMMAND_TIMEOUT);
        assert!(!denied.status.success(), "Debug-off manual upload accepted");
        assert!(
            output_text(&denied).to_ascii_lowercase().contains("debug"),
            "{}",
            output_text(&denied)
        );
        server.assert_quiet();
        if debug_from_config {
            fixture.configure(&server.url, Some(false), Some(true));
        }
        let debug_flags = if debug_from_config {
            vec![]
        } else {
            vec!["--debug"]
        };
        let review = fixture
            .submit(&bundle, &debug_flags)
            .finish(COMMAND_TIMEOUT);
        success(&review);
        let text = output_text(&review);
        for detail in [
            "Pre-upload review",
            "GPS",
            "13.125",
            "-44.875",
            "bundle.json",
        ] {
            assert!(
                text.contains(detail),
                "missing review detail {detail}: {text}"
            );
        }
        server.assert_quiet();
        let mut approved_flags = debug_flags;
        approved_flags.push("--yes");
        let child = fixture.submit(&bundle, &approved_flags);
        let request = server.receive();
        request.assert_capture(
            &fs::read(bundle.join("bundle.json")).unwrap(),
            &fs::read(bundle.join("pbris.log")).unwrap(),
        );
        request.respond(201);
        success(&child.finish(COMMAND_TIMEOUT));
        server.assert_quiet();
    }
}

#[test]
fn manual_flush_cannot_bypass_debug_or_yes_and_automatic_retry_leaves_it_alone() {
    let server = Collector::new();
    let fixture = Fixture::new();
    fixture.configure(&server.url, Some(true), None);
    let bundle = fixture.temp.path().join("manual-flush");
    write_capture(&bundle, "manual-flush-gate");
    success(
        &fixture
            .submit(&bundle, &["--debug", "--yes", "--enqueue-only"])
            .finish(COMMAND_TIMEOUT),
    );
    fixture.status(true, 0, 0);
    for flags in [
        vec!["--flush-only"],
        vec!["--flush-only", "--debug"],
        vec!["--flush-only", "--yes"],
    ] {
        fixture.submit(&bundle, &flags).finish(COMMAND_TIMEOUT);
        server.assert_quiet();
    }
    success(&fixture.run(&["uploads", "retry"]));
    fixture.status(true, 0, 0);
    server.assert_quiet();
    let child = fixture.submit(&bundle, &["--flush-only", "--debug", "--yes"]);
    let request = server.receive();
    request.assert_capture(
        &fs::read(bundle.join("bundle.json")).unwrap(),
        &fs::read(bundle.join("pbris.log")).unwrap(),
    );
    request.respond(201);
    success(&child.finish(COMMAND_TIMEOUT));
    server.assert_quiet();
}
