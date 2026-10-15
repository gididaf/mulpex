import { mount } from "svelte";
import "@xterm/xterm/css/xterm.css";
import "./styles.css";
import App from "./App.svelte";
import { IS_LINUX } from "./lib/platform";

// Before mount, so the first layout (and the terminal geometry measured from
// it) already leaves room for the Linux menu bar.
if (IS_LINUX) document.documentElement.classList.add("has-menubar");

const app = mount(App, {
  target: document.getElementById("app")!,
});

export default app;
