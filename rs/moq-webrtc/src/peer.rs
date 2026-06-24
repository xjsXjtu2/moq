//! WebRTC PeerConnection lifecycle management via [str0m].
//!
//! Wraps a str0m [`Rtc`] instance, sets up H.264 video and Opus audio tracks
//! on the SDP answer, provides push/poll APIs for the emulator loop, and
//! routes incoming DataChannel messages back to the application.
//!
//! str0m handles RTP packetization internally (we are NOT in RTP mode),
//! so media frames are written via `writer().write()`.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Instant;

use anyhow::{Context, Result};
use bytes::Bytes;
use str0m::change::SdpOffer;
use str0m::media::{Frequency, MediaTime, Mid};
use str0m::net::{DatagramRecv, Protocol};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

/// A parsed viewer command received from the DataChannel.
#[derive(Debug, Clone, serde::Deserialize)]
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
    /// ICE candidate to push to the client via signaling (trickle ICE).
    IceCandidate {
        session_id: String,
        candidate: String,
    },
    /// The ICE connection was lost for this session.
    IceDisconnected(String),
    /// UDP packet to send to the browser (STUN/DTLS/RTP).
    Transmit {
        destination: SocketAddr,
        contents: Bytes,
    },
}

/// Manages a single WebRTC peer connection.
///
/// Each viewer gets one `WebrtcPeer`. The emulator loop pushes encoded frames
/// through `push_video` / `push_audio` and polls for incoming DataChannel
/// messages via `recv_input`.
pub struct WebrtcPeer {
    session_id: String,
    rtc: Rtc,
    /// Whether the SDP answer has been emitted yet.
    answer_emitted: bool,
    /// The SDP answer string (populated by accept_offer, emitted on first poll).
    pending_answer: Option<String>,
    /// Media line ID for the video track (populated from MediaAdded events).
    video_mid: Option<Mid>,
    /// Media line ID for the audio track (populated from MediaAdded events).
    audio_mid: Option<Mid>,
    /// Pending DataChannel messages from the viewer.
    incoming_commands: VecDeque<InputCommand>,
    /// Whether the ICE connection is established (media can flow).
    ice_connected: bool,
    /// Buffered SPS/PPS NAL units to send before the next keyframe.
    buffered_sps_pps: Vec<Bytes>,
    /// Pending trickle ICE candidates gathered during ICE.
    pending_candidates: Vec<String>,
}

impl WebrtcPeer {
    /// Accept a client SDP offer and produce an SDP answer.
    ///
    /// `local_addr` is the UDP socket address that will receive ICE/STUN/DTLS
    /// traffic. It becomes the server's host candidate in the SDP answer.
    pub fn accept_offer(
        session_id: String,
        offer_sdp: &str,
        local_addr: SocketAddr,
    ) -> Result<Self> {
        // NOT in RTP mode: let str0m handle RTP packetization internally.
        // Explicitly disable VP8/VP9 so H.264 is the only video codec the
        // server supports. Without this the browser's SDP offer (which lists
        // VP8 first) would cause str0m to negotiate VP8, but we encode H.264.
        let mut rtc = Rtc::builder()
            .set_ice_lite(true)
            .enable_vp8(false)
            .enable_vp9(false)
            .enable_h264(true)
            .enable_opus(true)
            .build();

        // ICE-Lite: the server has a known public address. Add a single host
        // candidate that will be included in the SDP answer. The browser does
        // full ICE; the server is "lite" and never sends trickle candidates.
        let candidate =
            Candidate::host(local_addr, Protocol::Udp).context("failed to create host candidate")?;
        rtc.add_local_candidate(candidate);

        // Parse and accept the browser's SDP offer.
        let offer =
            SdpOffer::from_sdp_string(offer_sdp).context("failed to parse SDP offer")?;
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .context("failed to accept SDP offer")?;
        let answer_sdp = answer.to_sdp_string();

        tracing::info!(
            %session_id,
            offer_len = offer_sdp.len(),
            answer_len = answer_sdp.len(),
            %local_addr,
            "WebRTC offer accepted"
        );

        Ok(Self {
            session_id,
            rtc,
            answer_emitted: false,
            pending_answer: Some(answer_sdp),
            video_mid: None,
            audio_mid: None,
            incoming_commands: VecDeque::new(),
            ice_connected: false,
            buffered_sps_pps: Vec::new(),
            pending_candidates: Vec::new(),
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
    /// - Emit the SDP answer (first call only)
    /// - Generate trickle ICE candidates
    /// - Detect ICE connection state changes
    /// - Process incoming DataChannel messages
    /// - Collect UDP transmits (STUN/DTLS/RTP)
    pub fn poll(&mut self) -> Result<Vec<WebrtcOutput>> {
        let mut outputs = Vec::new();

        // Emit the SDP answer on the first poll call.
        if !self.answer_emitted {
            if let Some(answer) = self.pending_answer.take() {
                outputs.push(WebrtcOutput::Answer {
                    session_id: self.session_id.clone(),
                    sdp: answer,
                });
                self.answer_emitted = true;
            }
        }

        // Emit any pending trickle ICE candidates.
        for c in self.pending_candidates.drain(..) {
            outputs.push(WebrtcOutput::IceCandidate {
                session_id: self.session_id.clone(),
                candidate: c,
            });
        }

        // Drive the str0m state machine.
        loop {
            let output = match self.rtc.poll_output() {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!(session = %self.session_id, error = %e, "poll_output error");
                    break;
                }
            };

            match output {
                Output::Timeout(_t) => break,
                Output::Transmit(t) => {
                    let contents: Vec<u8> = t.contents.into();
                    outputs.push(WebrtcOutput::Transmit {
                        destination: t.destination,
                        contents: Bytes::from(contents),
                    });
                }
                Output::Event(event) => match event {
                    Event::Connected => {
                        tracing::info!(session = %self.session_id, "ICE+DTLS connected");
                        self.ice_connected = true;
                    }
                    Event::IceConnectionStateChange(state) => {
                        tracing::debug!(session = %self.session_id, ?state, "ICE state");
                        if state == IceConnectionState::Disconnected {
                            self.ice_connected = false;
                            outputs.push(WebrtcOutput::IceDisconnected(
                                self.session_id.clone(),
                            ));
                        }
                    }
                    Event::MediaAdded(m) => {
                        tracing::info!(
                            session = %self.session_id,
                            mid = %m.mid,
                            ?m.kind,
                            "media added"
                        );
                        match m.kind {
                            str0m::media::MediaKind::Video => self.video_mid = Some(m.mid),
                            str0m::media::MediaKind::Audio => self.audio_mid = Some(m.mid),
                        }
                    }
                    Event::ChannelOpen(id, label) => {
                        tracing::info!(
                            session = %self.session_id,
                            channel_id = ?id,
                            %label,
                            "DataChannel opened"
                        );
                    }
                    Event::ChannelData(data) => {
                        if !data.binary {
                            if let Ok(text) = String::from_utf8(data.data) {
                                if let Ok(cmd) = serde_json::from_str::<InputCommand>(&text) {
                                    self.incoming_commands.push_back(cmd);
                                }
                            }
                        }
                    }
                    _ => {}
                },
            }
        }

        Ok(outputs)
    }

    /// Feed an incoming UDP packet to the str0m state machine.
    pub fn handle_input(
        &mut self,
        now: Instant,
        source: SocketAddr,
        destination: SocketAddr,
        data: &[u8],
    ) -> Result<()> {
        let contents = DatagramRecv::try_from(data)
            .with_context(|| format!("failed to parse incoming UDP datagram ({} bytes)", data.len()))?;
        let receive = str0m::net::Receive {
            proto: Protocol::Udp,
            source,
            destination,
            contents,
        };
        self.rtc.handle_input(Input::Receive(now, receive))?;
        Ok(())
    }

    /// Notify str0m of time passing. Call periodically even when idle.
    pub fn handle_timeout(&mut self, now: Instant) -> Result<()> {
        self.rtc.handle_input(Input::Timeout(now))?;
        Ok(())
    }

    /// Return the next pending viewer input command, if any.
    pub fn recv_input(&mut self) -> Option<InputCommand> {
        self.incoming_commands.pop_front()
    }

    /// Add a remote ICE candidate received from the browser via signaling.
    pub fn add_ice_candidate(&mut self, candidate: &str) -> Result<()> {
        let c = Candidate::from_sdp_string(candidate)
            .with_context(|| format!("failed to parse ICE candidate: {candidate}"))?;
        self.rtc.add_remote_candidate(c);
        tracing::debug!(session = %self.session_id, %candidate, "remote ICE candidate added");
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

    /// Push an encoded H.264 NAL unit to the video track.
    ///
    /// `pts_90khz` is the RTP timestamp in 90 kHz clock units.
    /// The NAL unit must be a raw NAL unit (no Annex-B start code).
    /// Returns silently if media hasn't been negotiated yet.
    pub fn push_video(&mut self, nal: &[u8], pts_90khz: u32, wallclock: Instant) -> Result<()> {
        let mid = match self.video_mid {
            Some(m) => m,
            None => return Ok(()),
        };

        let Some(writer) = self.rtc.writer(mid) else {
            return Ok(());
        };

        let Some(params) = writer.payload_params().nth(0) else {
            return Ok(());
        };
        let pt = params.pt();

        let rtp_time = MediaTime::from_90khz(pts_90khz as u64);

        writer.write(pt, wallclock, rtp_time, nal.to_vec())?;
        Ok(())
    }

    /// Push an encoded Opus audio frame to the audio track.
    ///
    /// `pts_48khz` is the RTP timestamp in 48 kHz clock units.
    pub fn push_audio(
        &mut self,
        opus_frame: &[u8],
        pts_48khz: u32,
        wallclock: Instant,
    ) -> Result<()> {
        let mid = match self.audio_mid {
            Some(m) => m,
            None => return Ok(()),
        };

        let Some(writer) = self.rtc.writer(mid) else {
            return Ok(());
        };

        let Some(params) = writer.payload_params().nth(0) else {
            return Ok(());
        };
        let pt = params.pt();

        let rtp_time = MediaTime::new(pts_48khz as u64, Frequency::FORTY_EIGHT_KHZ);

        writer.write(pt, wallclock, rtp_time, opus_frame.to_vec())?;
        Ok(())
    }
}
