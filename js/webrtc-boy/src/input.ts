/** A command sent to the server via WebRTC DataChannel. */
export interface InputCommand {
    type: "buttons" | "reset";
    buttons?: string[];
    client_ts?: number;
}

/**
 * Key-to-button mapping for Game Boy controls.
 *
 * Matches the mapping used by `@moq/boy` so keyboard handling is identical
 * across MoQ and WebRTC transport modes.
 */
export const KEY_MAP: Record<string, string> = {
    ArrowUp: "up",
    ArrowDown: "down",
    ArrowLeft: "left",
    ArrowRight: "right",
    z: "b",
    Z: "b",
    x: "a",
    X: "a",
    Enter: "start",
    Shift: "select",
};

/** All possible Game Boy button names. */
export type Button = "up" | "down" | "left" | "right" | "a" | "b" | "start" | "select";

/**
 * Input handler that tracks held buttons and sends commands to a WebRTC peer.
 *
 * Usage:
 * ```typescript
 * const input = new InputHandler(sendCommand);
 * document.addEventListener("keydown", (e) => input.keydown(e));
 * document.addEventListener("keyup", (e) => input.keyup(e));
 * window.addEventListener("blur", () => input.clear());
 * ```
 */
export class InputHandler {
    readonly heldButtons = new Set<Button>();

    readonly #send: (cmd: InputCommand) => void;
    readonly #showTs: boolean;

    constructor(send: (cmd: InputCommand) => void, showTsWatermark = false) {
        this.#send = send;
        this.#showTs = showTsWatermark;
    }

    /** Handle a keydown event. Returns true if the key was handled. */
    keydown(e: KeyboardEvent): boolean {
        const button = KEY_MAP[e.key];
        if (!button || e.repeat) return false;

        e.preventDefault();
        this.heldButtons.add(button as Button);
        this.#sendCommand({ type: "buttons", buttons: [...this.heldButtons] });
        return true;
    }

    /** Handle a keyup event. Returns true if the key was handled. */
    keyup(e: KeyboardEvent): boolean {
        const button = KEY_MAP[e.key];
        if (!button) return false;

        this.heldButtons.delete(button as Button);
        this.#sendCommand({ type: "buttons", buttons: [...this.heldButtons] });
        return true;
    }

    /** Send a reset command. */
    reset(): void {
        this.heldButtons.clear();
        this.#sendCommand({ type: "reset" });
    }

    /** Clear all held buttons (call on window blur). */
    clear(): void {
        if (this.heldButtons.size === 0) return;
        this.heldButtons.clear();
        this.#sendCommand({ type: "buttons", buttons: [] });
    }

    #sendCommand(cmd: InputCommand): void {
        if (this.#showTs) {
            cmd.client_ts = Math.round(performance.now());
        }
        this.#send(cmd);
    }
}
