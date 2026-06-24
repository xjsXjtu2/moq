//! HTTP-based signaling server for WebRTC SDP/ICE exchange (ICE-Lite mode).
//!
//! In ICE-Lite (server with known public IP:port), signaling is a single
//! round-trip: the browser POSTs its SDP offer, the server creates a peer
//! and returns the SDP answer directly in the HTTP response. No SSE is needed
//! because the server's host candidate is baked into the SDP answer.
//!
//! Endpoints:
//! - `POST /webrtc/offer`  — client sends SDP offer, receives SDP answer
//! - `POST /webrtc/ice/:id` — client sends ICE candidate (browser trickle)
//! - `POST /webrtc/close/:id` — client closes a session

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};

#[cfg(feature = "tls")]
use tokio_rustls::TlsAcceptor;

/// Called when a browser sends an SDP offer. Return the SDP answer or an error string.
pub type OfferFn = dyn Fn(&str, &str) -> std::result::Result<String, String> + Send + Sync;

/// Called when a browser sends an ICE candidate.
pub type IceCandidateFn = dyn Fn(&str, &str) -> std::result::Result<(), String> + Send + Sync;

/// Called when a browser closes a session.
pub type CloseFn = dyn Fn(&str) + Send + Sync;

/// Shared state for the signaling server.
pub struct SignalingServer {
    on_offer: Arc<OfferFn>,
    on_ice: Arc<IceCandidateFn>,
    on_close: Arc<CloseFn>,
}

impl SignalingServer {
    /// Create a new signaling server with the given callbacks.
    ///
    /// All callbacks are called synchronously from the HTTP handler, so they
    /// must not block. They should do minimal work: create a peer, add a
    /// candidate, or remove a session from a map.
    pub fn new(
        on_offer: Arc<OfferFn>,
        on_ice: Arc<IceCandidateFn>,
        on_close: Arc<CloseFn>,
    ) -> Self {
        Self {
            on_offer,
            on_ice,
            on_close,
        }
    }

    /// Start the plain HTTP signaling server, binding to `addr`.
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

        // No GET endpoints — ICE-Lite doesn't need SSE.
        if method == "GET" {
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

        let response = self.route(method, path, &body);

        writer.write_all(response.as_bytes()).await?;
        writer.shutdown().await?;

        Ok(())
    }

    fn route(&self, method: &str, path: &str, body: &[u8]) -> String {
        match (method, path) {
            ("POST", "/webrtc/offer") => self.handle_offer(body),
            ("POST", p) if p.starts_with("/webrtc/ice/") => {
                let session_id = p.strip_prefix("/webrtc/ice/").unwrap_or("");
                self.handle_ice_candidate(session_id, body)
            }
            ("POST", p) if p.starts_with("/webrtc/close/") => {
                let session_id = p.strip_prefix("/webrtc/close/").unwrap_or("");
                self.handle_close(session_id)
            }
            _ => http_response(404, "Not Found", "text/plain"),
        }
    }

    fn handle_offer(&self, body: &[u8]) -> String {
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

        let session_id = nanoid();

        // Create the peer and get the SDP answer synchronously.
        match (self.on_offer)(&session_id, &sdp) {
            Ok(answer_sdp) => {
                tracing::info!(
                    %session_id,
                    offer_len = sdp.len(),
                    answer_len = answer_sdp.len(),
                    "WebRTC offer accepted, returning answer in response"
                );
                let response_body = serde_json::json!({
                    "session_id": session_id,
                    "sdp": answer_sdp,
                });
                http_response(200, &response_body.to_string(), "application/json")
            }
            Err(error) => {
                tracing::warn!(%session_id, %error, "WebRTC offer rejected");
                let response_body = serde_json::json!({
                    "session_id": session_id,
                    "error": error,
                });
                http_response(400, &response_body.to_string(), "application/json")
            }
        }
    }

    fn handle_ice_candidate(&self, session_id: &str, body: &[u8]) -> String {
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

        match (self.on_ice)(session_id, &candidate_str) {
            Ok(()) => http_response(200, r#"{"status":"ok"}"#, "application/json"),
            Err(e) => {
                tracing::warn!(%session_id, error = %e, "ICE candidate rejected");
                http_response(400, &format!(r#"{{"error":"{}"}}"#, e), "application/json")
            }
        }
    }

    fn handle_close(&self, session_id: &str) -> String {
        (self.on_close)(session_id);
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
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}
