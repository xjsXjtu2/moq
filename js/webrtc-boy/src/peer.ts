import { SignalingClient } from "./signaling.ts";
import type { SignalingClientConfig } from "./signaling.ts";
import type { InputCommand } from "./input.ts";

/** Configuration for creating a WebRTC peer connection to moq-boy. */
export interface WebrtcPeerConfig {
    /** Signaling server config. */
    signaling: SignalingClientConfig;
    /** STUN server URL for ICE candidate gathering (optional). */
    stunServer?: string;
}

/**
 * Manages a WebRTC peer connection to the moq-boy server.
 *
 * Establishes a client-offer WebRTC connection:
 * 1. Creates an RTCPeerConnection with H.264 video and Opus audio codec preferences.
 * 2. Creates a DataChannel for sending button commands and receiving status.
 * 3. Generates an SDP offer and sends it to the signaling server.
 * 4. Receives the SDP answer and ICE candidates via SSE.
 * 5. Once connected, delivers remote media tracks via callbacks and accepts
 *    outgoing commands via the DataChannel.
 *
 * Usage:
 * ```typescript
 * const peer = new WebrtcPeer({
 *     signaling: { baseUrl: "http://localhost:8080/webrtc" },
 * });
 *
 * peer.onTrack = (stream) => {
 *     videoElement.srcObject = stream;
 * };
 *
 * await peer.connect();
 * peer.sendCommand({ type: "buttons", buttons: ["a"] });
 * ```
 */
export class WebrtcPeer {
    readonly #signaling: SignalingClient;
    readonly #stunServer?: string;

    #pc?: RTCPeerConnection;
    #dc?: RTCDataChannel;
    #connected = false;

    /** Callback invoked when remote media tracks arrive. */
    onTrack?: (stream: MediaStream) => void;

    /** Callback invoked when the ICE connection state changes. */
    onStateChange?: (state: RTCPeerConnectionState) => void;

    /** Callback invoked when a status message arrives on the DataChannel. */
    onStatus?: (status: Record<string, unknown>) => void;

    constructor(config: WebrtcPeerConfig) {
        this.#signaling = new SignalingClient(config.signaling);
        this.#stunServer = config.stunServer;
    }

    /** Whether the ICE connection is established. */
    get connected(): boolean {
        return this.#connected;
    }

    /**
     * Initialize the peer connection and begin the SDP offer/answer exchange.
     *
     * Returns a promise that resolves once the connection is established (ICE connected).
     * If the connection fails, the promise rejects with an error.
     */
    async connect(): Promise<void> {
        const config: RTCConfiguration = {};

        if (this.#stunServer) {
            config.iceServers = [{ urls: this.#stunServer }];
        }

        this.#pc = new RTCPeerConnection(config);

        // Set up connection state tracking.
        this.#pc.onconnectionstatechange = () => {
            const state = this.#pc!.connectionState;
            console.log("WebRTC connection state:", state);
            this.onStateChange?.(state);

            if (state === "connected") {
                this.#connected = true;
            } else if (state === "disconnected" || state === "failed" || state === "closed") {
                this.#connected = false;
            }
        };

        // Create the DataChannel for sending button commands.
        this.#dc = this.#pc.createDataChannel("control", {
            ordered: true,
            // Negotiate delivery semantics: reliable+ordered for control messages.
        });

        this.#dc.onopen = () => {
            console.log("WebRTC DataChannel: open");
        };

        this.#dc.onclose = () => {
            console.log("WebRTC DataChannel: closed");
        };

        this.#dc.onmessage = (event: MessageEvent) => {
            try {
                const msg = JSON.parse(event.data);
                this.onStatus?.(msg);
            } catch {
                console.warn("WebRTC DataChannel: invalid JSON message");
            }
        };

        // Handle incoming media tracks.
        this.#pc.ontrack = (event: RTCTrackEvent) => {
            console.log("WebRTC ontrack:", event.track.kind, event.track.id);
            // Build a MediaStream from the received tracks.
            const stream = event.streams[0] ?? new MediaStream();
            if (!event.streams[0]) {
                stream.addTrack(event.track);
            }
            this.onTrack?.(stream);
        };

        // Configure transceivers for receiving video and audio.
        // This tells the browser to include H.264 and Opus in the SDP offer.
        this.#pc.addTransceiver("video", {
            direction: "recvonly",
        });
        this.#pc.addTransceiver("audio", {
            direction: "recvonly",
        });

        // Subscribe to server answer and ICE candidates BEFORE sending offer
        // to avoid missing the answer.
        const answerPromise = new Promise<RTCSessionDescriptionInit>(
            (resolve) => {
                this.#signaling.subscribe(
                    (sdp) => resolve({ type: "answer", sdp }),
                    (candidate) => {
                        this.#pc?.addIceCandidate(new RTCIceCandidate(JSON.parse(candidate)))
                            .catch((e) => console.warn("ICE candidate add failed:", e));
                    },
                );
            },
        );

        // Create and send the SDP offer.
        const offer = await this.#pc.createOffer();
        await this.#pc.setLocalDescription(offer);

        const sessionId = await this.#signaling.sendOffer(offer.sdp!);
        console.log("WebRTC: offer sent, session_id=", sessionId);

        // Wait for the server answer.
        const answer = await answerPromise;
        await this.#pc.setRemoteDescription(new RTCSessionDescription(answer));
        console.log("WebRTC: remote description set");

        // Send local ICE candidates as they are discovered.
        this.#pc.onicecandidate = (event: RTCPeerConnectionIceEvent) => {
            if (event.candidate) {
                this.#signaling.sendIceCandidate(JSON.stringify(event.candidate.toJSON()));
            }
        };

        // Wait for ICE connection to complete.
        await this.#waitForConnection();
    }

    /**
     * Send a button command to the server via the DataChannel.
     */
    sendCommand(cmd: InputCommand): void {
        if (!this.#dc || this.#dc.readyState !== "open") {
            console.warn("WebRTC DataChannel: not ready, dropping command");
            return;
        }
        this.#dc.send(JSON.stringify(cmd));
    }

    /**
     * Close the peer connection and signaling session.
     */
    close(): void {
        this.#dc?.close();
        this.#pc?.close();
        this.#signaling.close();
        this.#connected = false;
    }

    async #waitForConnection(): Promise<void> {
        return new Promise((resolve, reject) => {
            const timeout = setTimeout(() => {
                reject(new Error("WebRTC connection timed out (30s)"));
            }, 30_000);

            const pc = this.#pc!;
            const check = () => {
                const state = pc.connectionState;
                if (state === "connected" || state === "completed") {
                    clearTimeout(timeout);
                    resolve();
                } else if (state === "failed") {
                    clearTimeout(timeout);
                    reject(new Error("WebRTC connection failed"));
                }
                // "connecting" and "new" states: keep waiting.
            };

            pc.onconnectionstatechange = () => {
                check();
                this.onStateChange?.(pc.connectionState);
            };
            check(); // In case it's already connected.
        });
    }
}
