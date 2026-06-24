//! Video encoding pipeline: RGBA framebuffer -> H.264 -> MoQ, via `moq-video`.
//!
//! Runs on a dedicated thread so the emulator's frame loop never blocks on the
//! encoder. Frames arrive on a bounded channel; if the encoder falls behind,
//! frames are dropped to keep latency low. moq-video does the RGBA -> H.264
//! encode and the avc3 publish; this module keeps moq-boy's threading,
//! frame-dropping, force-keyframe and timing-stats behavior.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::emulator::{HEIGHT, WIDTH};

/// Encoder configuration passed from the CLI.
#[derive(Clone, Debug)]
pub struct EncoderConfig {
	pub framerate: u32,
	/// Target bitrate in bits per second. None = auto.
	pub bitrate: Option<u64>,
	/// When true, tap encoded NAL units to a broadcast channel for WebRTC consumers.
	pub webrtc_tap: bool,
}

impl Default for EncoderConfig {
	fn default() -> Self {
		Self {
			framerate: 60,
			bitrate: None,
			webrtc_tap: false,
		}
	}
}

/// A single encoded frame's data, sent over the WebRTC tap channel.
#[derive(Clone)]
pub struct EncodedFrame {
	/// The H.264 NAL units (Annex-B start codes stripped).
	pub nals: Vec<Bytes>,
	/// Emulator timestamp for this frame, in microseconds.
	pub ts_us: u64,
	/// Whether this frame is a keyframe (IDR).
	pub is_keyframe: bool,
	/// Approximate encode duration in microseconds for latency tracking.
	pub encode_us: u64,
}

/// Handle to the video encoding thread.
///
/// Frames are submitted via `try_frame()` (non-blocking, drops if full).
pub struct VideoEncoder {
	tx: tokio::sync::mpsc::Sender<EncoderMsg>,
	/// Clone of the video track producer, for monitoring used/unused.
	pub track: moq_net::TrackProducer,
	force_keyframe: Arc<AtomicBool>,
	/// Latest encode duration in microseconds.
	encode_duration: Arc<AtomicU64>,
	/// Total frames successfully encoded and published.
	frames_encoded: Arc<AtomicU64>,
	/// Total bytes of encoded H.264 packets.
	bytes_encoded: Arc<AtomicU64>,
	/// Total keyframes emitted.
	keyframes_encoded: Arc<AtomicU64>,
	/// WebRTC tap: broadcast channel sender for encoded frames (optional).
	webrtc_tx: Option<tokio::sync::broadcast::Sender<EncodedFrame>>,
	_thread: std::thread::JoinHandle<()>,
}

struct EncoderMsg {
	rgba: Bytes,
	ts: hang::container::Timestamp,
}

impl VideoEncoder {
	pub fn spawn(
		broadcast: moq_net::BroadcastProducer,
		catalog: moq_mux::catalog::Producer,
		enc: EncoderConfig,
	) -> Self {
		let (tx, rx) = tokio::sync::mpsc::channel(4);
		let producer = moq_video::encode::Producer::new(broadcast, catalog).expect("failed to create avc3 producer");
		let track = producer.track().expect("avc3 track is eagerly created").clone();

		let force_keyframe = Arc::new(AtomicBool::new(false));
		let encode_duration = Arc::new(AtomicU64::new(0));
		let frames_encoded = Arc::new(AtomicU64::new(0));
		let bytes_encoded = Arc::new(AtomicU64::new(0));
		let keyframes_encoded = Arc::new(AtomicU64::new(0));
		let fk = force_keyframe.clone();
		let ed = encode_duration.clone();
		let fe = frames_encoded.clone();
		let be = bytes_encoded.clone();
		let ke = keyframes_encoded.clone();

		// Create the WebRTC tap channel if requested.
		let webrtc_tx = if enc.webrtc_tap {
			let (tx, _) = tokio::sync::broadcast::channel::<EncodedFrame>(8);
			Some(tx)
		} else {
			None
		};
		let webrtc_tx_clone = webrtc_tx.clone();

		let thread = std::thread::Builder::new()
			.name("video-encoder".into())
			.spawn(move || encoder_thread(rx, producer, enc, fk, ed, fe, be, ke, webrtc_tx_clone))
			.expect("failed to spawn video encoder thread");

		Self {
			tx,
			track,
			force_keyframe,
			encode_duration,
			frames_encoded,
			bytes_encoded,
			keyframes_encoded,
			webrtc_tx,
			_thread: thread,
		}
	}

	/// Send a frame to the encoder. Non-blocking: drops the frame if the
	/// channel is full (capacity=4) to keep latency low.
	pub fn try_frame(&self, rgba: Bytes, ts: hang::container::Timestamp) {
		if self.tx.try_send(EncoderMsg { rgba, ts }).is_err() {
			tracing::warn!("video frame dropped: encoder backpressure");
		}
	}

	/// Force the next encoded frame to be a keyframe (I-frame).
	/// Used on resume after pause so new viewers can start decoding.
	pub fn force_keyframe(&self) {
		self.force_keyframe.store(true, Ordering::Release);
	}

	/// Latest per-frame encode duration.
	pub fn encode_duration(&self) -> Duration {
		Duration::from_micros(self.encode_duration.load(Ordering::Relaxed))
	}

	/// Shared atomic for the latest encode duration in microseconds,
	/// for polling from other tasks (e.g. WebRTC status messages).
	pub fn encode_duration_us(&self) -> Arc<AtomicU64> {
		self.encode_duration.clone()
	}

	/// Shared atomic counter for the encoded frame total (for periodic logging).
	pub(crate) fn frames_encoded(&self) -> Arc<AtomicU64> {
		self.frames_encoded.clone()
	}

	/// Shared atomic counter for the encoded byte total (for periodic logging).
	pub(crate) fn bytes_encoded(&self) -> Arc<AtomicU64> {
		self.bytes_encoded.clone()
	}

	/// Shared atomic counter for the keyframe total (for periodic logging).
	pub(crate) fn keyframes_encoded(&self) -> Arc<AtomicU64> {
		self.keyframes_encoded.clone()
	}

	/// Subscribe to encoded H.264 frames via broadcast channel (WebRTC mode).
	///
	/// Returns `None` if the WebRTC tap was not enabled at construction time.
	pub fn webrtc_subscribe(&self) -> Option<tokio::sync::broadcast::Receiver<EncodedFrame>> {
		self.webrtc_tx.as_ref().map(|tx| tx.subscribe())
	}

	/// Return a clone of the WebRTC tap sender, for creating new subscriptions later.
	pub fn webrtc_tap(&self) -> Option<tokio::sync::broadcast::Sender<EncodedFrame>> {
		self.webrtc_tx.clone()
	}
}

fn encoder_thread(
	mut rx: tokio::sync::mpsc::Receiver<EncoderMsg>,
	mut producer: moq_video::encode::Producer,
	enc: EncoderConfig,
	force_keyframe: Arc<AtomicBool>,
	encode_duration: Arc<AtomicU64>,
	frames_encoded: Arc<AtomicU64>,
	bytes_encoded: Arc<AtomicU64>,
	keyframes_encoded: Arc<AtomicU64>,
	webrtc_tx: Option<tokio::sync::broadcast::Sender<EncodedFrame>>,
) {
	let mut encoder: Option<moq_video::encode::Encoder> = None;

	while let Some(msg) = rx.blocking_recv() {
		let e = match encoder.as_mut() {
			Some(e) => e,
			None => {
				// Game Boy is 160x144; force software (libx264) since hardware
				// encoders can reject such tiny resolutions.
				let mut config = moq_video::encode::Config::new(WIDTH, HEIGHT, enc.framerate);
				config.bitrate = enc.bitrate;
				config.kind = moq_video::encode::Kind::Software;
				match moq_video::encode::Encoder::new(&config) {
					Ok(e) => encoder.insert(e),
					Err(e) => {
						tracing::error!(error = %e, "H.264 encoder init failed");
						return;
					}
				}
			}
		};

		let is_keyframe: bool = force_keyframe.swap(false, Ordering::AcqRel);
		let start = Instant::now();
		match e.encode_rgba(&msg.rgba, WIDTH, HEIGHT, is_keyframe) {
			Ok(packets) => {
				let encode_us = start.elapsed().as_micros() as u64;
				let byte_count: u64 = packets.iter().map(|p| p.len() as u64).sum();
				if let Err(e) = producer.publish(packets.clone(), msg.ts) {
					// Publish only fails once the track/broadcast is gone, which
					// is terminal -- stop rather than flooding logs every frame.
					tracing::error!(error = %e, "video publish failed; stopping encoder");
					return;
				}

				// Tap encoded frame to the WebRTC broadcast channel.
				if let Some(ref tx) = webrtc_tx {
					let frame = EncodedFrame {
						nals: packets,
						ts_us: msg.ts.as_micros() as u64,
						is_keyframe,
						encode_us,
					};
					// Ignore send errors: no WebRTC subscribers is fine.
					let _ = tx.send(frame);
				}

				frames_encoded.fetch_add(1, Ordering::Relaxed);
				bytes_encoded.fetch_add(byte_count, Ordering::Relaxed);
				keyframes_encoded.store(e.keyframe_count(), Ordering::Relaxed);
			}
			// A single bad frame is tolerable; keep going.
			Err(e) => tracing::error!(error = %e, "H.264 encode error"),
		}
		encode_duration.store(start.elapsed().as_micros() as u64, Ordering::Relaxed);
	}
}
