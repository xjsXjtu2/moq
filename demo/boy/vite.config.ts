import { resolve } from "path";
import { defineConfig } from "vite";
import basicSsl from "@vitejs/plugin-basic-ssl";
import solidPlugin from "vite-plugin-solid";
import { workletInline } from "../../js/common/vite-plugin-worklet";

// vite-plugin-solid uses import.meta.hot, which forces @vite/client injection
// even when server.hmr is false. This plugin neutralizes it at two levels:
// 1. Strips the <script> tag from HTML (best-effort)
// 2. Intercepts the /@vite/client request and returns a no-op module
//    (catches it at the network layer if the HTML strip doesn't fire)
function removeViteClient() {
	return {
		name: "remove-vite-client",
		enforce: "post" as const,
		configureServer(server: any) {
			server.middlewares.use(
				(req: any, res: any, next: () => void) => {
					if (req.url?.startsWith("/@vite/client")) {
						res.setHeader("Content-Type", "application/javascript");
						res.end(`
// Stub: HMR is disabled, all exports are no-ops.
const noop = () => {};
const hotCtx = { accept: noop, dispose: noop, invalidate: noop, on: noop, off: noop, send: noop, prune: noop, data: {} };
export function createHotContext() { return hotCtx; }
export default {};
`);
						return;
					}
					next();
				},
			);
		},
		transformIndexHtml(html: string) {
			return html.replace(
				/<script[^>]*src=["'][^"']*@vite\/client[^"']*["'][^>]*><\/script>/g,
				"",
			);
		},
	};
}

export default defineConfig({
	root: "src",
	envDir: resolve(__dirname),
	plugins: [removeViteClient(), solidPlugin(), workletInline(), basicSsl()],
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
