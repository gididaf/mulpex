import { mount } from "svelte";
import App from "./App.svelte";

mount(App, { target: document.getElementById("app")! });

// Needed to install to the Home Screen (and, later, for push). Service workers
// only exist on https or localhost; a LAN http page simply goes without.
if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}
