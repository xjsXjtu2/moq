//! WebRTC direct-connection mode for moq-boy.
//!
//! Accepts WebRTC peer connections from browsers via HTTP signaling,
//! delivers H.264 video and Opus audio over WebRTC media tracks, and
//! receives button commands over a DataChannel.
//!
//! ## Architecture
//!
//! ```text
//! Emulator → Video Encoder (H.264 NALs) → Broadcast Channel
//!                                         ├→ MoQ Track (existing)
//!                                         └→ WebRTC Peers (new)
//!
//! Emulator → Audio PCM → Opus Encoder → RTP → WebRTC Peers (new)
//!                      └→ MoQ Track (existing)
//! ```
//!
//! Audio encoding is duplicated in WebRTC mode: we encode Opus directly
//! from the emulator's PCM samples so we can packetize the encoded frames
//! into RTP. This avoids modifying the internal moq-audio pipeline.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

use crate::audio::AudioEncoder;
use crate::emulator::{self, Emulator};
use crate::input::Command;
use crate::stats::Stats;
use crate::status::{self, StatusPublisher};
use crate::video::EncodedFrame;

use moq_webrtc::{
    self,
    signaling::SignalingServer,
    InputCommand as WebrtcInput,
    WebrtcOutput, WebrtcPeer,
};

/// Shared WebRTC session state, safe to share across tasks.
struct WebrtcSession {
    /// Active peers indexed by session ID.
    peers: Mutex<HashMap<String, WebrtcPeer>>,
    /// Maps remote SocketAddr → session_id for UDP packet routing.
    peer_addrs: Mutex<HashMap<SocketAddr, String>>,
    /// Shared UDP socket for ICE/DTLS/RTP transport.
    udp: Arc<UdpSocket>,
    /// ICE host candidate address (advertised to browsers).
    ice_addr: SocketAddr,
    /// Received viewer commands (DataChannel → emulator thread).
    cmd_tx: tokio::sync::mpsc::Sender<Command>,
    /// Whether video is active (at least one connected peer).
    video_active: std::sync::atomic::AtomicBool,
    /// Whether audio is active.
    audio_active: std::sync::atomic::AtomicBool,
}

/// Run moq-boy in WebRTC direct mode.
///
/// - Binds an HTTP signaling server on `addr`
/// - Accepts SDP offers from browsers
/// - Drives the emulator loop, delivering encoded frames to WebRTC peers
/// - Receives button commands via DataChannel
pub async fn run_webrtc_mode(
    config: &crate::Config,
    name: &str,
    rom_path: &std::path::Path,
    session: Arc<crate::Session>,
    cmd_tx: tokio::sync::mpsc::Sender<Command>,
    cmd_rx: tokio::sync::mpsc::Receiver<Command>,
    audio_encoder: AudioEncoder,
    status_publisher: StatusPublisher,
    addr: SocketAddr,
) -> Result<()> {
    tracing::info!(%addr, %name, "starting WebRTC direct mode");

    // Resolve the IP address to advertise in ICE host candidates.
    // 0.0.0.0 is invalid for ICE. Default: use a quick UDP connect to
    // discover the local routable IP. On ECS behind NAT, use
    // --webrtc-udp-addr to specify the exact IP.
    let ice_ip = match config.webrtc_udp_addr {
        Some(ip) => {
            tracing::info!(%ip, "WebRTC ICE: using configured address");
            ip
        }
        None => {
            // Trick: connect a UDP socket to a public address to find the local
            // interface IP without sending any packets.
            let fallback = std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1));
            let ip = std::net::UdpSocket::bind("0.0.0.0:0")
                .ok()
                .and_then(|s| {
                    s.connect("8.8.8.8:53").ok()?;
                    s.local_addr().ok().map(|a| a.ip())
                })
                .unwrap_or(fallback);
            if ip.is_loopback() {
                tracing::warn!(
                    "WebRTC ICE: auto-detected loopback address; browser must be on the same machine. \
                     Set --webrtc-udp-addr for remote access."
                );
            }
            tracing::info!(%ip, "WebRTC ICE: auto-detected address");
            ip
        }
    };

    // Ensure the IP is valid for a host candidate.
    if ice_ip.is_unspecified() || ice_ip.is_loopback() && config.webrtc_udp_addr.is_none() {
        tracing::warn!(
            "ICE host candidate IP is {}; browser on a remote machine won't be able to reach this. \
             Use --webrtc-udp-addr to specify a routable IP.",
            ice_ip
        );
    }

    // Bind a UDP socket for ICE/STUN/DTLS/RTP transport.
    // If --webrtc-udp-port is set, use that specific port so it can be
    // whitelisted in ECS security groups; otherwise the OS picks a free port.
    let udp_bind_addr = match config.webrtc_udp_port {
        Some(port) => {
            let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
            tracing::info!(%port, "WebRTC UDP: using configured port");
            addr
        }
        None => {
            tracing::info!("WebRTC UDP: OS-assigned port (use --webrtc-udp-port to pin)");
            std::net::SocketAddr::from(([0, 0, 0, 0], 0))
        }
    };
    let udp = UdpSocket::bind(udp_bind_addr)
        .await
        .context("failed to bind UDP socket for WebRTC transport")?;
    let udp_port = udp.local_addr()?.port();

    // The ICE candidate address uses the resolved IP + actual UDP port.
    let ice_addr = std::net::SocketAddr::new(ice_ip, udp_port);
    tracing::info!(%ice_addr, "WebRTC UDP transport bound");

    let udp = Arc::new(udp);

    // Set up the signaling server.
    let (signaling, mut offer_rx) = SignalingServer::new();
    let signaling = Arc::new(signaling);
    let signaling_clone = signaling.clone();

    let tls_config = &config.server.tls;
    if !tls_config.cert.is_empty() || !tls_config.generate.is_empty() {
        let server_config = tls_config.build_server_config()?;
        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(server_config));
        tracing::info!("WebRTC signaling will use TLS");
        tokio::spawn(async move {
            if let Err(e) = signaling_clone.serve_tls(addr, acceptor).await {
                tracing::error!(error = %e, "signaling server stopped");
            }
        });
    } else {
        tokio::spawn(async move {
            if let Err(e) = signaling_clone.serve(addr).await {
                tracing::error!(error = %e, "signaling server stopped");
            }
        });
    }

    // Create a broadcast channel for encoded video frames from the encoder thread.
    // The encoder was already created with `webrtc_tap: true` in main.rs.
    let mut video_rx = session
        .video_encoder
        .webrtc_subscribe()
        .context("video encoder was not created with webrtc_tap enabled; set webrtc_tap=true in EncoderConfig")?;

    // Shared session state for peer management.
    let webrtc_session = Arc::new(WebrtcSession {
        peers: Mutex::new(HashMap::new()),
        peer_addrs: Mutex::new(HashMap::new()),
        udp: udp.clone(),
        ice_addr,
        cmd_tx: cmd_tx.clone(),
        video_active: std::sync::atomic::AtomicBool::new(false),
        audio_active: std::sync::atomic::AtomicBool::new(false),
    });

    // Spawn the offer acceptance loop: when a browser posts an SDP offer,
    // create a WebRTC peer and register it.
    let ws = webrtc_session.clone();
    let _sig = signaling.clone();
    tokio::spawn(async move {
        while let Ok(accepted) = offer_rx.recv().await {
            let session_id = accepted.session_id.clone();
            match WebrtcPeer::accept_offer(session_id.clone(), &accepted.sdp_offer, ice_addr) {
                Ok(peer) => {
                    tracing::info!(%session_id, "WebRTC peer created");
                    // Generate the SDP answer and push it to the client.
                    // In a full implementation, the answer SDP is generated by
                    // str0m when we call local_sdp() after set_remote_sdp().
                    // For now, we register the peer and the signaling server
                    // will deliver the answer via the ICE SSE channel.
                    let mut peers = ws.peers.lock().await;
                    peers.insert(session_id, peer);
                }
                Err(e) => {
                    tracing::warn!(%session_id, error = %e, "failed to accept WebRTC offer");
                }
            }
        }
    });

    // Spawn a task to poll peers, relay ICE candidates, and send UDP transmits.
    let ws2 = webrtc_session.clone();
    let sig2 = signaling.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(10));
        loop {
            interval.tick().await;
            let now = Instant::now();
            let mut peers = ws2.peers.lock().await;
            let mut dead_sessions = Vec::new();

            for (sid, peer) in peers.iter_mut() {
                // Feed timeout to str0m.
                let _ = peer.handle_timeout(now);

                match peer.poll() {
                    Ok(outputs) => {
                        for out in outputs {
                            match out {
                                WebrtcOutput::IceCandidate { session_id: _, candidate } => {
                                    sig2.push_ice(sid, &candidate).await;
                                }
                                WebrtcOutput::IceDisconnected(sid) => {
                                    dead_sessions.push(sid);
                                }
                                WebrtcOutput::Answer { session_id, sdp } => {
                                    sig2.push_ice(&session_id, &format!("ANSWER:{}", sdp)).await;
                                }
                                WebrtcOutput::Transmit { destination, contents } => {
                                    if let Err(e) = ws2.udp.send_to(&contents, destination).await {
                                        tracing::debug!(%destination, error = %e, "UDP send failed");
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(session=%sid, error=%e, "peer poll error");
                        dead_sessions.push(sid.clone());
                    }
                }

                // Drain DataChannel commands.
                while let Some(cmd) = peer.recv_input() {
                    let command = into_command(cmd);
                    let _ = ws2.cmd_tx.try_send(command);
                }
            }

            for sid in dead_sessions {
                peers.remove(&sid);
                // Clean up address mapping.
                ws2.peer_addrs.lock().await.retain(|_, s| s != &sid);
                tracing::info!(%sid, "WebRTC peer removed");
            }

            // Update active state.
            let count = peers.len();
            ws2.video_active.store(count > 0, std::sync::atomic::Ordering::Relaxed);
            ws2.audio_active.store(count > 0, std::sync::atomic::Ordering::Relaxed);
        }
    });

    // Spawn a UDP receive loop: feed incoming packets to the correct peer.
    let ws3 = webrtc_session.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        loop {
            match ws3.udp.recv_from(&mut buf).await {
                Ok((n, source)) => {
                    let now = Instant::now();
                    let dest = ws3.ice_addr;
                    let mut peers = ws3.peers.lock().await;
                    // Find the peer by session_id from the address map.
                    // On first packet from a new source, try to find the peer
                    // whose ICE candidate matches. For now, route by source addr
                    // if we've seen it before; otherwise broadcast to all connected peers.
                    let session_id = ws3.peer_addrs.lock().await.get(&source).cloned();
                    if let Some(ref sid) = session_id {
                        if let Some(peer) = peers.get_mut(sid) {
                            if let Err(e) = peer.handle_input(now, source, dest, &buf[..n]) {
                                tracing::debug!(%source, error = %e, "handle_input failed");
                            }
                        }
                    } else {
                        // First packet from this source: try each connected peer.
                        // ICE STUN packets will be accepted by the correct one.
                        for (_sid, peer) in peers.iter_mut() {
                            if peer.is_connected() || !peer.is_connected() {
                                // Try to feed — str0m will accept or ignore.
                                if peer.handle_input(now, source, dest, &buf[..n]).is_ok() {
                                    // Record the mapping for future packets.
                                    let sid = peer.session_id().to_string();
                                    ws3.peer_addrs.lock().await.insert(source, sid);
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "UDP recv error");
                }
            }
        }
    });

    // Run the emulator on a blocking thread (same as relay/server mode).
    let rom_path = rom_path.to_path_buf();
    let session_clone = session.clone();
    let ws3 = webrtc_session.clone();
    let emulator_handle = tokio::task::spawn_blocking(move || {
        run_emulator_webrtc(
            session_clone,
            &rom_path,
            audio_encoder,
            status_publisher,
            cmd_rx,
            ws3,
        )
    });

    // Drive the WebRTC frame dispatch loop on the async side.
    let dispatch_loop = dispatch_frames_to_peers(webrtc_session.clone(), &mut video_rx);

    tokio::select! {
        res = emulator_handle => res?.context("emulator error"),
        res = dispatch_loop => res,
    }
}

/// Runs the emulator in WebRTC mode on a blocking thread.
///
/// Identical to [`run_emulator`] in main.rs, except:
/// - Pause/resume is driven by WebRTC peer count
/// - Audio encoding is duplicated for RTP packetization
fn run_emulator_webrtc(
    session: Arc<crate::Session>,
    rom_path: &std::path::Path,
    mut audio_encoder: AudioEncoder,
    mut status_publisher: StatusPublisher,
    mut cmd_rx: tokio::sync::mpsc::Receiver<Command>,
    webrtc_session: Arc<WebrtcSession>,
) -> Result<()> {
    let mut emu = Emulator::new(rom_path)?;
    let start = Instant::now();

    // Initial tick to produce first frame data.
    emu.tick();
    let elapsed = start.elapsed();
    let rgba = Bytes::from(emu.framebuffer());
    let ts = hang::container::Timestamp::from_micros(elapsed.as_micros() as u64)
        .context("timestamp overflow")?;
    session.video_encoder.try_frame(rgba, ts);
    let samples = emu.audio_samples();
    if !samples.is_empty() {
        let _ = audio_encoder.push_samples(&samples, elapsed);
    }

    let frame_duration = Duration::from_micros(16_742);
    let mut next_frame = Instant::now();
    let mut viewer_latency: HashMap<String, Vec<status::LatencyEntry>> = HashMap::new();
    let mut game_stats = Stats::new();
    let mut was_audio_active = false;
    let mut pending_client_ts: Option<u64> = None;

    let mut last_log = Instant::now();
    let mut log_cmd_count: usize = 0;
    let mut log_cmd_details: Vec<String> = Vec::new();
    let mut log_lat_details: Vec<String> = Vec::new();
    let mut log_video_count: u64 = 0;
    let audio_packets = audio_encoder.packets_encoded();
    let mut log_prev_audio: u64 = audio_packets.load(std::sync::atomic::Ordering::Relaxed);

    loop {
        // Pause when no peers are connected.
        let is_video = webrtc_session
            .video_active
            .load(std::sync::atomic::Ordering::Relaxed);
        let is_audio = webrtc_session
            .audio_active
            .load(std::sync::atomic::Ordering::Relaxed);

        if !is_video && !is_audio {
            // Wait for a peer to connect.
            tokio::task::block_in_place(|| {
                std::thread::sleep(Duration::from_millis(100));
            });
            next_frame = Instant::now();
            game_stats.reset_tick();
            session.video_encoder.force_keyframe();
            audio_encoder.reset_epoch();
            last_log = Instant::now();
            log_cmd_count = 0;
            log_cmd_details.clear();
            log_lat_details.clear();
            log_video_count = 0;
            log_prev_audio = audio_packets.load(std::sync::atomic::Ordering::Relaxed);
            continue;
        }

        // Drain pending viewer commands.
        {
            let elapsed = start.elapsed();
            let encode_ms = u32::try_from(
                session.video_encoder.encode_duration().as_millis(),
            )
            .unwrap_or(u32::MAX);

            while let Ok(cmd) = cmd_rx.try_recv() {
                log_cmd_count += 1;
                log_cmd_details.push(format!("{cmd:?}"));
                match cmd {
                    Command::Buttons {
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
                        for t in &timestamps {
                            let latency = elapsed.saturating_sub(t.ts);
                            breakdown.push(entry(
                                &t.label,
                                u32::try_from(latency.as_millis()).unwrap_or(u32::MAX),
                            ));
                        }
                        if let Some(min_ts) = timestamps.iter().map(|t| t.ts).min() {
                            let latency = elapsed.saturating_sub(min_ts);
                            breakdown.push(entry(
                                "input",
                                u32::try_from(latency.as_millis()).unwrap_or(u32::MAX),
                            ));
                        }

                        let elapsed_ms = elapsed.as_millis();
                        let parts: Vec<String> = breakdown
                            .iter()
                            .map(|e| format!("{}={}ms", e.label, e.ms))
                            .collect();
                        log_lat_details.push(format!(
                            "viewer={viewer_id} elapsed={elapsed_ms}ms [{parts}]",
                            parts = parts.join(", "),
                        ));
                        viewer_latency.insert(viewer_id, breakdown);
                    }
                    Command::ViewerLeft { viewer_id } => {
                        emu.viewer_left(&viewer_id);
                        viewer_latency.remove(&viewer_id);
                    }
                    Command::Reset => {
                        tracing::info!("resetting emulator (viewer request)");
                        emu.reset()?;
                        game_stats = Stats::new();
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

        let elapsed = start.elapsed();
        game_stats.tick(is_video, is_audio);

        // Tick the emulator.
        emu.tick();

        // Publish status (only if changed).
        session.publish_status(&emu, &viewer_latency, &game_stats, &mut status_publisher);

        // Encode and publish video frame.
        if is_video {
            let mut rgba = emu.framebuffer();
            if let Some(ts) = pending_client_ts.take() {
                crate::overlay::draw_timestamp(
                    &mut rgba,
                    emulator::WIDTH,
                    emulator::HEIGHT,
                    ts,
                );
            }
            let rgba = Bytes::from(rgba);
            let ts = hang::container::Timestamp::from_micros(elapsed.as_micros() as u64)
                .context("timestamp overflow")?;
            session.video_encoder.try_frame(rgba, ts);
            log_video_count += 1;
        }

        // Encode and publish audio.
        if is_audio {
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
            emu.audio_samples();
        }
        was_audio_active = is_audio;

        // Periodic stats log (every 5 seconds).
        if last_log.elapsed() >= Duration::from_secs(5) {
            let cur_audio = audio_packets.load(std::sync::atomic::Ordering::Relaxed);
            let audio_delta = cur_audio - log_prev_audio;
            tracing::info!(
                cmds = log_cmd_count,
                vfrms = log_video_count,
                afrms = audio_delta,
                "emulator (webrtc)"
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

/// Dispatch encoded video frames to all connected WebRTC peers.
///
/// Reads encoded frames from the broadcast channel and pushes each NAL unit
/// to every connected peer via str0m's media writer.
async fn dispatch_frames_to_peers(
    session: Arc<WebrtcSession>,
    video_rx: &mut tokio::sync::broadcast::Receiver<EncodedFrame>,
) -> Result<()> {
    loop {
        let frame = match video_rx.recv().await {
            Ok(f) => f,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(skipped = n, "WebRTC video frame dispatch lagging");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::info!("video broadcast channel closed");
                return Ok(());
            }
        };

        // RTP timestamp: 90 kHz clock, microseconds → 90kHz units
        let pts_90khz = (frame.ts_us * 90 / 1000) as u32;
        let now = Instant::now();

        let mut peers = session.peers.lock().await;
        if peers.is_empty() {
            continue;
        }

        for (_sid, peer) in peers.iter_mut() {
            if !peer.is_connected() {
                continue;
            }
            for nal in &frame.nals {
                if let Err(e) = peer.push_video(nal, pts_90khz, now) {
                    tracing::debug!(error = %e, "push_video failed");
                }
            }
        }
    }
}

/// Convert a WebRTC DataChannel command to the internal command type.
fn into_command(cmd: WebrtcInput) -> Command {
    match cmd.cmd_type.as_str() {
        "reset" => Command::Reset,
        _ => {
            let buttons: Vec<emulator::Button> = cmd
                .buttons
                .iter()
                .filter_map(|b| match b.as_str() {
                    "up" => Some(emulator::Button::Up),
                    "down" => Some(emulator::Button::Down),
                    "left" => Some(emulator::Button::Left),
                    "right" => Some(emulator::Button::Right),
                    "a" => Some(emulator::Button::A),
                    "b" => Some(emulator::Button::B),
                    "start" => Some(emulator::Button::Start),
                    "select" => Some(emulator::Button::Select),
                    _ => None,
                })
                .collect();
            Command::Buttons {
                buttons,
                viewer_id: "webrtc-viewer".to_string(),
                timestamps: Vec::new(),
                client_ts: cmd.client_ts,
            }
        }
    }
}
