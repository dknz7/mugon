import { api } from "../api";

export function initRecorder(
  display: HTMLElement,
  record: HTMLButtonElement,
  clear: HTMLButtonElement,
) {
  record.addEventListener("click", () => {
    if (display.classList.contains("is-recording")) {
      api.cancelRecording();
    } else {
      api.beginRecording();
    }
  });
  clear.addEventListener("click", () => api.clearHotkey());
}

export function renderRecorder(
  display: HTMLElement,
  record: HTMLButtonElement,
  warning: HTMLElement,
  opts: { recording: boolean; combo: string | null; bare: boolean },
) {
  display.classList.toggle("is-recording", opts.recording);
  record.textContent = opts.recording ? "Cancel" : "Record";
  // amendment §2: `combo` is a pre-formatted display string owned by the
  // Rust side (`Hotkey::display()`). Render it as given — do not parse it.
  display.textContent = opts.recording
    ? (opts.combo ?? "Press a key…")
    : (opts.combo ?? "Not set");
  // The warning's copy is authored in index.html (a Task 13 ::before-backed
  // static string) — only its visibility toggles here.
  warning.hidden = !opts.bare;
}
