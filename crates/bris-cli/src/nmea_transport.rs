//! NMEA 0183 transport sinks for the CLI's `bris serve`.
//!
//! Drains the engine's [`bris_streaming::FixReceiver`],
//! formats each [`bris_streaming::PublishedFix`] into NMEA
//! sentences via [`bris_streaming::format_fix_as_nmea`], and
//! writes the bytes to one or more transport sinks
//! (stdout, TCP server, UDP, serial).
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
//! - **UDP** ([`UdpSink`]): sends each NMEA batch as one
//!   datagram to a fixed destination address. With a
//!   broadcast destination (`255.255.255.255:10110`) every
//!   plotter on the LAN segment receives the feed with zero
//!   per-client connection state; with a unicast address it
//!   is a point-to-point push. `SO_BROADCAST` is enabled on
//!   the socket so a broadcast destination works out of the
//!   box.
//! - **Serial** ([`SerialSink`]): writes each NMEA batch to a
//!   serial device path (`/dev/ttyUSB0`, the Pi's
//!   `/dev/ttyAMA0`). This is the classic NMEA-0183 wire:
//!   most dedicated marine plotters take a serial feed. The
//!   line discipline (baud rate — NMEA-0183 is 4800 8N1,
//!   AIS-grade is 38400) is configured out of band with
//!   `stty` before `bris serve` starts; see
//!   `docs/operator/nmea_output.md`. Keeping the tty setup
//!   external means the binary carries no platform serial
//!   dependency (no `libudev`) and cross-compiles clean to
//!   the aarch64 Pi target.
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

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::Path;
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
        let listener =
            TcpListener::bind(addr).with_context(|| format!("bind TCP NMEA listener on {addr}"))?;
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
                    clients
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(stream);
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

/// UDP sink: sends each NMEA batch as one datagram to a
/// fixed destination address.
///
/// The socket binds an ephemeral local port on the
/// wildcard address and enables `SO_BROADCAST`, so the
/// destination may be a unicast peer, a directed broadcast,
/// or the limited broadcast address `255.255.255.255`. Each
/// [`Self::write`] call sends the whole batch as a single
/// datagram — NMEA sentence batches are well under the
/// safe UDP payload size, so no fragmentation handling is
/// needed.
pub(crate) struct UdpSink {
    name: String,
    socket: UdpSocket,
    dest: SocketAddr,
}

impl UdpSink {
    /// Bind a UDP socket and target it at `dest`. Returns an
    /// error if the socket cannot be created or broadcast
    /// cannot be enabled.
    ///
    /// # Errors
    ///
    /// Propagates any `io::Error` from binding the local
    /// socket or enabling `SO_BROADCAST`.
    pub(crate) fn bind(dest: SocketAddr) -> Result<Self> {
        // Bind the local end to the wildcard address on an
        // ephemeral port matching the destination's address
        // family (v4 dest -> 0.0.0.0:0, v6 dest -> [::]:0).
        let bind_addr: SocketAddr = if dest.is_ipv4() {
            "0.0.0.0:0".parse().expect("valid v4 wildcard")
        } else {
            "[::]:0".parse().expect("valid v6 wildcard")
        };
        let socket = UdpSocket::bind(bind_addr)
            .with_context(|| format!("bind UDP NMEA socket for dest {dest}"))?;
        socket
            .set_broadcast(true)
            .context("enable SO_BROADCAST on UDP NMEA socket")?;
        info!(dest = %dest, "UDP NMEA sink: sending datagrams");
        Ok(Self {
            name: format!("udp:{dest}"),
            socket,
            dest,
        })
    }
}

impl NmeaSink for UdpSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        // send_to writes the whole batch as one datagram. A
        // short write cannot happen for a single send_to (it
        // either sends the full payload or errors), so we do
        // not loop.
        self.socket.send_to(bytes, self.dest)?;
        Ok(())
    }
    fn name(&self) -> &str {
        &self.name
    }
}

/// Serial sink: writes each NMEA batch to a serial device
/// path.
///
/// The device is opened for writing as a plain file. The tty
/// line discipline (baud rate, framing) is configured out of
/// band with `stty` before `bris serve` runs — see
/// `docs/operator/nmea_output.md`. This deliberately avoids a
/// platform serial dependency: NMEA-0183 is a fixed 4800 8N1
/// (or 38400 for high-speed) link the operator sets up once
/// with `stty`, and keeping it external lets the binary
/// cross-compile clean to the aarch64 Pi target with no
/// `libudev`/`unsafe` termios wrapper.
pub(crate) struct SerialSink {
    name: String,
    file: File,
}

impl SerialSink {
    /// Open `device` for writing.
    ///
    /// # Errors
    ///
    /// Propagates any `io::Error` from opening the device
    /// (does not exist, permission denied, not a writable
    /// device).
    pub(crate) fn open(device: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .open(device)
            .with_context(|| format!("open serial NMEA device {}", device.display()))?;
        info!(device = %device.display(), "serial NMEA sink: writing sentences");
        Ok(Self {
            name: format!("serial:{}", device.display()),
            file,
        })
    }
}

impl NmeaSink for SerialSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.file.write_all(bytes)?;
        self.file.flush()?;
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

    // ---- Fixture fix + well-formed-sentence assertions over
    //      the UDP and serial sinks. -------------------------

    use bris_core::time::{Tt, JD_J2000};
    use bris_core::{Latitude, Longitude};
    use bris_nav::Fix;
    use bris_streaming::{format_fix_as_nmea, DominantSource, FixProvenance, PublishedFix};
    use chrono::{TimeZone, Utc};
    use std::net::UdpSocket;

    /// A deterministic published fix carrying a known 1σ
    /// uncertainty (`sigma_major` = `sigma_minor` = 0.5 nm ⇒ 926.0 m in
    /// `$GPGST`), used to assert the transport sinks ship the
    /// honest uncertainty over the wire unmodified.
    fn fixture_fix() -> PublishedFix {
        PublishedFix {
            fix: Fix {
                lat: Latitude::from_degrees(47.6).unwrap(),
                lon: Longitude::from_degrees(-122.3).unwrap(),
                covariance_nm2: [[0.25, 0.0], [0.0, 0.25]],
                sigma_major_nm: 0.5,
                sigma_minor_nm: 0.5,
                orientation_rad: 0.0,
                sight_count: 3,
                chi_square: None,
            },
            n_sights: 3,
            azimuth_spread_rad: std::f64::consts::FRAC_PI_2,
            oldest_sight_age_seconds: 60.0,
            dominant_source: DominantSource::Horizon,
            timestamp: Tt::from_julian_date(JD_J2000),
            contributing_frame_ids: Vec::new(),
            provenance: FixProvenance::SaintHilaire,
        }
    }

    /// Formatted NMEA batch for the fixture fix at a fixed
    /// wall-clock instant.
    fn fixture_batch() -> String {
        let utc = Utc.with_ymd_and_hms(2024, 6, 15, 12, 34, 56).unwrap();
        format_fix_as_nmea(&fixture_fix(), utc, QualityThresholds::default())
    }

    /// Assert a received NMEA batch is well formed and carries
    /// the fixture's honest 1σ uncertainty:
    /// - every sentence's `*XX` checksum matches the XOR of
    ///   its body (so nothing was mangled in transit), and
    /// - the `$GPGST` uncertainty channel reports the fix's
    ///   real σ (926.0 m semi-major/minor for the 0.5 nm
    ///   fixture) — never a zeroed / hidden value.
    fn assert_well_formed_with_uncertainty(received: &str) {
        // At least the standard set + $PBRIS,FIX.
        for tag in ["$GPGLL", "$GPRMC", "$GPGGA", "$GPGST", "$PBRIS,FIX"] {
            assert!(
                received.contains(tag),
                "missing {tag} in received batch:\n{received}"
            );
        }
        // Every sentence checksum must validate.
        for line in received.split("\r\n").filter(|l| l.starts_with('$')) {
            let star = line.rfind('*').unwrap_or_else(|| {
                panic!("sentence has no checksum delimiter: {line}");
            });
            let body = &line[1..star];
            let want = &line[star + 1..];
            let got = format!("{:02X}", bris_nmea::checksum(body));
            assert_eq!(
                got, want,
                "checksum mismatch on {line} (body xor = {got}, sentence claims {want})"
            );
        }
        // The GPGST 1σ ellipse: 0.5 nm * 1852 m/nm = 926.0 m
        // on both semi-axes, and the sigma-lat/sigma-lon fields
        // (√0.25 nm = 0.5 nm → 926.0 m). The uncertainty is
        // present and honest, not hidden.
        let gst = received
            .split("\r\n")
            .find(|l| l.starts_with("$GPGST"))
            .expect("no $GPGST sentence");
        assert!(
            gst.contains(",926.0,926.0,"),
            "GPGST does not carry the fixture's 926.0 m semi-axis σ: {gst}"
        );
    }

    #[test]
    fn formatter_batch_is_well_formed_with_honest_uncertainty() {
        // Sanity floor: the formatted bytes themselves are
        // well formed before any transport touches them.
        assert_well_formed_with_uncertainty(&fixture_batch());
    }

    #[test]
    fn udp_sink_delivers_well_formed_sentences() {
        // Stand up a receiver on an ephemeral loopback port,
        // point a UdpSink at it, send the fixture batch, and
        // assert the datagram arrives byte-for-byte correct.
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let dest = receiver.local_addr().unwrap();

        let mut sink = UdpSink::bind(dest).unwrap();
        assert!(sink.name().starts_with("udp:127.0.0.1:"));
        let batch = fixture_batch();
        sink.write(batch.as_bytes()).unwrap();

        let mut buf = [0u8; 4096];
        let (n, _peer) = receiver.recv_from(&mut buf).unwrap();
        let payload = std::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(payload, batch, "UDP payload mangled in transit");
        assert_well_formed_with_uncertainty(payload);
    }

    #[test]
    fn serial_sink_writes_well_formed_sentences() {
        // The serial sink opens its target path for writing
        // and streams bytes to it. A regular temp file stands
        // in for the tty device (both are just a writable fd
        // from the sink's point of view); assert the bytes
        // written to the "device" are the well-formed batch.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let mut sink = SerialSink::open(&path).unwrap();
        assert!(sink.name().starts_with("serial:"));
        let batch = fixture_batch();
        sink.write(batch.as_bytes()).unwrap();
        // Two batches to confirm the sink appends (streams)
        // rather than truncating on each write.
        sink.write(batch.as_bytes()).unwrap();
        drop(sink);

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, format!("{batch}{batch}"));
        // Each half is an independently well-formed batch.
        assert_well_formed_with_uncertainty(&batch);
        assert!(written.starts_with(&batch));
    }
}
