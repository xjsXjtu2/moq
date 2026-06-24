/**
 * Minimal renderer that attaches a WebRTC MediaStream to a `<video>` element.
 *
 * Compared to the MoQ pipeline (WebCodecs → Canvas → drawImage), this approach
 * is simpler: the browser decodes H.264/Opus natively via the `<video>` element.
 * WebRTC's built-in jitter buffer and bandwidth estimation also apply automatically.
 */
export class WebrtcRenderer {
    readonly #video: HTMLVideoElement;
    #stream?: MediaStream;
    #muted: boolean;

    constructor(video: HTMLVideoElement) {
        this.#video = video;
        this.#muted = video.muted;

        // Required for autoplay with audio (browser policy).
        video.muted = true;
        video.autoplay = true;
        video.playsInline = true;
    }

    /**
     * Set the MediaStream received from the remote peer.
     * Call this from the `ontrack` callback.
     */
    setStream(stream: MediaStream): void {
        this.#stream = stream;
        this.#video.srcObject = stream;
        this.#video.muted = this.#muted;
    }

    /** Add a track from an `ontrack` event to the stream. */
    addTrack(track: MediaStreamTrack): void {
        if (!this.#stream) {
            this.#stream = new MediaStream();
            this.#video.srcObject = this.#stream;
        }
        this.#stream.addTrack(track);
    }

    /** Unmute audio (must be called from a user gesture in most browsers). */
    unmute(): void {
        this.#muted = false;
        this.#video.muted = false;
    }

    /** Mute audio. */
    mute(): void {
        this.#muted = true;
        this.#video.muted = true;
    }

    /** Tear down. */
    close(): void {
        this.#stream?.getTracks().forEach((t) => t.stop());
        this.#video.srcObject = null;
    }
}
