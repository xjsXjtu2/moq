import "@moq/boy/element";
import { Effect } from "@moq/signals";

// Priority: ?url= query param > VITE_RELAY_URL env var > default localhost
const params = new URLSearchParams(location.search);
const url = params.get("url") ?? import.meta.env.VITE_RELAY_URL ?? "http://localhost:4443/anon";

const boy = document.querySelector("moq-boy");
if (boy) {
	boy.url = url;
	boy.showTsWatermark = params.has("ts-watermark");
	console.log(`url=${boy.url}, showTsWatermark=${boy.showTsWatermark}`)
}

const about = document.getElementById("about");
if (boy && about) {
	const effect = new Effect();
	effect.run((inner) => {
		about.hidden = inner.get(boy.expanded) !== undefined;
	});
	// Keep a reference and clean up on page unload to avoid the
	// "Signals was garbage collected without being closed" warning.
	window.addEventListener("beforeunload", () => effect.close());
}
