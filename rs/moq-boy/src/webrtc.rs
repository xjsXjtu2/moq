//! WebRTC direct-connection mode for moq-boy (ICE-Lite).
//!
//! The server has a known public IP:port. It uses ICE-Lite: a single
//! host candidate is baked into the SDP answer. Signaling is one HTTP
//! round-trip — the browser POSTs an SDP offer, the server creates a
//! peer and returns the SDP answer directly. No trickle ICE, no SSE.
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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::net::UdpSocket;

use crate::audio::{self, AudioEncoder};
use crate::emulator::{self, Emulator};
use crate::input::Command;
use crate::stats::Stats;
use crate::status::{self, StatusPublisher};
use crate::video::EncodedFrame;

use moq_webrtc::{
    signaling::SignalingServer,
    InputCommand as WebrtcInput,
    WebrtcOutput, WebrtcPeer,
};

/// Shared WebRTC session state.
struct WebrtcSession {
    /// Active peers indexed by session ID. std::sync::Mutex so callbacks
    /// from the signaling server (which run synchronously in the HTTP handler)
    /// can lock and insert without awaiting.
    peers: StdMutex<HashMap<String, WebrtcPeer>>,
    /// Maps remote SocketAddr → session_id for UDP packet routing.
    peer_addrs: StdMutex<HashMap<SocketAddr, String>>,
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
    audio_rx: Option<tokio::sync::broadcast::Receiver<audio::EncodedAudio>>,
) -> Result<()> {
    tracing::info!(%addr, %name, "starting WebRTC direct mode (ICE-Lite)");

    // Resolve the IP address to advertise in the ICE host candidate.
    let ice_ip = match config.webrtc_udp_addr {
        Some(ip) => {
            tracing::info!(%ip, "WebRTC ICE: using configured address");
            ip
        }
        None => {
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
                    "WebRTC ICE: auto-detected loopback address; use --webrtc-udp-addr for remote access"
                );
            }
            tracing::info!(%ip, "WebRTC ICE: auto-detected address");
            ip
        }
    };

    if ice_ip.is_unspecified() || (ice_ip.is_loopback() && config.webrtc_udp_addr.is_none()) {
        tracing::warn!(
            "ICE host candidate IP is {}; use --webrtc-udp-addr to specify a routable IP",
            ice_ip
        );
    }

    // Bind a UDP socket for ICE/STUN/DTLS/RTP transport.
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

    let ice_addr = std::net::SocketAddr::new(ice_ip, udp_port);
    tracing::info!(%ice_addr, "WebRTC UDP transport bound");

    let udp = Arc::new(udp);

    // Shared session state for peer management.
    let webrtc_session = Arc::new(WebrtcSession {
        peers: StdMutex::new(HashMap::new()),
        peer_addrs: StdMutex::new(HashMap::new()),
        udp: udp.clone(),
        ice_addr,
        cmd_tx: cmd_tx.clone(),
        video_active: std::sync::atomic::AtomicBool::new(false),
        audio_active: std::sync::atomic::AtomicBool::new(false),
    });

    // Build the signaling server callbacks.
    let ws_offer = webrtc_session.clone();
    let ws_ice = webrtc_session.clone();
    let ws_close = webrtc_session.clone();

    let on_offer = Arc::new(move |session_id: &str, sdp_offer: &str| -> std::result::Result<String, String> {
        let mut peer = WebrtcPeer::accept_offer(
            session_id.to_string(),
            sdp_offer,
            ws_offer.ice_addr,
        )
        .map_err(|e| format!("{e:#}"))?;

        // Extract the SDP answer. WebrtcPeer stores it after accept_offer.
        // Poll once to get the answer out.
        let answer = match peer.poll().map_err(|e| format!("{e:#}"))? {
            outputs => {
                let mut answer_sdp = None;
                for out in outputs {
                    if let WebrtcOutput::Answer { sdp, .. } = out {
                        answer_sdp = Some(sdp);
                    }
                }
                answer_sdp.ok_or_else(|| "no answer generated".to_string())?
            }
        };

        ws_offer.peers.lock().unwrap().insert(session_id.to_string(), peer);
        tracing::info!(%session_id, "WebRTC peer created (ICE-Lite)");
        Ok(answer)
    });

    let on_ice = Arc::new(move |session_id: &str, candidate: &str| -> std::result::Result<(), String> {
        let mut peers = ws_ice.peers.lock().unwrap();
        let peer = peers.get_mut(session_id).ok_or_else(|| "session not found".to_string())?;
        peer.add_ice_candidate(candidate).map_err(|e| format!("{e:#}"))
    });

    let on_close = Arc::new(move |session_id: &str| {
        let mut peers = ws_close.peers.lock().unwrap();
        peers.remove(session_id);
        tracing::info!(%session_id, "WebRTC peer closed by client");
    });

    let signaling = Arc::new(SignalingServer::new(on_offer, on_ice, on_close));

    // Start the signaling server.
    let tls_config = &config.server.tls;
    let signaling_clone = signaling.clone();
    if !tls_config.cert.is_empty() || !tls_config.generate.is_empty() {
        let server_config = tls_config.build_server_config()?;
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
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

    // Create a broadcast channel for encoded video frames.
    let mut video_rx = session
        .video_encoder
        .webrtc_subscribe()
        .context("video encoder was not created with webrtc_tap enabled")?;

    // Spawn a task to poll peers: timeouts, ICE state, DataChannel, UDP transmits.
    let ws_poll = webrtc_session.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(10));
        loop {
            interval.tick().await;
            let now = Instant::now();

            // Collect UDP transmits while holding the lock, then send
            // after releasing it (MutexGuard is !Send, can't hold across await).
            let mut transmits = Vec::new();

            {
                let mut peers = ws_poll.peers.lock().unwrap();
                let mut dead_sessions = Vec::new();

                for (sid, peer) in peers.iter_mut() {
                    let _ = peer.handle_timeout(now);

                    match peer.poll() {
                        Ok(outputs) => {
                            for out in outputs {
                                match out {
                                    WebrtcOutput::Transmit { destination, contents } => {
                                        transmits.push((destination, contents));
                                    }
                                    WebrtcOutput::IceDisconnected(sid) => {
                                        dead_sessions.push(sid);
                                    }
                                    _ => {}
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
                        let _ = ws_poll.cmd_tx.try_send(command);
                    }
                }

                for sid in dead_sessions {
                    peers.remove(&sid);
                    ws_poll.peer_addrs.lock().unwrap().retain(|_, s| s != &sid);
                    tracing::info!(%sid, "WebRTC peer removed");
                }

                let count = peers.len();
                ws_poll.video_active.store(count > 0, std::sync::atomic::Ordering::Relaxed);
                ws_poll.audio_active.store(count > 0, std::sync::atomic::Ordering::Relaxed);
            } // MutexGuard dropped here.

            // Send UDP transmits outside the lock.
            for (destination, contents) in transmits {
                if let Err(e) = ws_poll.udp.send_to(&contents, destination).await {
                    tracing::debug!(%destination, error = %e, "UDP send failed");
                }
            }
        }
    });

    // Spawn a UDP receive loop.
    let ws_recv = webrtc_session.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        loop {
            match ws_recv.udp.recv_from(&mut buf).await {
                Ok((n, source)) => {
                    let now = Instant::now();
                    let dest = ws_recv.ice_addr;

                    // Check the address map first (separate lock).
                    let session_id = ws_recv.peer_addrs.lock().unwrap().get(&source).cloned();

                    let mut peers = ws_recv.peers.lock().unwrap();
                    if let Some(ref sid) = session_id {
                        if let Some(peer) = peers.get_mut(sid) {
                            let _ = peer.handle_input(now, source, dest, &buf[..n]);
                        }
                    } else {
                        // First packet from this source: try each connected peer.
                        let matched_sid = {
                            let mut found = None;
                            for (_sid, peer) in peers.iter_mut() {
                                if peer.handle_input(now, source, dest, &buf[..n]).is_ok() {
                                    found = Some(peer.session_id().to_string());
                                    break;
                                }
                            }
                            found
                        };
                        if let Some(sid) = matched_sid {
                            drop(peers);
                            ws_recv.peer_addrs.lock().unwrap().insert(source, sid);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "UDP recv error");
                }
            }
        }
    });

    // Run the emulator on a blocking thread.
    let rom_path = rom_path.to_path_buf();
    let session_clone = session.clone();
    let ws_emu = webrtc_session.clone();
    let emulator_handle = tokio::task::spawn_blocking(move || {
        run_emulator_webrtc(
            session_clone,
            &rom_path,
            audio_encoder,
            status_publisher,
            cmd_rx,
            ws_emu,
        )
    });

    // Drive the WebRTC video frame dispatch loop on the async side.
    let video_dispatch = dispatch_frames_to_peers(webrtc_session.clone(), &mut video_rx);

    // Drive the WebRTC audio dispatch loop if we have a tap.
    let audio_dispatch = if let Some(rx) = audio_rx {
        let audio_fut = dispatch_audio_to_peers(webrtc_session.clone(), rx);
        tokio::spawn(async move {
            if let Err(e) = audio_fut.await {
                tracing::error!(error = %e, "audio dispatch failed");
            }
        });
        // The audio dispatch is spawned separately so we select on both
        // emulator and video dispatch; audio runs in the background.
        std::future::pending::<Result<()>>()
    } else {
        std::future::pending::<Result<()>>()
    };

    tokio::select! {
        res = emulator_handle => res?.context("emulator error"),
        res = video_dispatch => res,
        res = audio_dispatch => res,
    }
}

/// Runs the emulator in WebRTC mode on a blocking thread.
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
                            "viewer={} elapsed={}ms [{}]",
                            viewer_id, elapsed_ms, parts.join(", "),
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

        // Publish status.
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

/// Dispatch encoded video frames to all connected WebRTC peers via str0m.
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

        let pts_90khz = (frame.ts_us * 90 / 1000) as u32;
        let now = Instant::now();

        let mut peers = session.peers.lock().unwrap();
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

/// Dispatch encoded audio frames to all connected WebRTC peers via str0m.
async fn dispatch_audio_to_peers(
    session: Arc<WebrtcSession>,
    mut audio_rx: tokio::sync::broadcast::Receiver<audio::EncodedAudio>,
) -> Result<()> {
    loop {
        let frame = match audio_rx.recv().await {
            Ok(f) => f,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(skipped = n, "WebRTC audio dispatch lagging");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::info!("audio broadcast channel closed");
                return Ok(());
            }
        };

        // RTP timestamp: 48 kHz clock, microseconds → 48kHz units
        let pts_48khz = (frame.ts_us * 48 / 1000) as u32;
        let now = Instant::now();

        let mut peers = session.peers.lock().unwrap();
        for (_sid, peer) in peers.iter_mut() {
            if !peer.is_connected() {
                continue;
            }
            if let Err(e) = peer.push_audio(&frame.data, pts_48khz, now) {
                tracing::debug!(error = %e, "push_audio failed");
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
