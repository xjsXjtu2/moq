/** Configuration for the HTTP-based WebRTC signaling client. */
export interface SignalingClientConfig {
    /** Base URL of the signaling server (e.g. http://localhost:8080/webrtc). */
    baseUrl: string;
}

/** Response from POST /webrtc/offer (ICE-Lite mode). */
interface OfferResponse {
    session_id: string;
    sdp: string; // SDP answer, returned directly in the response
    error?: string;
}

/**
 * HTTP signaling client for WebRTC SDP/ICE exchange (ICE-Lite).
 *
 * In ICE-Lite mode, the server has a known public address. Signaling is a
 * single round-trip: the browser POSTs its SDP offer, and the server returns
 * the SDP answer (with the host candidate baked in) directly in the response.
 *
 * Endpoints:
 * - POST /webrtc/offer     — send offer, receive answer synchronously
 * - POST /webrtc/ice/:id   — send local ICE candidate (browser trickle)
 * - POST /webrtc/close/:id — close the session
 */
export class SignalingClient {
    readonly #baseUrl: string;
    #sessionId?: string;

    constructor(config: SignalingClientConfig) {
        this.#baseUrl = config.baseUrl.replace(/\/$/, "");
    }

    /** The session ID assigned by the server after a successful offer. */
    get sessionId(): string | undefined {
        return this.#sessionId;
    }

    /**
     * Send the local SDP offer to the server and receive the SDP answer
     * directly in the response (ICE-Lite: no SSE needed).
     */
    async sendOffer(sdp: string): Promise<{ sessionId: string; answerSdp: string }> {
        const response = await fetch(`${this.#baseUrl}/offer`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ sdp }),
        });

        if (!response.ok) {
            throw new Error(`Signaling offer failed: ${response.status} ${response.statusText}`);
        }

        const data = (await response.json()) as OfferResponse;
        if (data.error) {
            throw new Error(`Signaling offer rejected: ${data.error}`);
        }

        this.#sessionId = data.session_id;
        console.log("WebRTC signaling: offer accepted, session_id=", data.session_id);
        return { sessionId: data.session_id, answerSdp: data.sdp };
    }

    /**
     * Send a local (browser) ICE candidate to the server.
     */
    async sendIceCandidate(candidate: string): Promise<void> {
        if (!this.#sessionId) throw new Error("No session ID");

        const response = await fetch(`${this.#baseUrl}/ice/${this.#sessionId}`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ candidate }),
        });

        if (!response.ok) {
            console.warn(`ICE candidate send failed: ${response.status}`);
        }
    }

    /**
     * Close the signaling session.
     */
    async close(): Promise<void> {
        if (this.#sessionId) {
            try {
                await fetch(`${this.#baseUrl}/close/${this.#sessionId}`, { method: "POST" });
            } catch {
                // Best-effort close.
            }
            this.#sessionId = undefined;
        }
    }
}
