import { renderHero } from "./components/hero";

/**
 * Task 13 builds the shell only — there is no IPC in this file on purpose.
 * Task 14 adds `invoke`/`listen`, mirrors `AppState` onto the ids below and
 * calls `setHeroMuted` from ./components/hero.
 *
 * The one behaviour that lives here is the settings sheet, which is pure
 * presentation: Task 14 should not add a second click handler to the cog.
 */

const hero = document.querySelector<HTMLElement>("#hero");
if (hero) renderHero(hero);

const cog = document.querySelector<HTMLButtonElement>("#settings-cog");
const panel = document.querySelector<HTMLElement>("#settings-panel");

cog?.addEventListener("click", () => {
  if (!panel) return;
  const open = panel.hidden;
  panel.hidden = !open;
  cog.setAttribute("aria-expanded", String(open));
});

// Escape closes the sheet — it has no close button of its own, and the cog is
// the only other way out.
window.addEventListener("keydown", (e) => {
  if (e.key !== "Escape" || !panel || panel.hidden) return;
  panel.hidden = true;
  cog?.setAttribute("aria-expanded", "false");
});
