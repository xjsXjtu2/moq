//! MoQ Boy: a crowd-controlled Game Boy Color emulator that streams over MoQ.
//!
//! Supports two connection modes:
//!
//! - **Relay mode** (`--url`): connects to a relay server; viewers also connect
//!   via the same relay. Pause/resume is driven by per-track subscription monitoring.
//! - **Server mode** (`--listen`): the emulator acts as a WebTransport server;
//!   viewers connect directly. Pause/resume is driven by session count.
//!
//! Architecture:
//! - **Emulator thread** (blocking): runs the Game Boy at ~59.73fps, captures
//!   framebuffers and audio samples, publishes status JSON.
//! - **Video encoder thread**: receives RGBA frames, converts to H.264, publishes.
//! - **Audio encoder** (on emulator thread): resamples and encodes to Opus.
//! - **Monitor tasks** (async): watch video/audio track subscriptions (relay mode)
//!   or session count (server mode) to pause/resume the emulator when no viewers
//!   are watching.
//! - **Viewer handler** (async): discovers viewer broadcasts, relays button
//!   commands to the emulator.
//!
//! Pause/resume state machine:
//! ```text
//!   relay mode:
//!     video_active ─┐
//!                    ├─ both false → paused (emulation stops, condvar blocks)
//!     audio_active ─┘
//!                      either true → resumed (condvar notified)
//!
//!   server mode:
//!     session_count == 0 → paused
//!     session_count > 0  → resumed
//!
//!   On resume → force video keyframe, re-anchor audio epoch
//! ```
//!
//! Emulator state is preserved across pauses: a new viewer joining after a
//! break picks up mid-playthrough rather than seeing a fresh boot.

use anyhow::{Context, Result};
use bytes::Bytes;
use clap::Parser;
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use url::Url;

#[cfg(feature = "jemalloc")]
#[global_allocator]
static ALLOC: moq_native::jemalloc::tikv_jemallocator::Jemalloc = moq_native::jemalloc::tikv_jemallocator::Jemalloc;

mod audio;
mod emulator;
mod input;
mod overlay;
mod stats;
mod status;
mod video;

#[derive(Parser, Clone)]
pub struct Config {
	/// Connect to the given relay URL (relay mode). Mutually exclusive with --listen.
	#[arg(long, conflicts_with = "server-bind")]
	pub url: Option<Url>,

	/// Path to the Game Boy ROM file.
	#[arg(long)]
	pub rom: PathBuf,

	/// Session name (used in broadcast path). Defaults to ROM filename.
	#[arg(long)]
	pub name: Option<String>,

	/// Base path prefix. Used to derive --prefix-game and --prefix-viewer defaults.
	#[arg(long, default_value = "boy")]
	pub prefix: String,

	/// Path prefix for game broadcasts ("{prefix-game}/{name}"). Defaults to "{prefix}/game".
	#[arg(long)]
	pub prefix_game: Option<String>,

	/// Path prefix for viewer broadcasts ("{prefix-viewer}/{name}"). Defaults to "{prefix}/viewer".
	#[arg(long)]
	pub prefix_viewer: Option<String>,

	/// Location label shown in viewer stats (e.g. "Dallas, TX").
	#[arg(long)]
	pub location: Option<String>,

	/// The MoQ client configuration (used in relay mode).
	#[command(flatten)]
	pub client: moq_native::ClientConfig,

	/// The MoQ server configuration (used in server/direct mode).
	/// Use --listen to specify the bind address (e.g. 0.0.0.0:4443).
	#[command(flatten)]
	pub server: moq_native::ServerConfig,

	/// The log configuration.
	#[command(flatten)]
	pub log: moq_native::Log,

	/// Output framerate (e.g. 30, 60). Defaults to 60.
	#[arg(long, short = 'f', default_value = "60")]
	pub framerate: u32,

	/// Target video bitrate in kbps (e.g. 1000 for 1 Mbps). Defaults to
	/// auto (encoder derives a sane value from resolution and framerate).
	#[arg(long, short = 'b')]
	pub bitrate: Option<u64>,
}

/// Shared state for a game session, accessible from multiple threads/tasks.
///
/// Everything here is either atomic, behind a mutex, or immutable —
/// safe to share via `Arc<Session>` between the emulator thread,
/// track monitors, and async tasks.
struct Session {
	video_encoder: video::VideoEncoder,
	video_track: moq_net::TrackProducer,
	audio_track: moq_net::TrackProducer,

	/// Whether anyone is subscribed to the video/audio tracks (relay mode).
	video_active: AtomicBool,
	audio_active: AtomicBool,

	/// Active session count (server mode only).
	session_count: AtomicUsize,

	/// True when no viewers are watching.
	paused: AtomicBool,
	/// Condvar to wake the emulator thread on resume.
	resume: (Mutex<()>, Condvar),

	/// Location label for status reporting.
	location: Option<String>,
}

impl Session {
	/// Monitor a single track's subscription state (relay mode).
	/// Sets the flag when a viewer subscribes, clears it when all unsubscribe.
	async fn run_track_monitor(&self, name: &str, track: &moq_net::TrackProducer, flag: &AtomicBool) {
		loop {
			if track.used().await.is_err() {
				break;
			}
			tracing::info!("resuming {name}: viewer subscribed");
			flag.store(true, Ordering::Release);
			self.paused.store(false, Ordering::Release);
			self.resume.1.notify_all();

			if track.unused().await.is_err() {
				break;
			}
			tracing::info!("pausing {name}: no viewers");
			flag.store(false, Ordering::Release);
		}
	}

	/// Monitor overall pause state (relay mode).
	/// Pauses when BOTH tracks are unused, resumes when EITHER becomes used.
	async fn run_pause_monitor(&self) {
		loop {
			// Wait for BOTH tracks to become unused.
			let (v, a) = tokio::join!(self.video_track.unused(), self.audio_track.unused());
			if v.is_err() || a.is_err() {
				break;
			}
			tracing::info!("pausing emulation: no viewers");
			self.paused.store(true, Ordering::Release);

			// Wait for EITHER track to become used.
			tokio::select! {
				Err(_) = self.video_track.used() => break,
				Err(_) = self.audio_track.used() => break,
				else => {},
			}
			tracing::info!("resuming emulation: viewer connected");
			self.paused.store(false, Ordering::Release);
			self.resume.1.notify_all();
		}
		// Ensure emulator thread isn't stuck waiting on resume.
		self.paused.store(false, Ordering::Release);
		self.resume.1.notify_all();
	}

	/// Monitor session count (server mode).
	/// Pauses when session count drops to 0, resumes when a session connects.
	async fn run_session_monitor(&self) {
		loop {
			// Wait until paused (session count == 0).
			while self.session_count.load(Ordering::Acquire) > 0 {
				tokio::time::sleep(Duration::from_millis(100)).await;
			}
			tracing::info!("pausing emulation: no connected viewers");
			self.video_active.store(false, Ordering::Release);
			self.audio_active.store(false, Ordering::Release);
			self.paused.store(true, Ordering::Release);

			// Wait until a viewer connects.
			while self.session_count.load(Ordering::Acquire) == 0 {
				tokio::time::sleep(Duration::from_millis(100)).await;
			}
			tracing::info!("resuming emulation: viewer connected");
			self.video_active.store(true, Ordering::Release);
			self.audio_active.store(true, Ordering::Release);
			self.paused.store(false, Ordering::Release);
			self.resume.1.notify_all();
		}
	}

	/// Block the emulator thread until viewers connect.
	///
	/// Emulator state is preserved across pauses so that a new viewer
	/// joins mid-playthrough rather than a fresh boot.
	fn wait_for_resume(&self) {
		tracing::info!("pausing encoding");
		let (lock, cvar) = &self.resume;
		let mut guard = lock.lock().unwrap();
		while self.paused.load(Ordering::Acquire) {
			guard = cvar.wait(guard).unwrap();
		}
		tracing::info!("resuming encoding");
	}

	/// Publish status if it changed since last frame.
	fn publish_status(
		&self,
		emu: &emulator::Emulator,
		viewer_latency: &HashMap<String, Vec<status::LatencyEntry>>,
		game_stats: &stats::Stats,
		publisher: &mut status::StatusPublisher,
	) {
		let held: Vec<_> = emu.pressed_buttons().iter().copied().collect();
		let latency_map: BTreeMap<String, Vec<status::LatencyEntry>> =
			viewer_latency.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

		let status = status::Status {
			buttons: held,
			latency: latency_map,
			stats: game_stats.report(),
			location: self.location.clone(),
		};

		publisher.publish(&status);
	}
}

async fn run(config: &Config) -> Result<()> {
	let rom_path = config.rom.clone();

	// Default name to ROM filename without extension.
	let name = config.name.clone().unwrap_or_else(|| {
		rom_path
			.file_stem()
			.and_then(|s| s.to_str())
			.unwrap_or("unknown")
			.to_string()
	});

	tracing::info!(rom = %rom_path.display(), %name, "starting Game Boy emulator");

	let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<input::Command>(64);

	// Create the broadcast producer.
	let mut broadcast = moq_net::Broadcast::new().produce();

	// Publish origin: the game session broadcast.
	let publish_origin = moq_net::Origin::random().produce();

	// Determine broadcast paths.
	let default_game_prefix = format!("{}/game", config.prefix);
	let default_viewer_prefix = format!("{}/viewer", config.prefix);
	let game_prefix = config.prefix_game.as_deref().unwrap_or(&default_game_prefix);
	let viewer_prefix = config.prefix_viewer.as_deref().unwrap_or(&default_viewer_prefix);

	let broadcast_path = format!("{game_prefix}/{name}");
	publish_origin.publish_broadcast(&broadcast_path, broadcast.consume());

	let viewer_path = format!("{viewer_prefix}/{name}");

	// Set up catalog and encoders (shared between both modes).
	let catalog = moq_mux::catalog::Producer::new(&mut broadcast)?;
	let enc_config = video::EncoderConfig {
		framerate: config.framerate,
		bitrate: config.bitrate.map(|kbps| kbps * 1000),
	};
	let video_encoder = video::VideoEncoder::spawn(broadcast.clone(), catalog.clone(), enc_config);
	let audio_encoder = audio::AudioEncoder::new(broadcast.clone(), catalog.clone(), 44100)?;

	let video_track = video_encoder.track.clone();
	let audio_track = audio_encoder.track().clone();

	// Grab shared counters before the encoders are moved into the session.
	let enc_video_frames = video_encoder.frames_encoded();
	let enc_video_bytes = video_encoder.bytes_encoded();
	let enc_video_keyframes = video_encoder.keyframes_encoded();
	let enc_audio_packets = audio_encoder.packets_encoded();

	let status_publisher = status::StatusPublisher::new(&mut broadcast)?;

	let session = Arc::new(Session {
		video_encoder,
		video_track,
		audio_track,
		video_active: AtomicBool::new(false),
		audio_active: AtomicBool::new(false),
		session_count: AtomicUsize::new(0),
		paused: AtomicBool::new(true), // Start paused until first viewer.
		resume: (Mutex::new(()), Condvar::new()),
		location: config.location.clone(),
	});

	// Periodic encoder frame-rate log.
	{
		let video_cnt = enc_video_frames;
		let bytes_cnt = enc_video_bytes;
		let keyframe_cnt = enc_video_keyframes;
		let audio_cnt = enc_audio_packets;
		tokio::spawn(async move {
			use std::sync::atomic::Ordering;
			let mut prev_video = 0u64;
			let mut prev_bytes = 0u64;
			let mut prev_keyframes = 0u64;
			let mut prev_audio = 0u64;
			let intv = 5;
			let mut interval = tokio::time::interval(std::time::Duration::from_secs(intv));
			interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
			loop {
				interval.tick().await;
				let v = video_cnt.load(Ordering::Relaxed);
				let b = bytes_cnt.load(Ordering::Relaxed);
				let k = keyframe_cnt.load(Ordering::Relaxed);
				let a = audio_cnt.load(Ordering::Relaxed);
				let dk = k - prev_keyframes;
				let dv = (v - prev_video) / intv;
				let db = ((b - prev_bytes) * 8 / 1000) / intv;
				let da = (a - prev_audio) / intv;
				prev_video = v;
				prev_bytes = b;
				prev_keyframes = k;
				prev_audio = a;
				if dv > 0 || da > 0 {
					tracing::info!(vfps = dv, v_kbps = db, afps = da, ifrms = dk, "enc ");
				}
			}
		});
	}

	if let Some(url) = &config.url {
		run_relay_mode(
			config,
			url,
			&name,
			&broadcast_path,
			&viewer_path,
			&rom_path,
			session,
			cmd_tx,
			cmd_rx,
			audio_encoder,
			status_publisher,
			publish_origin,
		)
		.await
	} else {
		run_server_mode(
			config,
			&name,
			&broadcast_path,
			&viewer_path,
			&rom_path,
			session,
			cmd_tx,
			cmd_rx,
			audio_encoder,
			status_publisher,
			publish_origin,
		)
		.await
	}
}

/// Relay mode: connect to a relay server, publish game data, consume viewer broadcasts.
async fn run_relay_mode(
	config: &Config,
	url: &Url,
	name: &str,
	broadcast_path: &str,
	viewer_path: &str,
	rom_path: &PathBuf,
	session: Arc<Session>,
	cmd_tx: tokio::sync::mpsc::Sender<input::Command>,
	cmd_rx: tokio::sync::mpsc::Receiver<input::Command>,
	audio_encoder: audio::AudioEncoder,
	status_publisher: status::StatusPublisher,
	publish_origin: moq_net::OriginProducer,
) -> Result<()> {
	let client = config.client.clone().init()?;

	// Consume origin: viewer broadcasts under the viewer prefix.
	let consume_origin = moq_net::Origin::random().produce();
	let mut viewer_consumer = consume_origin
		.with_root(viewer_path)
		.expect("viewer prefix should be valid")
		.consume();

	tracing::info!(url = %url, %name, broadcast = %broadcast_path, "connecting to relay");

	let reconnect = client
		.with_publish(publish_origin.consume())
		.with_consume(consume_origin)
		.reconnect(url.clone());

	// Monitor track subscriptions (relay mode pause/resume).
	let s = session.clone();
	tokio::spawn(async move { s.run_track_monitor("video", &s.video_track, &s.video_active).await });

	let s = session.clone();
	tokio::spawn(async move { s.run_track_monitor("audio", &s.audio_track, &s.audio_active).await });

	let s = session.clone();
	tokio::spawn(async move { s.run_pause_monitor().await });

	// Run the emulator on a blocking thread.
	let emulator_handle = tokio::task::spawn_blocking({
		let session = session.clone();
		let rom_path = rom_path.clone();
		move || run_emulator(session, &rom_path, audio_encoder, status_publisher, cmd_rx)
	});

	tokio::select! {
		res = emulator_handle => res?.context("emulator error"),
		res = reconnect.closed() => Ok(res?),
		res = input::handle_viewers(&mut viewer_consumer, &cmd_tx) => res,
	}
}

/// Server (direct) mode: listen for incoming connections, each client gets game data
/// and can publish button commands.
async fn run_server_mode(
	config: &Config,
	name: &str,
	broadcast_path: &str,
	viewer_path: &str,
	rom_path: &PathBuf,
	session: Arc<Session>,
	cmd_tx: tokio::sync::mpsc::Sender<input::Command>,
	cmd_rx: tokio::sync::mpsc::Receiver<input::Command>,
	audio_encoder: audio::AudioEncoder,
	status_publisher: status::StatusPublisher,
	publish_origin: moq_net::OriginProducer,
) -> Result<()> {
	let server_config = config.server.clone();
	let mut server = server_config.init().context("failed to initialize server")?;

	let addr = server.local_addr()?;
	tracing::info!(%addr, %name, broadcast = %broadcast_path, "server listening (direct mode)");

	// Start a plain HTTP server on the same port (TCP) to serve /certificate.sha256.
	// The QUIC server uses UDP, so TCP 8443 is free for HTTP. The JS client fetches
	// this endpoint when connecting via `http:` WebTransport.
	let tls_info = server.tls_info();
	tokio::spawn(serve_certificate_fingerprint(addr, tls_info));

	// Configure server-level publish: all sessions see the game broadcast by default.
	server = server.with_publish(publish_origin.consume());

	// Monitor session count for pause/resume.
	let s = session.clone();
	tokio::spawn(async move { s.run_session_monitor().await });

	// Run the emulator on a blocking thread.
	let emulator_handle = tokio::task::spawn_blocking({
		let session = session.clone();
		let rom_path = rom_path.clone();
		move || run_emulator(session, &rom_path, audio_encoder, status_publisher, cmd_rx)
	});

	// Accept loop: each JS client connection becomes a session.
	let accept_loop = async {
		while let Some(request) = server.accept().await {
			let transport = request.transport();
			tracing::info!(transport, "incoming connection");

			// Per-session consume origin for receiving this viewer's button commands.
			let viewer_origin = moq_net::Origin::random().produce();
			let viewer_consumer = viewer_origin
				.with_root(viewer_path)
				.expect("viewer prefix should be valid")
				.consume();

			let cmd_tx = cmd_tx.clone();
			let session = session.clone();
			let game_publish = publish_origin.clone();

			tokio::spawn(async move {
				// Accept the MoQ session: publish game data, consume viewer buttons.
				let moq_session = match request
					.with_publish(game_publish.consume())
					.with_consume(viewer_origin)
					.ok()
					.await
				{
					Ok(s) => s,
					Err(e) => {
						tracing::warn!(error = %e, "session handshake failed");
						return;
					}
				};

				tracing::info!(version = %moq_session.version(), transport, "session established");

				session.session_count.fetch_add(1, Ordering::Release);
				session.paused.store(false, Ordering::Release);
				session.resume.1.notify_all();

				// Handle this viewer's button commands.
				let mut viewer_consumer = viewer_consumer;
				let input_handle = tokio::spawn(async move {
					if let Err(e) = input::handle_viewers(&mut viewer_consumer, &cmd_tx).await {
						tracing::warn!(error = %e, "viewer input error");
					}
				});

				// Wait for session to close.
				let _ = moq_session.closed().await;

				// Cleanup.
				input_handle.abort();
				let prev = session.session_count.fetch_sub(1, Ordering::Release);
				if prev == 1 {
					// Last viewer disconnected: pause.
					session.paused.store(true, Ordering::Release);
				}
				tracing::info!("viewer disconnected");
			});
		}
		anyhow::bail!("server stopped accepting connections")
	};

	tokio::select! {
		res = emulator_handle => res?.context("emulator error"),
		res = accept_loop => res,
	}
}

/// Serve the TLS certificate fingerprint over plain HTTP on the same TCP port as the
/// QUIC server (which uses UDP). This allows the JS client to fetch
/// `/certificate.sha256` and establish an `http:` WebTransport connection.
async fn serve_certificate_fingerprint(
	addr: SocketAddr,
	tls_info: Arc<std::sync::RwLock<moq_native::tls::Info>>,
) -> Result<()> {
	let listener = tokio::net::TcpListener::bind(addr)
		.await
		.context("failed to bind TCP for certificate HTTP server")?;
	tracing::info!(%addr, "certificate HTTP server listening (TCP)");

	loop {
		let (mut stream, peer) = match listener.accept().await {
			Ok(conn) => conn,
			Err(e) => {
				tracing::warn!(error = %e, "HTTP accept error");
				continue;
			}
		};

		let tls_info = tls_info.clone();
		tokio::spawn(async move {
			use tokio::io::{AsyncReadExt, AsyncWriteExt};

			let mut buf = [0u8; 1024];
			let n = match stream.read(&mut buf).await {
				Ok(n) if n > 0 => n,
				_ => return,
			};

			let request = String::from_utf8_lossy(&buf[..n]);
			let path = request
				.lines()
				.next()
				.and_then(|line| line.split_whitespace().nth(1))
				.unwrap_or("");

			let response = if path == "/certificate.sha256" {
				let fingerprint = tls_info
					.read()
					.expect("tls_info lock poisoned")
					.fingerprints
					.first()
					.cloned()
					.unwrap_or_default();
				tracing::debug!(%peer, "serving certificate fingerprint");
				format!(
					"HTTP/1.1 200 OK\r\n\
					 Content-Type: text/plain\r\n\
					 Access-Control-Allow-Origin: *\r\n\
					 Content-Length: {}\r\n\
					 Connection: close\r\n\
					 \r\n\
					 {}",
					fingerprint.len(),
					fingerprint
				)
			} else {
				"HTTP/1.1 404 Not Found\r\n\
				 Content-Length: 0\r\n\
				 Connection: close\r\n\
				 \r\n"
					.to_string()
			};

			let _ = stream.write_all(response.as_bytes()).await;
			let _ = stream.shutdown().await;
		});
	}
}

/// The main emulator loop, running on a blocking thread.
///
/// Ticks the Game Boy at ~59.73fps (the real hardware rate), captures
/// video/audio, publishes status, and handles pause/resume.
fn run_emulator(
	session: Arc<Session>,
	rom_path: &std::path::Path,
	mut audio_encoder: audio::AudioEncoder,
	mut status_publisher: status::StatusPublisher,
	mut cmd_rx: tokio::sync::mpsc::Receiver<input::Command>,
) -> Result<()> {
	let mut emu = emulator::Emulator::new(rom_path)?;
	let start = Instant::now();

	// Run a single tick so the encoders get initial data and publish
	// codec config, even before any viewer subscribes.
	emu.tick();
	let elapsed = start.elapsed();
	let rgba = Bytes::from(emu.framebuffer());
	let ts = hang::container::Timestamp::from_micros(elapsed.as_micros() as u64).context("timestamp overflow")?;
	session.video_encoder.try_frame(rgba, ts);
	let samples = emu.audio_samples();
	if !samples.is_empty() {
		audio_encoder.push_samples(&samples, elapsed)?;
	}

	// Game Boy runs at exactly 59.727 Hz (4194304 Hz CPU / 70224 cycles per frame).
	// 1/59.727 ≈ 16742 microseconds per frame.
	let frame_duration = Duration::from_micros(16_742);
	let mut next_frame = Instant::now();
	let mut viewer_latency: HashMap<String, Vec<status::LatencyEntry>> = HashMap::new();
	let mut game_stats = stats::Stats::new();
	let mut was_audio_active = false;

	// Pending client timestamp watermark to render on the next video frame.
	let mut pending_client_ts: Option<u64> = None;

	// Periodic stats logging (every 5 seconds).
	let mut last_log = Instant::now();
	let mut log_cmd_count: usize = 0;
	let mut log_cmd_details: Vec<String> = Vec::new();
	let mut log_lat_details: Vec<String> = Vec::new();
	let mut log_video_count: u64 = 0;
	let audio_packets = audio_encoder.packets_encoded();
	let mut log_prev_audio: u64 = audio_packets.load(Ordering::Relaxed);

	loop {
		// Block when no viewers are watching. See state diagram in module docs.
		if session.paused.load(Ordering::Acquire) {
			session.wait_for_resume();

			// Don't try to catch up after a pause.
			next_frame = Instant::now();
			// Reset tick timer so pause duration isn't counted.
			game_stats.reset_tick();
			// Force a keyframe so new viewers can start decoding.
			session.video_encoder.force_keyframe();
			// Re-anchor audio timestamps so the pause gap appears in PTS.
			audio_encoder.reset_epoch();
			// Reset log window so the pause gap isn't counted.
			last_log = Instant::now();
			log_cmd_count = 0;
			log_cmd_details.clear();
			log_lat_details.clear();
			log_video_count = 0;
			log_prev_audio = audio_packets.load(Ordering::Relaxed);
		}

		// Drain pending viewer commands before sleeping, so input that
		// arrived during the previous frame's work is applied immediately.
		{
			let elapsed = start.elapsed();
			let encode_ms = u32::try_from(session.video_encoder.encode_duration().as_millis()).unwrap_or(u32::MAX);

			while let Ok(cmd) = cmd_rx.try_recv() {
				log_cmd_count += 1;
				log_cmd_details.push(format!("{:?}", cmd));
				match cmd {
					input::Command::Buttons {
						buttons,
						viewer_id,
						timestamps,
						client_ts,
					} => {
						emu.set_buttons(&viewer_id, buttons.into_iter().collect());
						if let Some(ts) = client_ts {
							pending_client_ts = Some(ts);
						}

						let mut breakdown = Vec::new();
						let entry = |label: &str, ms: u32| status::LatencyEntry {
							label: label.to_string(),
							ms,
						};

						breakdown.push(entry("encode", encode_ms));

						let ms_saturating = |d: Duration| u32::try_from(d.as_millis()).unwrap_or(u32::MAX);

						// Compute latency for each viewer-reported timestamp.
						for t in &timestamps {
							let latency = elapsed.saturating_sub(t.ts);
							breakdown.push(entry(&t.label, ms_saturating(latency)));
						}

						// Input: gap between what the viewer sees and what the server
						// is currently emulating. Uses the oldest viewer timestamp
						// (the rendered frame the user reacted to).
						if let Some(min_ts) = timestamps.iter().map(|t| t.ts).min() {
							let latency = elapsed.saturating_sub(min_ts);
							breakdown.push(entry("input", ms_saturating(latency)));
						}

						{
							let elapsed_ms = elapsed.as_millis();
							let parts: Vec<String> = breakdown.iter().map(|e| format!("{}={}ms", e.label, e.ms)).collect();
							log_lat_details.push(format!("viewer={} elapsed={}ms [{}]", viewer_id, elapsed_ms, parts.join(", ")));
						}
						viewer_latency.insert(viewer_id, breakdown);
					}
					input::Command::ViewerLeft { viewer_id } => {
						emu.viewer_left(&viewer_id);
						viewer_latency.remove(&viewer_id);
					}
					input::Command::Reset => {
						tracing::info!("resetting emulator (viewer request)");
						emu.reset()?;
						game_stats = stats::Stats::new();
					}
				}
			}
		}

		// Wait for next frame.
		let now = Instant::now();
		if now < next_frame {
			std::thread::sleep(next_frame - now);
		}
		next_frame += frame_duration;

		// Capture a single reference timestamp for this tick.
		// Used for both video and audio PTS so they stay aligned.
		let elapsed = start.elapsed();

		// Accumulate stats for this frame.
		let is_video = session.video_active.load(Ordering::Relaxed);
		let is_audio = session.audio_active.load(Ordering::Relaxed);
		game_stats.tick(is_video, is_audio);

		// Tick the emulator.
		emu.tick();

		// Publish status (only if changed).
		session.publish_status(&emu, &viewer_latency, &game_stats, &mut status_publisher);

		// Encode and publish video frame.
		if is_video {
			let mut rgba = emu.framebuffer();
			if let Some(ts) = pending_client_ts.take() {
				overlay::draw_timestamp(&mut rgba, emulator::WIDTH, emulator::HEIGHT, ts);
			}
			let rgba = Bytes::from(rgba);
			let ts =
				hang::container::Timestamp::from_micros(elapsed.as_micros() as u64).context("timestamp overflow")?;
			session.video_encoder.try_frame(rgba, ts);
			log_video_count += 1;
		}

		// Encode and publish audio.
		if is_audio {
			// Re-anchor audio PTS when audio resumes after being inactive.
			if !was_audio_active {
				audio_encoder.reset_epoch();
			}
			let samples = emu.audio_samples();
			if !samples.is_empty() {
				if let Err(e) = audio_encoder.push_samples(&samples, elapsed) {
					tracing::warn!(error = %e, "audio encode error");
				}
			}
		} else {
			// Drain audio buffer even when not encoding to prevent buildup.
			emu.audio_samples();
		}
		was_audio_active = is_audio;

		// Periodic stats log (every 5 seconds).
		if last_log.elapsed() >= Duration::from_secs(5) {
			let cur_audio = audio_packets.load(Ordering::Relaxed);
			let audio_delta = cur_audio - log_prev_audio;
			tracing::info!(
				cmds = log_cmd_count,
				vfrms = log_video_count,
				afrms = audio_delta,
				"emulator cap"
			);
			for detail in &log_cmd_details {
				tracing::debug!(cmd = %detail, "  recv command detail");
			}
			for detail in &log_lat_details {
				tracing::debug!(latency = %detail, "  send latency detail");
			}
			last_log = Instant::now();
			log_cmd_count = 0;
			log_cmd_details.clear();
			log_lat_details.clear();
			log_video_count = 0;
			log_prev_audio = cur_audio;
		}
	}
}

#[tokio::main]
async fn main() -> Result<()> {
	let config = Config::parse();
	config.log.init()?;

	// Validate: need at least one of --url or --listen (server-bind).
	if config.url.is_none()
		&& config.server.bind.is_none()
		&& config.server.tls.generate.is_empty()
		&& config.server.tls.cert.is_empty()
	{
		anyhow::bail!(
			"must specify either --url <relay-url> (relay mode) or --listen <addr> with TLS config (server/direct mode)"
		);
	}

	#[cfg(feature = "jemalloc")]
	let jemalloc = moq_native::jemalloc::run();
	#[cfg(not(feature = "jemalloc"))]
	let jemalloc = std::future::pending::<anyhow::Result<()>>();

	let result = tokio::select! {
		res = run(&config) => res,
		Err(err) = jemalloc => Err(err).context("jemalloc profiler failed"),
		res = tokio::signal::ctrl_c() => res.context("failed to listen for ctrl-c"),
	};

	// run() owns a spawn_blocking emulator thread that loops forever. Returning from main
	// drops the tokio runtime, whose drop blocks indefinitely joining that thread, so the
	// process would hang instead of exiting (defeating systemd Restart=always when the
	// reconnect loop gives up). Exit explicitly so a real exit code reaches the supervisor.
	match result {
		Ok(()) => std::process::exit(0),
		Err(err) => {
			tracing::error!(err = %format!("{err:#}"), "exiting");
			std::process::exit(1);
		}
	}
}
