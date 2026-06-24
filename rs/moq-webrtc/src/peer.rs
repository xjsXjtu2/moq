//! WebRTC PeerConnection lifecycle management via [str0m].
//!
//! Wraps a str0m [`Rtc`] instance, sets up H.264 video and Opus audio tracks
//! on the SDP answer, provides push/poll APIs for the emulator loop, and
//! routes incoming DataChannel messages back to the application.
//!
//! ## Integration Status
//!
//! The str0m API surface changed between 0.6.x and later versions. This module
//! currently provides the integration contract (types and function signatures)
//! that moq-boy's WebRTC mode expects. The internal str0m calls are marked with
//! `// TODO(str0m):` comments and need to be updated to match the specific
//! str0m version in use.

use std::collections::VecDeque;

use anyhow::Result;
use bytes::Bytes;

/// A parsed viewer command received from the DataChannel.
#[derive(Debug, Clone)]
pub struct InputCommand {
    /// `"buttons"` or `"reset"`.
    pub cmd_type: String,
    /// Button names when cmd_type is `"buttons"`.
    pub buttons: Vec<String>,
    /// Optional client-side timestamp (for latency measurement).
    pub client_ts: Option<u64>,
}

/// Output produced by driving the str0m state machine.
#[derive(Debug)]
pub enum WebrtcOutput {
    /// SDP answer to send to the client via signaling.
    Answer {
        session_id: String,
        sdp: String,
    },
    /// ICE candidate to push to the client via signaling.
    IceCandidate {
        session_id: String,
        candidate: String,
    },
    /// The ICE connection was lost for this session.
    IceDisconnected(String),
}

/// Manages a single WebRTC peer connection.
///
/// Each viewer gets one `WebrtcPeer`. The emulator loop pushes encoded frames
/// through `push_video` / `push_audio` and polls for incoming DataChannel
/// messages via `recv_input`.
pub struct WebrtcPeer {
    session_id: String,
    /// Pending DataChannel messages from the viewer.
    incoming_commands: VecDeque<InputCommand>,
    /// Whether the ICE connection is established (media can flow).
    ice_connected: bool,
    /// Buffered SPS/PPS to send before the next IDR.
    buffered_sps_pps: Vec<Bytes>,
}

impl WebrtcPeer {
    /// Accept a client SDP offer and produce an SDP answer.
    ///
    /// This creates the str0m `Rtc` instance, adds H.264 video and Opus audio
    /// tracks as `sendonly`, sets the remote SDP, and generates the local answer.
    pub fn accept_offer(session_id: String, offer_sdp: &str) -> Result<Self> {
        // TODO(str0m): Create an Rtc instance and negotiate the SDP offer.
        //
        // Example flow with str0m 0.6+:
        //
        //   let mut rtc = str0m::Rtc::new(str0m::RtcConfig::new());
        //
        //   // Add video track (H.264 sendonly)
        //   let video_mid = rtc.add_media(
        //       MediaKind::Video,
        //       Direction::SendOnly,
        //       &[Codec::H264],
        //       None,
        //   );
        //
        //   // Add audio track (Opus sendonly)
        //   let audio_mid = rtc.add_media(
        //       MediaKind::Audio,
        //       Direction::SendOnly,
        //       &[Codec::Opus],
        //       None,
        //   );
        //
        //   // Set remote offer and generate answer
        //   rtc.set_remote_sdp(offer_sdp)?;
        //   let answer_sdp = rtc.local_sdp()?;
        //
        // The answer SDP is delivered to the client via
        // `WebrtcOutput::Answer { session_id, sdp: answer_sdp }`.
        //
        // For now, we accept the offer and record it. The actual str0m
        // integration is gated on confirming the exact str0m version and API.

        tracing::info!(
            %session_id,
            offer_len = offer_sdp.len(),
            "WebRTC offer accepted (str0m integration pending)"
        );

        Ok(Self {
            session_id,
            incoming_commands: VecDeque::new(),
            ice_connected: false,
            buffered_sps_pps: Vec::new(),
        })
    }

    /// Set the cached SPS/PPS that should be sent before the next keyframe.
    ///
    /// Must be called once at session start (after the encoder produces the first
    /// IDR) so new peers receive the parameter sets needed for decoding.
    pub fn set_sps_pps(&mut self, sps: Bytes, pps: Bytes) {
        self.buffered_sps_pps = vec![sps, pps];
    }

    /// Drive the str0m state machine and collect pending output.
    ///
    /// Must be called periodically (every frame or via a timer) to:
    /// - Generate ICE candidates
    /// - Detect ICE connection state changes
    /// - Process incoming DataChannel messages
    pub fn poll(&mut self) -> Result<Vec<WebrtcOutput>> {
        // TODO(str0m): Drive the Rtc state machine.
        //
        // Example:
        //   let duration = std::time::Duration::from_millis(1);
        //   let events = self.rtc.poll_output(duration)?;
        //   for event in events {
        //       match event {
        //           Output::Transmit(t) => { /* write to socket */ }
        //           Output::Time(v) => { /* handle timer */ }
        //       }
        //   }
        //
        //   // Check ICE state
        //   let ice = self.rtc.ice_connection_state();
        //   self.ice_connected = ice == IceConnectionState::Connected;
        //
        //   // Drain DataChannel messages
        //   while let Some(msg) = self.rtc.receive_text() {
        //       // parse and push to incoming_commands
        //   }

        Ok(Vec::new())
    }

    /// Return the next pending viewer input command, if any.
    pub fn recv_input(&mut self) -> Option<InputCommand> {
        self.incoming_commands.pop_front()
    }

    /// Add a remote ICE candidate received from the browser via signaling.
    pub fn add_ice_candidate(&mut self, candidate: &str) -> Result<()> {
        // TODO(str0m): Feed candidate to Rtc instance.
        //
        // Example:
        //   self.rtc.add_remote_candidate(candidate)?;

        tracing::debug!(
            session = %self.session_id,
            %candidate,
            "remote ICE candidate received"
        );
        Ok(())
    }

    /// Whether the ICE connection is established and media can flow.
    pub fn is_connected(&self) -> bool {
        self.ice_connected
    }

    /// The session identifier for signaling routing.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}
