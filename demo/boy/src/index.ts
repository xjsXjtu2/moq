import "@moq/boy/element";
import { Effect } from "@moq/signals";

const url = import.meta.env.VITE_RELAY_URL || "http://localhost:4443/anon";

const boy = document.querySelector("moq-boy");
if (boy) boy.url = url;

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
