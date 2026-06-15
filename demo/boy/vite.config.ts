import { resolve } from "path";
import { defineConfig } from "vite";
import basicSsl from "@vitejs/plugin-basic-ssl";
import solidPlugin from "vite-plugin-solid";
import { workletInline } from "../../js/common/vite-plugin-worklet";

export default defineConfig({
	root: "src",
	envDir: resolve(__dirname),
	plugins: [solidPlugin(), workletInline(), basicSsl()],
	build: {
		target: "esnext",
		rollupOptions: {
			input: {
				main: resolve(__dirname, "src/index.html"),
			},
		},
	},
	server: {
		hmr: false,
		headers: {
			// Required for SharedArrayBuffer (used by audio worklets for low-latency audio).
			// SharedArrayBuffer is gated behind cross-origin isolation to mitigate Spectre attacks.
			"Cross-Origin-Opener-Policy": "same-origin",
			"Cross-Origin-Embedder-Policy": "require-corp",
		},
		proxy: {
			// Proxy certificate fingerprint requests to moq-boy's HTTP server.
			// Avoids mixed-content blocking when the page is served over HTTPS.
			"/cert-proxy": {
				target: "http://localhost:4443",
				changeOrigin: true,
				rewrite: (path) => path.replace(/^\/cert-proxy/, ""),
			},
		},
	},
	optimizeDeps: {
		exclude: ["@libav.js/variant-opus-af"],
	},
});
