//! Audio: stereo unsigned-8-bit PCM (Game Boy APU) -> Opus -> MoQ.
//!
//! A thin wrapper over [`moq_audio::AudioProducer`], which resamples to 48 kHz,
//! encodes Opus, and anchors timestamps to a wall clock so audio stays in sync
//! with video. `push_samples` stamps each buffer with the shared emulator
//! clock; `reset_epoch` re-anchors on pause/resume so the gap lands in the PTS.
//!
//! When the WebRTC tap is enabled, encoded Opus frames are also sent to a
//! broadcast channel so WebRTC peers can pick them up for RTP packetization.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;

/// The Game Boy APU outputs stereo audio.
const CHANNELS: u32 = 2;
/// 64 kbps is reasonable for stereo Game Boy audio (simple waveforms).
const OPUS_BITRATE: u32 = 64_000;

/// An encoded Opus frame sent over the WebRTC tap channel.
#[derive(Clone)]
pub struct EncodedAudio {
	/// The Opus-encoded audio data.
	pub data: Bytes,
	/// Timestamp in microseconds since emulator start (aligned with video).
	pub ts_us: u64,
}

pub struct AudioEncoder {
	producer: moq_audio::AudioProducer,
}

impl AudioEncoder {
	pub fn new(
		mut broadcast: moq_net::BroadcastProducer,
		catalog: moq_mux::catalog::Producer,
		input_sample_rate: u32,
	) -> Result<Self> {
		let input = moq_audio::EncoderInput {
			format: moq_audio::AudioFormat::U8,
			sample_rate: input_sample_rate,
			channels: CHANNELS,
		};
		let output = moq_audio::EncoderOutput {
			bitrate: Some(OPUS_BITRATE),
			..Default::default()
		};

		let producer = moq_audio::AudioProducer::new(&mut broadcast, catalog, "audio", input, output)?;
		Ok(Self { producer })
	}

	/// Create an audio encoder that also taps encoded Opus frames to a
	/// broadcast channel for WebRTC streaming.
	pub fn new_with_webrtc(
		mut broadcast: moq_net::BroadcastProducer,
		catalog: moq_mux::catalog::Producer,
		input_sample_rate: u32,
	) -> Result<(Self, tokio::sync::broadcast::Receiver<EncodedAudio>)> {
		let input = moq_audio::EncoderInput {
			format: moq_audio::AudioFormat::U8,
			sample_rate: input_sample_rate,
			channels: CHANNELS,
		};
		let output = moq_audio::EncoderOutput {
			bitrate: Some(OPUS_BITRATE),
			..Default::default()
		};

		let mut producer = moq_audio::AudioProducer::new(&mut broadcast, catalog, "audio", input, output)?;
		let (webrtc_tx, webrtc_rx) = tokio::sync::broadcast::channel::<EncodedAudio>(16);

		// Wire the tap: each encoded Opus packet is forwarded to the broadcast
		// channel so the WebRTC dispatch task can push it to peer connections.
		producer.set_webrtc_tap(Box::new(move |payload: Bytes, ts_us: u64| {
			let frame = EncodedAudio {
				data: payload,
				ts_us,
			};
			let _ = webrtc_tx.send(frame);
		}));

		Ok((Self { producer }, webrtc_rx))
	}

	pub fn track(&self) -> &moq_net::TrackProducer {
		self.producer.track()
	}

	/// Shared counter of encoded audio packets, bumped on every publish.
	pub fn packets_encoded(&self) -> Arc<AtomicU64> {
		self.producer.packets_encoded()
	}

	/// Re-anchor the timeline so a pause gap shows up in the audio PTS.
	pub fn reset_epoch(&mut self) {
		self.producer.reset_epoch();
	}

	/// Push interleaved unsigned-8-bit stereo PCM captured at `elapsed` (since
	/// the emulator started, shared with the video clock).
	pub fn push_samples(&mut self, samples: &[u8], elapsed: Duration) -> Result<()> {
		let frame = moq_audio::Frame {
			timestamp_us: elapsed.as_micros() as u64,
			data: Bytes::copy_from_slice(samples),
		};
		self.producer.write(&frame)?;
		Ok(())
	}
}
