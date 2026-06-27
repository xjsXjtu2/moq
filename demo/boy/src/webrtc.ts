/**
 * WebRTC Demo for MoQ Boy.
 *
 * Connects directly to a moq-boy instance running in WebRTC mode
 * (e.g. `moq-boy --webrtc-listen 0.0.0.0:8080 --rom pokemon.gb`).
 *
 * Uses browser-native WebRTC APIs — no MoQ protocol libraries required.
 */
import { WebrtcPeer } from "@moq/webrtc-boy";
import { InputHandler, KEY_MAP } from "@moq/webrtc-boy/input";

// --- Configuration ---

const params = new URLSearchParams(location.search);
const urlParam = params.get("url");

function resolveBaseUrl(raw: string): string {
    const target = new URL(raw);
    if (
        target.hostname === "localhost" ||
        target.hostname === "127.0.0.1" ||
        target.hostname === location.hostname
    ) {
        return target.protocol === "https:" ? "/webrtc-proxy-tls" : "/webrtc-proxy";
    }
    return `${target.origin}/webrtc`;
}

let baseUrl: string;
if (urlParam) {
    baseUrl = resolveBaseUrl(urlParam);
} else {
    baseUrl = "/webrtc-proxy";
}

const stunServer = params.get("stun") ?? undefined;

const video = document.getElementById("video") as HTMLVideoElement;
const statusEl = document.getElementById("status")!;
const errorEl = document.getElementById("error")!;
const controlsEl = document.getElementById("controls")!;
const actionRow = document.getElementById("action-row")!;
const statsPanel = document.getElementById("stats")!;

// --- Stats DOM refs ---
const sResolution = document.getElementById("s-resolution")!;
const sCodec = document.getElementById("s-codec")!;
const sFps = document.getElementById("s-fps")!;
const sPlost = document.getElementById("s-plost")!;
const sNack = document.getElementById("s-nack")!;
const sJitter = document.getElementById("s-jitter")!;
const sDecode = document.getElementById("s-decode")!;
const sEncode = document.getElementById("s-encode")!;
const sRtt = document.getElementById("s-rtt")!;
const sCapren = document.getElementById("s-capren")!;
const sTotal = document.getElementById("s-total")!;

// Running values for total latency computation.
let latencyJitter = 0;
let latencyDecode = 0;
let latencyEncode = 0;
let latencyCapren = 0;
let latencyRtt = 0;
const latencyOthers = 10; // fixed: OS/encoder pipeline/etc overhead

// --- WebRTC Connection ---

const peer = new WebrtcPeer({
    signaling: { baseUrl },
    stunServer,
});

peer.onTrack = (stream) => {
    video.srcObject = stream;
    statusEl.textContent = "Playing (WebRTC)";
    // Muted autoplay: Chrome requires muted for autoplay without user gesture.
    video.muted = true;
    video.play().catch((e) => console.warn("video.play() failed:", e));
    // First click/tap on the video unmutes (browser policy: audio needs user gesture).
    const unmuteOnFirstClick = () => {
        if (video.muted) {
            video.muted = false;
            const unmuteBtn = document.getElementById("btn-unmute");
            if (unmuteBtn) unmuteBtn.textContent = "🔊 Mute";
        }
    };
    video.addEventListener("click", unmuteOnFirstClick, { once: true });
    // Also unmute on first keydown (keyboard players).
    document.addEventListener("keydown", unmuteOnFirstClick, { once: true });
};

peer.onStateChange = (state) => {
    console.log("WebRTC state:", state);
    statusEl.textContent = `WebRTC: ${state}`;

    if (state === "connected") {
        controlsEl.style.display = "";
        actionRow.style.display = "";
        statsPanel.style.display = "";
        stopStats = startStatsPolling();
    } else if (state === "disconnected" || state === "failed") {
        statusEl.textContent = `WebRTC: ${state} — reconnecting...`;
        stopStats?.();
    }
};

// Server-pushed status (encode time) arrives via DataChannel onmessage.
peer.onStatus = (status) => {
    if (typeof status.encode_ms === "number") {
        latencyEncode = status.encode_ms;
        sEncode.textContent = `${latencyEncode}ms`;
        colorizeLatency(sEncode, latencyEncode, 5, 15);
    }
    if (typeof status.video_fps === "number") {
        sFps.textContent = `${status.video_fps.toFixed(0)} fps`;
    }
};

// --- Browser-side timestamp overlay ---

const tsOverlay = document.getElementById("ts-overlay")!;
function updateTsOverlay() {
    tsOverlay.textContent = Math.round(performance.now()).toString();
    requestAnimationFrame(updateTsOverlay);
}
requestAnimationFrame(updateTsOverlay);

// --- Input Handling ---

const input = new InputHandler((cmd) => {
    peer.sendCommand(cmd);
}, /* showTsWatermark */ true);

document.addEventListener("keydown", (e) => {
    if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement) return;
    input.keydown(e);
});
document.addEventListener("keyup", (e) => {
    input.keyup(e);
});
window.addEventListener("blur", () => {
    input.clear();
});

controlsEl.querySelectorAll("button[data-btn]").forEach((btn) => {
    const buttonName = (btn as HTMLButtonElement).dataset.btn!;
    const down = () => {
        input.heldButtons.add(buttonName as never);
        peer.sendCommand({ type: "buttons", buttons: [...input.heldButtons] });
    };
    const up = () => {
        input.heldButtons.delete(buttonName as never);
        peer.sendCommand({ type: "buttons", buttons: [...input.heldButtons] });
    };

    btn.addEventListener("mousedown", down);
    btn.addEventListener("mouseup", up);
    btn.addEventListener("mouseleave", up);
    btn.addEventListener("touchstart", (e) => { e.preventDefault(); down(); });
    btn.addEventListener("touchend", (e) => { e.preventDefault(); up(); });
});

document.getElementById("btn-reset")?.addEventListener("click", () => {
    input.reset();
});

document.getElementById("btn-unmute")?.addEventListener("click", function () {
    video.muted = !video.muted;
    (this as HTMLButtonElement).textContent = video.muted ? "🔇 Unmute" : "🔊 Mute";
});

// --- Stats Polling ---

let stopStats: (() => void) | undefined;

function startStatsPolling(): () => void {
    let timer = setInterval(pollStats, 1000);
    return () => clearInterval(timer);
}

async function pollStats() {
    const pc = peer.getPeerConnection();
    if (!pc) return;
    try {
        const report = await pc.getStats();
        let videoInbound: any = null;
        let remoteVideo: any = null;
        let candidatePair: any = null;

        for (const [, stat] of report) {
            if (stat.type === "inbound-rtp" && stat.kind === "video") {
                videoInbound = stat;
            }
            if (stat.type === "remote-outbound-rtp" && stat.kind === "video") {
                remoteVideo = stat;
            }
            if (stat.type === "candidate-pair" && stat.state === "succeeded") {
                candidatePair = stat;
            }
        }

        if (videoInbound) {
            // Resolution
            if (videoInbound.frameWidth && videoInbound.frameHeight) {
                sResolution.textContent = `${videoInbound.frameWidth}x${videoInbound.frameHeight}`;
            }

            // Codec
            if (videoInbound.codecId) {
                const codec = report.get(videoInbound.codecId);
                if (codec && codec.mimeType) {
                    sCodec.textContent = codec.mimeType.replace("video/", "");
                }
            }

            // FPS
            if (typeof videoInbound.framesPerSecond === "number") {
                sFps.textContent = `${videoInbound.framesPerSecond.toFixed(0)} fps`;
            }

            // Packets lost
            const lost = videoInbound.packetsLost ?? 0;
            const received = videoInbound.packetsReceived ?? 1;
            const lossPct = ((lost / (received + lost)) * 100).toFixed(1);
            sPlost.textContent = `${lost} (${lossPct}%)`;
            colorizeLatency(sPlost, Number(lossPct), 1, 5);

            // NACK / PLI
            const nack = videoInbound.nackCount ?? 0;
            const pli = videoInbound.pliCount ?? 0;
            sNack.textContent = `${nack} / ${pli}`;

            // Jitter buffer delay (ms)
            if (typeof videoInbound.jitterBufferDelay === "number" &&
                typeof videoInbound.jitterBufferEmittedCount === "number" &&
                videoInbound.jitterBufferEmittedCount > 0) {
                latencyJitter = (videoInbound.jitterBufferDelay / videoInbound.jitterBufferEmittedCount) * 1000;
                sJitter.textContent = `${latencyJitter.toFixed(1)}ms`;
                colorizeLatency(sJitter, latencyJitter, 30, 80);
            }

            // Decode time (ms)
            if (typeof videoInbound.totalDecodeTime === "number" &&
                typeof videoInbound.framesDecoded === "number" &&
                videoInbound.framesDecoded > 0) {
                latencyDecode = (videoInbound.totalDecodeTime / videoInbound.framesDecoded) * 1000;
                sDecode.textContent = `${latencyDecode.toFixed(1)}ms`;
                colorizeLatency(sDecode, latencyDecode, 5, 15);
            }

            // cap_ren(est): capture + render estimate.
            // Both are periodic triggers. Each has an expected wait of 1/(2*fps),
            // so combined = 1/fps = 1000/fps milliseconds.
            if (typeof videoInbound.framesPerSecond === "number" && videoInbound.framesPerSecond > 0) {
                latencyCapren = 1000 / videoInbound.framesPerSecond;
                sCapren.textContent = `${latencyCapren.toFixed(1)}ms`;
                colorizeLatency(sCapren, latencyCapren, 17, 34);
            }
        }

        // RTT from the candidate pair
        if (candidatePair && typeof candidatePair.currentRoundTripTime === "number") {
            latencyRtt = candidatePair.currentRoundTripTime * 1000;
            sRtt.textContent = `${latencyRtt.toFixed(1)}ms`;
            colorizeLatency(sRtt, latencyRtt, 50, 150);
        }

        // Total latency estimate
        const total = latencyJitter + latencyDecode + latencyEncode + latencyCapren + latencyRtt + latencyOthers;
        sTotal.textContent = total > 0 ? `${total.toFixed(0)}ms` : "--";
        colorizeLatency(sTotal, total, 80, 200);
    } catch (err) {
        // getStats() can throw during connection teardown — ignore.
    }
}

function colorizeLatency(el: HTMLElement, ms: number, warn = 30, bad = 80) {
    el.className = `value ${ms < warn ? "good" : ms < bad ? "warn" : "bad"}`;
}

// --- Start ---

async function start() {
    try {
        statusEl.textContent = "Connecting via WebRTC...";
        await peer.connect();
    } catch (err) {
        console.error("WebRTC connection error:", err);
        errorEl.hidden = false;
        errorEl.textContent = `Connection failed: ${err instanceof Error ? err.message : String(err)}`;
        statusEl.textContent = "Connection failed";
    }
}

start();
