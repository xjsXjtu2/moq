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
 * Manages a WebRTC peer connection to the moq-boy server (ICE-Lite).
 *
 * In ICE-Lite mode the server has a known public address, so signaling
 * is a single round-trip: the SDP answer (with the host candidate) is
 * returned directly in the POST /webrtc/offer response. No SSE needed.
 *
 * The browser still uses trickle ICE for its own candidates. They are
 * sent to the server via POST /webrtc/ice/:id as they are discovered.
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

    /** Expose the underlying RTCPeerConnection for stats polling. */
    getPeerConnection(): RTCPeerConnection | undefined {
        return this.#pc;
    }

    /**
     * Initialize the peer connection and begin the SDP offer/answer exchange.
     *
     * 1. Creates RTCPeerConnection with recvonly video + audio transceivers
     * 2. Creates a DataChannel for button commands
     * 3. Generates SDP offer and sends it via POST /webrtc/offer
     * 4. Receives SDP answer directly in the response (ICE-Lite)
     * 5. Sends local ICE candidates via POST /webrtc/ice/:id
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
        this.#dc = this.#pc.createDataChannel("control", { ordered: true });

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

        // Handle incoming media tracks. str0m may deliver video and audio on
        // separate streams via `event.streams[0]`. To guarantee both play
        // through the same <video> element, we build our own MediaStream
        // that collects every track regardless of which stream it arrives on.
        const remoteStream = new MediaStream();

        this.#pc.ontrack = (event: RTCTrackEvent) => {
            console.log("WebRTC ontrack:", event.track.kind, event.track.id);
            remoteStream.addTrack(event.track);
            this.onTrack?.(remoteStream);
        };

        // Configure transceivers for receiving video and audio.
        this.#pc.addTransceiver("video", { direction: "recvonly" });
        this.#pc.addTransceiver("audio", { direction: "recvonly" });

        // Prefer H.264 over VP8/VP9 for the video transceiver.
        // The server encodes H.264; if the browser negotiates VP8 instead,
        // the VP8 decoder will reject H.264 NAL units and produce no frames.
        {
            const videoTcvr = this.#pc.getTransceivers().find((t) => t.receiver.track.kind === "video");
            if (videoTcvr) {
                const caps = RTCRtpReceiver.getCapabilities("video");
                if (caps) {
                    const h264Codecs = caps.codecs.filter(
                        (c) => c.mimeType.toLowerCase().includes("h264"),
                    );
                    if (h264Codecs.length > 0) {
                        videoTcvr.setCodecPreferences(h264Codecs);
                        console.log("WebRTC: H.264 codec preference set, count=", h264Codecs.length);
                    } else {
                        console.warn("WebRTC: no H.264 codec available in browser; video may not decode");
                    }
                }
            }
        }

        // Create and send the SDP offer.
        const offer = await this.#pc.createOffer();
        await this.#pc.setLocalDescription(offer);

        // POST offer, receive answer synchronously (ICE-Lite).
        const { sessionId, answerSdp } = await this.#signaling.sendOffer(offer.sdp!);
        console.log("WebRTC: offer sent, session_id=", sessionId);

        // Set the remote description from the answer.
        await this.#pc.setRemoteDescription(
            new RTCSessionDescription({ type: "answer", sdp: answerSdp }),
        );
        console.log("WebRTC: remote description set (answer from POST response)");

        // Send local (browser) ICE candidates to the server as they are discovered.
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
            };

            pc.onconnectionstatechange = () => {
                check();
                this.onStateChange?.(pc.connectionState);
            };
            check();
        });
    }
}
