const FLOOR = -60;

/**
 * Maps dBFS to a 0-100% bar width against the -60dB floor, colouring the
 * fill via the `.is-warn` / `.is-hot` classes styles.css already defines
 * (accent below -12, warn -12..-3, danger above -3 — DESIGN.md §4.7).
 *
 * Called once per `level` tick (~30Hz). Must touch only the meter bar and
 * its readout — see task-14-amendment.md §7.
 */
export function renderMeter(bar: HTMLElement, value: HTMLElement, db: number) {
  const clamped = Math.max(FLOOR, Math.min(0, db));
  const pct = ((clamped - FLOOR) / -FLOOR) * 100;
  bar.style.width = `${pct}%`;
  bar.classList.toggle("is-hot", db > -3);
  bar.classList.toggle("is-warn", db <= -3 && db > -12);
  value.textContent = db <= FLOOR ? `${FLOOR.toFixed(1)} dB` : `${db.toFixed(1)} dB`;
}
