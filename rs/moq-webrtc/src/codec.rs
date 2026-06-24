/// SDP fmtp parameters for H.264 (Baseline Profile, Level 3.1).
///
/// - `profile-level-id=42e01f`: Constrained Baseline Profile, Level 3.1.
///   Widely supported by all major browsers and sufficient for up to 720p.
/// - `packetization-mode=1`: Non-interleaved mode (Single NAL Unit / STAP-A / FU-A).
/// - `level-asymmetry-allowed=1`: The answerer may use a different level than the offerer.
///
/// Reference: <https://datatracker.ietf.org/doc/html/rfc6184>
pub fn h264_fmtp() -> String {
    "profile-level-id=42e01f;packetization-mode=1;level-asymmetry-allowed=1".into()
}

/// SDP fmtp parameters for Opus (stereo, 64 kbps).
///
/// - `minptime=10`: Minimum packetization time (10 ms).
/// - `useinbandfec=1`: Enable in-band forward error correction.
/// - `stereo=1`: The encoder produces stereo.
/// - `sprop-stereo=1`: The stream is stereo.
/// - `maxaveragebitrate=64000`: Target average bitrate (64 kbps).
///
/// Reference: <https://datatracker.ietf.org/doc/html/rfc7587>
pub fn opus_fmtp() -> String {
    "minptime=10;useinbandfec=1;stereo=1;sprop-stereo=1;maxaveragebitrate=64000".into()
}
