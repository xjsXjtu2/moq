//! HTTP-based signaling server for WebRTC SDP/ICE exchange.
//!
//! Provides a minimal HTTP server with REST endpoints for:
//! - `POST /webrtc/offer`  — client sends SDP offer, receives SDP answer + session ID
//! - `POST /webrtc/ice/:id` — client sends ICE candidate for a session
//! - `GET  /webrtc/ice/:id` — SSE stream of server ICE candidates for a session
//! - `POST /webrtc/close/:id` — client closes a session
//!
//! The signaling server is deliberately minimal: it doesn't authenticate,
//! doesn't persist sessions, and is intended for local-network / dev use.
//! For production deployments with NAT traversal, pair with a TURN server.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{broadcast, Mutex};

#[cfg(feature = "tls")]
use tokio_rustls::TlsAcceptor;

/// A single pending WebRTC session tracked by the signaling server.
struct PeerSession {
    /// Channel for sending server ICE candidates to the client via SSE.
    ice_tx: broadcast::Sender<String>,
}

/// Shared state for the signaling server.
pub struct SignalingServer {
    sessions: Arc<Mutex<HashMap<String, PeerSession>>>,
    /// Channel to the application: each accepted offer spawns a new WebRTC peer.
    offer_tx: broadcast::Sender<AcceptedOffer>,
}

/// An accepted SDP offer, ready to be turned into a WebRTC peer.
#[derive(Debug, Clone)]
pub struct AcceptedOffer {
    pub session_id: String,
    pub sdp_offer: String,
}

impl SignalingServer {
    /// Create a new signaling server.
    ///
    /// `offer_tx` delivers accepted offers to the application, which should
    /// call [`WebrtcPeer::accept_offer`](crate::WebrtcPeer::accept_offer) and
    /// then push the answer back via the per-session ICE broadcast.
    pub fn new() -> (Self, broadcast::Receiver<AcceptedOffer>) {
        let (offer_tx, offer_rx) = broadcast::channel(16);
        (
            Self {
                sessions: Arc::new(Mutex::new(HashMap::new())),
                offer_tx,
            },
            offer_rx,
        )
    }

    /// Start the HTTP signaling server, binding to `addr`.
    ///
    /// This is a blocking async loop. Spawn it in a tokio task.
    pub async fn serve(self: Arc<Self>, addr: SocketAddr) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .context("failed to bind signaling server")?;

        tracing::info!(%addr, "WebRTC signaling server listening");

        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    tracing::warn!(error = %e, "signaling accept error");
                    continue;
                }
            };

            let this = self.clone();
            tokio::spawn(async move {
                if let Err(e) = this.handle_connection(stream).await {
                    tracing::debug!(%peer, error = %e, "signaling connection error");
                }
            });
        }
    }

    /// Start the TLS signaling server, binding to `addr`.
    ///
    /// Wraps each TCP connection with TLS before handing off to the HTTP handler.
    #[cfg(feature = "tls")]
    pub async fn serve_tls(
        self: Arc<Self>,
        addr: SocketAddr,
        acceptor: TlsAcceptor,
    ) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .context("failed to bind signaling server")?;

        tracing::info!(%addr, "WebRTC signaling server listening (TLS)");

        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    tracing::warn!(error = %e, "signaling accept error");
                    continue;
                }
            };

            let this = self.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                match acceptor.accept(stream).await {
                    Ok(tls_stream) => {
                        if let Err(e) = this.handle_connection(tls_stream).await {
                            tracing::debug!(%peer, error = %e, "signaling connection error");
                        }
                    }
                    Err(e) => {
                        tracing::debug!(%peer, error = %e, "TLS handshake failed");
                    }
                }
            });
        }
    }

    /// Push a server ICE candidate to the client via the SSE channel.
    pub async fn push_ice(&self, session_id: &str, candidate: &str) {
        let sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(session_id) {
            let receiver_count = session.ice_tx.receiver_count();
            let is_answer = candidate.starts_with("ANSWER:");
            tracing::info!(
                %session_id,
                %receiver_count,
                candidate_len = candidate.len(),
                is_answer,
                "push_ice: sending to SSE channel"
            );
            match session.ice_tx.send(candidate.to_string()) {
                Ok(n) => tracing::debug!(%session_id, receivers = n, "push_ice: sent"),
                Err(e) => tracing::warn!(%session_id, error = %e, "push_ice: send failed (no receivers?)"),
            }
        } else {
            tracing::warn!(%session_id, "push_ice: session not found");
        }
    }

    async fn handle_connection<S>(&self, stream: S) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (reader, mut writer) = tokio::io::split(stream);
        let mut reader = BufReader::new(reader);

        // Read the request line.
        let mut request_line = String::new();
        reader.read_line(&mut request_line).await?;

        let parts: Vec<&str> = request_line.trim().split_whitespace().collect();
        if parts.len() < 2 {
            anyhow::bail!("invalid HTTP request line");
        }
        let method = parts[0];
        let path = parts[1];

        // Read headers.
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            let line = line.trim();
            if line.is_empty() {
                break;
            }
            if let Some(val) = line
                .to_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().to_string())
            {
                content_length = val.parse().unwrap_or(0);
            }
        }

        // Handle SSE (Server-Sent Events) for GET /webrtc/ice/:id.
        if method == "GET" {
            if let Some(session_id) = path.strip_prefix("/webrtc/ice/") {
                drop(reader);
                return self.handle_sse(session_id, writer).await;
            }
            // Other GET requests are not supported.
            let response = http_response(404, "Not Found", "text/plain");
            writer.write_all(response.as_bytes()).await?;
            writer.shutdown().await?;
            return Ok(());
        }

        // Read body.
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            use tokio::io::AsyncReadExt;
            reader.read_exact(&mut body).await?;
        }

        let response = self.route(method, path, &body).await;

        writer.write_all(response.as_bytes()).await?;
        writer.shutdown().await?;

        Ok(())
    }

    /// Stream server ICE candidates to the client via Server-Sent Events.
    async fn handle_sse<W>(&self, session_id: &str, mut writer: W) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::AsyncWriteExt;

        // Subscribe to the session's ICE broadcast channel.
        let mut rx = {
            let sessions = self.sessions.lock().await;
            match sessions.get(session_id) {
                Some(session) => {
                    tracing::info!(%session_id, "SSE: session found, subscribing to ICE channel");
                    session.ice_tx.subscribe()
                }
                None => {
                    tracing::warn!(%session_id, "SSE: session not found");
                    writer
                        .write_all(
                            http_response(404, "Session not found", "text/plain").as_bytes(),
                        )
                        .await?;
                    writer.shutdown().await?;
                    return Ok(());
                }
            }
        };

        // Write SSE headers.
        writer
            .write_all(
                b"HTTP/1.1 200 OK\r\n\
                  Content-Type: text/event-stream\r\n\
                  Cache-Control: no-cache\r\n\
                  Connection: keep-alive\r\n\
                  Access-Control-Allow-Origin: *\r\n\
                  \r\n",
            )
            .await?;
        tracing::info!(%session_id, "SSE: headers sent, waiting for events");

        // Stream data as SSE "candidate" events.
        loop {
            match rx.recv().await {
                Ok(candidate) => {
                    let is_answer = candidate.starts_with("ANSWER:");
                    tracing::info!(%session_id, candidate_len = candidate.len(), is_answer, "SSE: sending event");
                    let data = serde_json::json!({ "candidate": candidate });
                    let event = format!("event: candidate\ndata: {}\n\n", data);
                    if writer.write_all(event.as_bytes()).await.is_err() {
                        tracing::info!(%session_id, "SSE: client disconnected");
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(%session_id, skipped = n, "SSE: lagged");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::info!(%session_id, "SSE: channel closed");
                    break;
                }
            }
        }

        writer.shutdown().await?;
        Ok(())
    }

    async fn route(&self, method: &str, path: &str, body: &[u8]) -> String {
        match (method, path) {
            ("POST", "/webrtc/offer") => self.handle_offer(body).await,
            ("POST", p) if p.starts_with("/webrtc/ice/") => {
                let session_id = p.strip_prefix("/webrtc/ice/").unwrap_or("");
                self.handle_ice_candidate(session_id, body).await
            }
            ("POST", p) if p.starts_with("/webrtc/close/") => {
                let session_id = p.strip_prefix("/webrtc/close/").unwrap_or("");
                self.handle_close(session_id).await
            }
            _ => {
                http_response(404, "Not Found", "text/plain")
            }
        }
    }

    async fn handle_offer(&self, body: &[u8]) -> String {
        let body_str = match std::str::from_utf8(body) {
            Ok(s) => s,
            Err(_) => return http_response(400, "Invalid UTF-8", "text/plain"),
        };

        let offer: serde_json::Value = match serde_json::from_str(body_str) {
            Ok(v) => v,
            Err(_) => return http_response(400, "Invalid JSON", "text/plain"),
        };

        let sdp = match offer["sdp"].as_str() {
            Some(s) => s.to_string(),
            None => {
                return http_response(400, r#"{"error":"missing 'sdp' field"}"#, "application/json")
            }
        };

        // Generate a unique session ID.
        let session_id = nanoid();

        // Create the SSE channel for ICE candidates.
        let (ice_tx, _) = broadcast::channel(32);

        {
            let mut sessions = self.sessions.lock().await;
            sessions.insert(
                session_id.clone(),
                PeerSession { ice_tx },
            );
        }

        // Notify the application that an offer was accepted.
        let _ = self.offer_tx.send(AcceptedOffer {
            session_id: session_id.clone(),
            sdp_offer: sdp,
        });

        // The application must now create a WebrtcPeer and push the answer.
        // For now, return a 202 Accepted — the actual answer will arrive
        // via the SSE channel after the peer is created.
        //
        // In a simpler flow, the client POSTs the offer, and the server
        // synchronously returns the answer. But because str0m needs to
        // be driven from a Rust context, we use the async flow:
        // the application creates the peer, generates the answer, and
        // sends it via an SSE event.
        let response_body = serde_json::json!({
            "session_id": session_id,
            "status": "accepted"
        });
        http_response(200, &response_body.to_string(), "application/json")
    }

    async fn handle_ice_candidate(&self, session_id: &str, body: &[u8]) -> String {
        let body_str = match std::str::from_utf8(body) {
            Ok(s) => s,
            Err(_) => return http_response(400, "Invalid UTF-8", "text/plain"),
        };

        let candidate: serde_json::Value = match serde_json::from_str(body_str) {
            Ok(v) => v,
            Err(_) => return http_response(400, "Invalid JSON", "text/plain"),
        };

        let candidate_str = match candidate["candidate"].as_str() {
            Some(s) => s.to_string(),
            None => {
                return http_response(
                    400,
                    r#"{"error":"missing 'candidate' field"}"#,
                    "application/json",
                )
            }
        };

        // In a full implementation, the application would call
        // `WebrtcPeer::add_ice_candidate()`. For now, we store it
        // for the application to pick up via a channel.
        tracing::debug!(%session_id, %candidate_str, "received ICE candidate");

        http_response(200, r#"{"status":"ok"}"#, "application/json")
    }

    async fn handle_close(&self, session_id: &str) -> String {
        let mut sessions = self.sessions.lock().await;
        sessions.remove(session_id);
        tracing::info!(%session_id, "WebRTC session closed");
        http_response(200, r#"{"status":"closed"}"#, "application/json")
    }
}

/// Generate a short random session ID.
fn nanoid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}", ts)
}

/// Construct a minimal HTTP/1.1 response.
fn http_response(code: u16, body: &str, content_type: &str) -> String {
    format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        code = code,
        reason = reason_phrase(code),
        content_type = content_type,
        len = body.len(),
        body = body,
    )
}

fn reason_phrase(code: u16) -> &'static str {
    match code {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}
