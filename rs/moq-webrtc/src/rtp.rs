//! RTP packetizer: wraps encoded H.264 NAL units and Opus frames
//! into standard RTP packets for delivery over WebRTC media tracks.
//!
//! ## RFC references
//! - H.264: [RFC 6184](https://datatracker.ietf.org/doc/html/rfc6184)
//! - Opus:  [RFC 7587](https://datatracker.ietf.org/doc/html/rfc7587)

use bytes::{BufMut, Bytes, BytesMut};

/// SSRC value used for both video and audio RTP streams.
/// Using a single SSRC simplifies str0m integration, which assigns
/// its own SSRC internally — this value is informational.
const DEFAULT_SSRC: u32 = 0x6d_6f_71_00;

/// Standard RTP header length in bytes (no CSRC, no extension).
const RTP_HEADER_LEN: usize = 12;

/// RTP payload type for H.264 (dynamic range 96–127, chosen 96 by convention).
pub const H264_PT: u8 = 96;

/// RTP payload type for Opus (dynamic range 96–127, chosen 111 by convention).
pub const OPUS_PT: u8 = 111;

/// RTP clock rate for H.264 video (90 kHz).
pub const H264_CLOCK_RATE: u32 = 90_000;

/// RTP clock rate for Opus audio (48 kHz).
pub const OPUS_CLOCK_RATE: u32 = 48_000;

/// Holds per-track RTP sequence numbers and SSRC state.
///
/// Not `Clone` on purpose: each peer needs its own packetizer so sequence
/// numbers and timestamps are strictly monotonic per connection.
pub struct RtpPacketizer {
    ssrc: u32,
    video_seq: u16,
    audio_seq: u16,
}

impl Default for RtpPacketizer {
    fn default() -> Self {
        Self {
            ssrc: DEFAULT_SSRC,
            video_seq: 0,
            audio_seq: 0,
        }
    }
}

impl RtpPacketizer {
    /// Create a packetizer with a specific SSRC.
    pub fn new(ssrc: u32) -> Self {
        Self {
            ssrc,
            ..Default::default()
        }
    }

    /// Return the current SSRC.
    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// Wrap a single H.264 NAL unit into an RTP packet.
    ///
    /// Uses **Single NAL Unit mode** (RFC 6184 §5.6): the NAL header byte
    /// immediately follows the 12-byte RTP header. This is the simplest mode
    /// and works well when every NAL unit fits within the MTU — which is always
    /// the case for Game Boy frames (160×144, each frame typically produces
    /// a handful of tiny NAL units).
    ///
    /// If `nal` were larger than the MTU (~1400 bytes with overhead), FU-A
    /// fragmentation would be needed, but that path is never hit for moq-boy.
    pub fn wrap_h264(&mut self, nal: &[u8], timestamp_90khz: u32) -> Bytes {
        let seq = self.video_seq;
        self.video_seq = self.video_seq.wrapping_add(1);

        let mut buf = BytesMut::with_capacity(RTP_HEADER_LEN + nal.len());

        // RTP header (RFC 3550 §5.1)
        buf.put_u8(0x80); // V=2, P=0, X=0, CC=0
        buf.put_u8(H264_PT); // PT=96, no marker bit by default
        buf.put_u16(seq);
        buf.put_u32(timestamp_90khz);
        buf.put_u32(self.ssrc);

        // Payload: the NAL unit as-is (includes the NAL header byte).
        // moq-video already strips the Annex-B start code (00 00 00 01),
        // so what we receive is the raw NAL unit ready for RTP.
        buf.put_slice(nal);

        buf.freeze()
    }

    /// Wrap a single H.264 NAL unit with the RTP marker bit set.
    ///
    /// The marker bit signals the last packet of a video frame. Set it on
    /// the final NAL of each encoded frame so the decoder knows when a
    /// complete access unit has arrived.
    pub fn wrap_h264_marker(&mut self, nal: &[u8], timestamp_90khz: u32) -> Bytes {
        let seq = self.video_seq;
        self.video_seq = self.video_seq.wrapping_add(1);

        let mut buf = BytesMut::with_capacity(RTP_HEADER_LEN + nal.len());

        buf.put_u8(0x80); // V=2
        buf.put_u8(H264_PT | 0x80); // PT=96, M=1 (marker)
        buf.put_u16(seq);
        buf.put_u32(timestamp_90khz);
        buf.put_u32(self.ssrc);
        buf.put_slice(nal);

        buf.freeze()
    }

    /// Wrap an Opus audio frame into an RTP packet.
    ///
    /// RFC 7587 §4.1: the RTP payload of an Opus packet is simply the Opus
    /// encoded frame itself. We set the marker bit on every packet because
    /// each Opus frame is a self-contained coded audio unit.
    ///
    /// The `opus_frame` should be the raw bytes produced by the Opus encoder.
    /// For Game Boy audio, each frame is typically 2.5 ms or 5 ms worth of
    /// encoded audio at 64 kbps (stereo), far below MTU.
    pub fn wrap_opus(&mut self, opus_frame: &[u8], timestamp_48khz: u32) -> Bytes {
        let seq = self.audio_seq;
        self.audio_seq = self.audio_seq.wrapping_add(1);

        let mut buf = BytesMut::with_capacity(RTP_HEADER_LEN + opus_frame.len());

        // RTP header with marker bit set (each Opus frame is a complete audio unit).
        buf.put_u8(0x80); // V=2
        buf.put_u8(OPUS_PT | 0x80); // PT=111, M=1
        buf.put_u16(seq);
        buf.put_u32(timestamp_48khz);
        buf.put_u32(self.ssrc);
        buf.put_slice(opus_frame);

        buf.freeze()
    }

    /// Reset sequence numbers. Call after a pause-resume cycle to avoid
    /// the decoder seeing a large discontinuity in sequence numbers.
    pub fn reset_seq(&mut self) {
        self.video_seq = 0;
        self.audio_seq = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_rtp_header() {
        let mut pkt = RtpPacketizer::new(0x12345678);
        // A minimal NAL unit: SPS-like content (7 bytes including NAL header)
        let nal = b"\x67\x42\x00\x1e\xab\x40\x80";
        let ts = 90000u32; // 1 second at 90 kHz

        let rtp = pkt.wrap_h264(nal, ts);

        assert_eq!(rtp.len(), 12 + 7);
        assert_eq!(rtp[0], 0x80); // V=2
        assert_eq!(rtp[1] & 0x7f, H264_PT); // PT
        assert_eq!(rtp[1] & 0x80, 0); // M=0
        assert_eq!(u16::from_be_bytes([rtp[2], rtp[3]]), 0); // seq=0
        assert_eq!(u32::from_be_bytes([rtp[4], rtp[5], rtp[6], rtp[7]]), ts);
        assert_eq!(u32::from_be_bytes([rtp[8], rtp[9], rtp[10], rtp[11]]), 0x12345678);
        assert_eq!(&rtp[12..], nal);
    }

    #[test]
    fn h264_marker_bit() {
        let mut pkt = RtpPacketizer::default();
        let nal = b"\x41\x9a\x00";
        let ts = 45000u32;

        let rtp = pkt.wrap_h264_marker(nal, ts);
        assert_eq!(rtp[1], H264_PT | 0x80); // M=1
    }

    #[test]
    fn h264_sequence_increment() {
        let mut pkt = RtpPacketizer::default();
        let nal = b"\x65\xb8\x00";

        let p0 = pkt.wrap_h264(nal, 0);
        let p1 = pkt.wrap_h264(nal, 3000);

        assert_eq!(u16::from_be_bytes([p0[2], p0[3]]), 0);
        assert_eq!(u16::from_be_bytes([p1[2], p1[3]]), 1);
    }

    #[test]
    fn opus_rtp_header() {
        let mut pkt = RtpPacketizer::new(0xabcd0001);
        // A small Opus frame (e.g. 2.5ms of silence at 64 kbps stereo → ~80 bytes)
        let opus = b"\xfa\xff\xfe\x00";
        let ts = 480u32; // 10 ms at 48 kHz

        let rtp = pkt.wrap_opus(opus, ts);

        assert_eq!(rtp.len(), 12 + 4);
        assert_eq!(rtp[0], 0x80); // V=2
        assert_eq!(rtp[1] & 0x7f, OPUS_PT); // PT
        assert_eq!(rtp[1] & 0x80, 0x80); // M=1 (always for Opus)
        assert_eq!(u16::from_be_bytes([rtp[2], rtp[3]]), 0); // seq=0
        assert_eq!(u32::from_be_bytes([rtp[4], rtp[5], rtp[6], rtp[7]]), ts);
        assert_eq!(u32::from_be_bytes([rtp[8], rtp[9], rtp[10], rtp[11]]), 0xabcd0001);
        assert_eq!(&rtp[12..], opus);
    }

    #[test]
    fn wrap_sequence_overflow() {
        let mut pkt = RtpPacketizer::default();
        pkt.video_seq = u16::MAX;
        let nal = b"\x41\x9a";

        let _p0 = pkt.wrap_h264(nal, 0);
        let p1 = pkt.wrap_h264(nal, 0);

        assert_eq!(u16::from_be_bytes([p1[2], p1[3]]), 0); // wraps to 0
    }

    #[test]
    fn reset_seq() {
        let mut pkt = RtpPacketizer::default();
        pkt.video_seq = 42;
        pkt.audio_seq = 99;

        pkt.reset_seq();
        assert_eq!(pkt.video_seq, 0);
        assert_eq!(pkt.audio_seq, 0);
    }
}
