/** Configuration for the HTTP-based WebRTC signaling client. */
export interface SignalingClientConfig {
    /** Base URL of the signaling server (e.g. http://localhost:8080/webrtc). */
    baseUrl: string;
}

/** Response from POST /webrtc/offer. */
interface OfferResponse {
    session_id: string;
    status: string;
}

/**
 * HTTP signaling client for WebRTC SDP/ICE exchange.
 *
 * Communicates with the moq-boy WebRTC signaling server via REST endpoints:
 * - POST /webrtc/offer     — send client SDP offer, receive session ID
 * - POST /webrtc/ice/:id   — send local ICE candidate
 * - GET  /webrtc/ice/:id   — receive server ICE candidates via SSE
 * - POST /webrtc/close/:id — close the session
 */
export class SignalingClient {
    readonly #baseUrl: string;
    #sessionId?: string;
    #eventSource?: EventSource;

    constructor(config: SignalingClientConfig) {
        this.#baseUrl = config.baseUrl.replace(/\/$/, "");
    }

    /** The session ID assigned by the server after a successful offer. */
    get sessionId(): string | undefined {
        return this.#sessionId;
    }

    /**
     * Send the local SDP offer to the server.
     *
     * Returns when the server acknowledges the offer. The actual SDP answer
     * is delivered asynchronously via the SSE channel — call `subscribeAnswer`
     * before calling this to receive the answer.
     */
    async sendOffer(sdp: string): Promise<string> {
        const response = await fetch(`${this.#baseUrl}/offer`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ sdp }),
        });

        if (!response.ok) {
            throw new Error(`Signaling offer failed: ${response.status} ${response.statusText}`);
        }

        const data = (await response.json()) as OfferResponse;
        this.#sessionId = data.session_id;
        console.log("WebRTC signaling: offer accepted, session_id=", data.session_id);
        return data.session_id;
    }

    /**
     * Subscribe to server-pushed SDP answer and ICE candidates via SSE.
     *
     * Call this BEFORE `sendOffer()` to ensure no events are missed.
     *
     * @param onAnswer  Called when the server sends the SDP answer.
     * @param onIce     Called for each server ICE candidate.
     */
    subscribe(onAnswer: (sdp: string) => void, onIce: (candidate: string) => void): void {
        if (!this.#sessionId) {
            throw new Error("Cannot subscribe before session is created; call sendOffer() first");
        }

        const url = `${this.#baseUrl}/ice/${this.#sessionId}`;
        console.log("WebRTC signaling: opening SSE", url);
        this.#eventSource = new EventSource(url);

        this.#eventSource.onopen = () => {
            console.log("WebRTC signaling: SSE connection opened");
        };

        this.#eventSource.addEventListener("candidate", (event: MessageEvent) => {
            console.log("WebRTC signaling: SSE candidate event, data length=", event.data.length);
            const data = JSON.parse(event.data);
            if (data.candidate) {
                // Check for prefixed answer delivery (the server sends the
                // SDP answer as a candidate with an "ANSWER:" prefix).
                if (data.candidate.startsWith("ANSWER:")) {
                    const sdp = data.candidate.slice("ANSWER:".length);
                    console.log("WebRTC signaling: received answer via SSE, sdp length=", sdp.length);
                    onAnswer(sdp);
                } else {
                    console.log("WebRTC signaling: received ICE candidate via SSE");
                    onIce(data.candidate);
                }
            }
        });

        this.#eventSource.onerror = () => {
            console.warn("WebRTC signaling: SSE connection error, readyState=", this.#eventSource?.readyState);
        };
    }

    /**
     * Send a local ICE candidate to the server.
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
        this.#eventSource?.close();
        this.#eventSource = undefined;

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
