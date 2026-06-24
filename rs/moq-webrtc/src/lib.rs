//! WebRTC transport helpers for MoQ applications.
//!
//! Provides:
//! - RTP packetization for H.264 Annex-B NAL units (RFC 6184) and Opus frames (RFC 7587)
//! - SDP codec parameter generation
//! - PeerConnection lifecycle management (via [str0m])
//! - HTTP-based signaling server for SDP / ICE exchange
//!
//! This crate is used by [`moq-boy`] to add a WebRTC direct-connection mode
//! alongside the existing MoQ-over-WebTransport transport.

mod codec;
mod peer;
mod rtp;
pub mod signaling;

pub use codec::{h264_fmtp, opus_fmtp};
pub use peer::{InputCommand, WebrtcOutput, WebrtcPeer};
pub use rtp::RtpPacketizer;
