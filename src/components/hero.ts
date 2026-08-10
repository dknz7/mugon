/**
 * The hero microphone — the one element in the window that has to be readable
 * from across the desk (DESIGN.md §5).
 *
 * Live:  teal mic, a slow breathing ring, a soft glow, a "Live" caption.
 * Muted: ring collapsed, mic desaturated, a red strike drawn across it, and a
 *        "Muted" caption in the danger colour.
 *
 * Pure CSS keyframes — no rAF loop, no canvas. Under
 * `prefers-reduced-motion: reduce` the motion stops but the colour, the strike
 * and the caption all stay, so the mute state is never carried by animation
 * alone: the animation is decoration, the state is information.
 *
 * Task 13 is markup only. Task 14 owns the state and calls `setHeroMuted`.
 */

/** Diagonal length of the strike line in viewBox units, for the draw-on dash. */
const STRIKE_LENGTH = 22;

const MARKUP = `
  <div class="hero">
    <div class="hero-stage">
      <div class="hero-glow"></div>
      <div class="hero-rim"></div>
      <div class="hero-ring"></div>
      <svg class="hero-mic" viewBox="0 0 24 24" width="64" height="64"
           fill="none" stroke="currentColor" stroke-width="1.5"
           stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        <rect x="9" y="2" width="6" height="11" rx="3"/>
        <path d="M5 10a7 7 0 0 0 14 0"/>
        <line x1="12" y1="17" x2="12" y2="20.6"/>
        <line x1="8.2" y1="20.6" x2="15.8" y2="20.6"/>
        <line class="hero-strike hero-strike-cut" x1="4.6" y1="19.4" x2="19.4" y2="4.6"
              stroke-dasharray="${STRIKE_LENGTH}" stroke-dashoffset="${STRIKE_LENGTH}"/>
        <line class="hero-strike" x1="4.6" y1="19.4" x2="19.4" y2="4.6"
              stroke-dasharray="${STRIKE_LENGTH}" stroke-dashoffset="${STRIKE_LENGTH}"/>
      </svg>
    </div>
    <p class="hero-caption" role="status">Live</p>
  </div>`;

export function renderHero(container: HTMLElement): void {
  container.innerHTML = MARKUP;
}

export function setHeroMuted(container: HTMLElement, muted: boolean): void {
  const hero = container.querySelector<HTMLElement>(".hero");
  if (!hero) return;

  hero.classList.toggle("is-muted", muted);

  const caption = hero.querySelector<HTMLElement>(".hero-caption");
  if (caption) caption.textContent = muted ? "Muted" : "Live";
}
