//! NMEA 0183 transport sinks for the CLI's `bris serve`.
//!
//! Drains the engine's [`bris_streaming::FixReceiver`],
//! formats each [`bris_streaming::PublishedFix`] into NMEA
//! sentences via [`bris_streaming::format_fix_as_nmea`], and
//! writes the bytes to one or more transport sinks
//! (stdout, TCP server).
//!
//! # Sink types
//!
//! - **Stdout** ([`StdoutSink`]): writes each sentence batch
//!   to the process's stdout. Useful for piping into another
//!   tool (e.g. `bris serve | gpsd`) and for low-overhead
//!   debugging without configuring a network listener.
//! - **TCP server** ([`TcpServerSink`]): binds a TCP
//!   listener, accepts incoming chartplotter connections,
//!   broadcasts each NMEA batch to every connected client.
//!   Per the NMEA-over-IP convention, the standard port is
//!   10110 (`OpenCPN`, `MaxSea`, Coastal Explorer all default
//!   here).
//!
//! Multiple sinks can be active simultaneously; the dispatch
//! loop fans each fix out to all of them. A sink that errors
//! on write logs and continues; one bad sink doesn't take
//! down the others.
//!
//! # Threading
//!
//! [`run_nmea_dispatch`] runs on the calling thread and
//! blocks on the fix channel. The TCP server runs its accept
//! loop on a separate background thread it spawns
//! internally; clients are owned by a `Mutex<Vec<TcpStream>>`
//! shared between the accept thread and the dispatch loop.
//!
//! Per the NMEA broadcast convention a client whose write
//! fails (broken pipe, slow consumer) is dropped from the
//! list silently. New connections rejoin via the accept
//! loop. We don't try to be fancy about flow control; NMEA
//! sentences are small and at the engine's ~1 Hz publication
//! cadence even very slow clients should keep up.

use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use bris_nmea::QualityThresholds;
use bris_streaming::{format_fix_as_nmea, FixReceiver};
use chrono::Utc;
use tracing::{debug, info, warn};

/// One transport sink. The dispatch loop calls `write` on
/// each registered sink for every published fix.
pub(crate) trait NmeaSink: Send {
    /// Write one NMEA batch. Failures are logged by the
    /// dispatch loop; the sink may stay registered (the
    /// fault may be transient) or unregister itself by
    /// returning a permanent error and erroring on every
    /// subsequent call.
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()>;

    /// Operator-meaningful name for the sink, used in log
    /// messages.
    fn name(&self) -> &str;
}

/// Stdout sink: writes each NMEA batch to `stdout`.
pub(crate) struct StdoutSink;

impl NmeaSink for StdoutSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let mut out = std::io::stdout().lock();
        out.write_all(bytes)?;
        out.flush()?;
        Ok(())
    }
    // The trait signature is `fn name(&self) -> &str`;
    // returning a string literal here ties the literal's
    // 'static lifetime to &self by elision, which clippy
    // flags as unnecessarily restrictive. Changing the
    // trait's return type to `&'static str` would conflict
    // with TcpServerSink, whose name is borrowed from a
    // String field.
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "stdout"
    }
}

/// TCP server sink: accepts connections on a configured
/// port, broadcasts each NMEA batch to all connected
/// clients.
///
/// The accept loop runs on a background thread spawned by
/// [`Self::bind`]; the thread shuts down when the listener
/// is dropped.
pub(crate) struct TcpServerSink {
    name: String,
    clients: Arc<Mutex<Vec<TcpStream>>>,
    /// Retained so the listener thread sees the shutdown
    /// signal when we drop. The listener's
    /// `set_nonblocking` + accept-with-timeout pattern lets
    /// it observe this flag at most every poll interval.
    shutdown: Arc<AtomicBool>,
}

impl TcpServerSink {
    /// Bind a TCP listener on the supplied address and start
    /// the accept thread. Returns immediately after binding;
    /// connections are accepted in the background.
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if the bind fails (port in
    /// use, permission denied for low ports, etc.).
    pub(crate) fn bind(addr: SocketAddr) -> Result<Self> {
        let listener = TcpListener::bind(addr)
            .with_context(|| format!("bind TCP NMEA listener on {addr}"))?;
        listener
            .set_nonblocking(true)
            .context("set_nonblocking on TCP listener")?;
        let clients: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let clients_thread = clients.clone();
        let shutdown_thread = shutdown.clone();
        let name = format!("tcp:{addr}");
        info!(addr = %addr, "TCP NMEA server: listening");
        std::thread::Builder::new()
            .name(format!("bris-nmea-tcp-accept-{addr}"))
            .spawn(move || {
                Self::accept_loop(&listener, &clients_thread, &shutdown_thread);
            })
            .context("spawn TCP NMEA accept thread")?;
        Ok(Self {
            name,
            clients,
            shutdown,
        })
    }

    fn accept_loop(
        listener: &TcpListener,
        clients: &Arc<Mutex<Vec<TcpStream>>>,
        shutdown: &Arc<AtomicBool>,
    ) {
        while !shutdown.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, peer)) => {
                    info!(peer = %peer, "TCP NMEA server: client connected");
                    // Per-client write timeout so a stalled
                    // client doesn't block the broadcast.
                    if let Err(e) = stream.set_write_timeout(Some(Duration::from_millis(500))) {
                        warn!(error = %e, "TCP NMEA server: set_write_timeout failed; dropping client");
                        continue;
                    }
                    clients.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(stream);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No pending connection; sleep briefly and re-poll
                    // shutdown.
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    warn!(error = %e, "TCP NMEA server: accept error");
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
        info!("TCP NMEA server: accept loop stopping");
    }
}

impl Drop for TcpServerSink {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // The accept thread polls shutdown every 100 ms;
        // we don't join here because the thread is
        // detached and will exit on its own. The clients
        // Mutex outlives this Drop via Arc clones.
    }
}

impl NmeaSink for TcpServerSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        // Broadcast under the Mutex; drop any client whose
        // write fails. Holding the mutex across all writes
        // serializes broadcasts but at NMEA's small-byte/
        // low-rate cadence this is in the noise.
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut failed_indices: Vec<usize> = Vec::new();
        for (i, stream) in clients.iter_mut().enumerate() {
            if let Err(e) = stream.write_all(bytes) {
                debug!(error = %e, "TCP NMEA server: client write failed; dropping");
                failed_indices.push(i);
            }
        }
        // Remove failed clients in reverse so indices stay
        // valid.
        for i in failed_indices.into_iter().rev() {
            clients.swap_remove(i);
        }
        Ok(())
    }
    fn name(&self) -> &str {
        &self.name
    }
}

/// Drain the engine's fix stream, format each fix as NMEA,
/// fan out to every registered sink.
///
/// Runs until `shutdown` is set to `true` (typically by
/// Ctrl-C in the CLI). Per-fix sink errors are logged but
/// do not stop the loop.
///
/// # Threading
///
/// Designed to be called from a dedicated dispatch thread
/// (or the main thread of `bris serve`). Writes happen
/// synchronously; if a sink blocks for a long time, the
/// next fix's emission is delayed by that long. Since
/// stdout flushes promptly and TCP writes have a 500 ms
/// per-client timeout, the worst-case total per-fix
/// emission cost is bounded.
#[allow(
    // FixReceiver and the shutdown Arc are deliberately
    // consumed: this function is "the dispatch loop, run
    // until shutdown." Ownership transfer matches that
    // lifecycle.
    clippy::needless_pass_by_value,
)]
pub(crate) fn run_nmea_dispatch(
    fix_rx: FixReceiver,
    mut sinks: Vec<Box<dyn NmeaSink>>,
    shutdown: Arc<AtomicBool>,
    quality_thresholds: QualityThresholds,
) {
    info!(n_sinks = sinks.len(), "NMEA dispatch loop starting");
    let sink_names: Vec<String> = sinks.iter().map(|s| s.name().to_string()).collect();
    debug!(sinks = ?sink_names, "NMEA dispatch sinks");

    while !shutdown.load(Ordering::Relaxed) {
        match fix_rx.try_recv() {
            Ok(Some(fix)) => {
                // Operator-facing structured log of every
                // published fix. Independent of whether any
                // NMEA sink is configured — the log helps
                // operators tell "engine is running" from
                // "engine is silent."
                info!(
                    lat_deg = fix.fix.lat.degrees(),
                    lon_deg = fix.fix.lon.degrees(),
                    sigma_nm = fix.fix.sigma_nm().value(),
                    n_sights = fix.n_sights,
                    azimuth_spread_deg = fix.azimuth_spread_rad.to_degrees(),
                    oldest_sight_age_s = fix.oldest_sight_age_seconds,
                    "published fix"
                );
                let utc = Utc::now();
                let bytes = format_fix_as_nmea(&fix, utc, quality_thresholds);
                for sink in &mut sinks {
                    if let Err(e) = sink.write(bytes.as_bytes()) {
                        warn!(
                            sink = sink.name(),
                            error = %e,
                            "NMEA dispatch: sink write failed"
                        );
                    }
                }
            }
            Ok(None) => {
                // No fix available; sleep briefly to avoid
                // busy-spinning. 100 ms matches the engine's
                // default min_fix_publication_interval_ms.
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(()) => {
                warn!("NMEA dispatch: fix stream channel closed; stopping");
                break;
            }
        }
    }
    info!("NMEA dispatch loop stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory sink for testing the dispatch loop in
    /// isolation from real I/O.
    struct InMemorySink {
        name: &'static str,
        captured: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl NmeaSink for InMemorySink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            self.captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(bytes.to_vec());
            Ok(())
        }
        #[allow(clippy::unnecessary_literal_bound)] // trait constraint; see StdoutSink::name
        fn name(&self) -> &str {
            self.name
        }
    }

    #[test]
    fn stdout_sink_name_is_stdout() {
        let s = StdoutSink;
        assert_eq!(s.name(), "stdout");
    }

    #[test]
    fn tcp_server_bind_to_ephemeral_port_succeeds() {
        // Bind to 127.0.0.1:0 (kernel picks a free port);
        // verify the constructor returns Ok and the sink
        // reports a sensible name.
        let sink = TcpServerSink::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        assert!(sink.name().starts_with("tcp:127.0.0.1:"));
        // Drop triggers shutdown of the accept thread.
    }

    #[test]
    fn in_memory_sink_captures_writes() {
        // Smoke test for the test-helper InMemorySink so
        // failures elsewhere in this module aren't masked
        // by a broken helper.
        let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let mut sink = InMemorySink {
            name: "test",
            captured: captured.clone(),
        };
        sink.write(b"hello").unwrap();
        sink.write(b"world").unwrap();
        let c = captured.lock().unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0], b"hello");
        assert_eq!(c[1], b"world");
    }
}
