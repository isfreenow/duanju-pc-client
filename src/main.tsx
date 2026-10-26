import ReactDOM from "react-dom/client";
import App from "./App";

/**
 * No StrictMode here on purpose. Opening an episode kicks off a ~27 MB download
 * and a full decrypt; StrictMode's deliberate mount → unmount → remount would
 * double that traffic on every navigation in development, which reads like a bug
 * in the player rather than a dev-only artefact.
 */
ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(<App />);
