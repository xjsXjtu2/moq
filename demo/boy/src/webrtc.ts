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

// Priority: ?url= query param > default localhost
const params = new URLSearchParams(location.search);
const baseUrl = params.get("url") ?? "http://localhost:8080";

const stunServer = params.get("stun") ?? undefined;

const video = document.getElementById("video") as HTMLVideoElement;
const statusEl = document.getElementById("status")!;
const errorEl = document.getElementById("error")!;
const controlsEl = document.getElementById("controls")!;
const actionRow = document.getElementById("action-row")!;

// --- WebRTC Connection ---

const peer = new WebrtcPeer({
    signaling: { baseUrl: `${baseUrl.replace(/\/$/, "")}/webrtc` },
    stunServer,
});

peer.onTrack = (stream) => {
    video.srcObject = stream;
    statusEl.textContent = "Playing (WebRTC)";
};

peer.onStateChange = (state) => {
    console.log("WebRTC state:", state);
    statusEl.textContent = `WebRTC: ${state}`;

    if (state === "connected") {
        controlsEl.style.display = "";
        actionRow.style.display = "";
        // Unmute video once connected (browser autoplay policy satisfied).
        video.muted = false;
    } else if (state === "disconnected" || state === "failed") {
        statusEl.textContent = `WebRTC: ${state} — reconnecting...`;
    }
};

peer.onStatus = (status) => {
    console.debug("Status:", status);
};

// --- Input Handling ---

const input = new InputHandler((cmd) => {
    peer.sendCommand(cmd);
});

// Keyboard events.
document.addEventListener("keydown", (e) => {
    // Don't capture when typing in an input field.
    if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement) return;
    input.keydown(e);
});
document.addEventListener("keyup", (e) => {
    input.keyup(e);
});
window.addEventListener("blur", () => {
    input.clear();
});

// On-screen button events.
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

// Reset button.
document.getElementById("btn-reset")?.addEventListener("click", () => {
    input.reset();
});

// Unmute button.
document.getElementById("btn-unmute")?.addEventListener("click", function () {
    video.muted = !video.muted;
    (this as HTMLButtonElement).textContent = video.muted ? "🔇 Unmute" : "🔊 Mute";
});

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
