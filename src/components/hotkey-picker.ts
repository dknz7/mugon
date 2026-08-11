import { api, type HotkeyParts, type HotkeyStatus, type KeyGroup } from "../api";

/**
 * The hotkey picker (Task 17), replacing the recorder.
 *
 * Recording required mugon to observe a keystroke while its own window had
 * focus, which is exactly the case that never worked. Picking observes nothing.
 *
 * The key list is fetched from the backend rather than declared here: the list
 * offered and the list `set_hotkey` accepts have to be the same object, or a key
 * ends up offered but unbindable — which is precisely how `:` was broken.
 */
export interface PickerElements {
  ctrl: HTMLInputElement;
  alt: HTMLInputElement;
  shift: HTMLInputElement;
  win: HTMLInputElement;
  key: HTMLSelectElement;
  clear: HTMLButtonElement;
  status: HTMLElement;
  hint: HTMLElement;
  warning: HTMLElement;
}

/** The dropdown entry meaning "no binding". Empty value, so it is falsy. */
const NONE = "";

/**
 * `Clear` is enabled while there is anything to clear — which includes lit chips
 * with no key chosen.
 *
 * Gating purely on the binding disabled the one control that resets the chips at
 * exactly the moment they were the only thing left set: select "None" with
 * modifiers ticked and they stay lit, by design, because `renderPicker` will not
 * touch them without a binding to render.
 *
 * Called from the chip handler as well as from `renderPicker`, because a chip
 * change with no key chosen writes nothing and so produces no `state-changed` to
 * re-render off.
 */
function syncClearEnabled(els: PickerElements) {
  const chipsLit = els.ctrl.checked || els.alt.checked || els.shift.checked || els.win.checked;
  els.clear.disabled = els.key.value === NONE && !chipsLit;
}

export function initPicker(els: PickerElements) {
  const write = () =>
    api.setHotkey(
      els.ctrl.checked,
      els.alt.checked,
      els.shift.checked,
      els.win.checked,
      els.key.value === NONE ? null : els.key.value,
    );

  // Every control writes immediately — no Save button, consistent with every
  // other control in the window.
  //
  // Modifiers write only when a key is chosen. Without that guard, ticking
  // "Ctrl" with the dropdown on "None" would send `key: null`, which clears the
  // binding — so the chips would silently wipe a hotkey the user was in the
  // middle of building.
  for (const chip of [els.ctrl, els.alt, els.shift, els.win]) {
    chip.addEventListener("change", () => {
      syncClearEnabled(els);
      if (els.key.value !== NONE) void write();
    });
  }
  els.key.addEventListener("change", () => void write());

  // Clears the binding *and* the modifiers in one action. Selecting "None" in
  // the dropdown does the same thing to the binding, but leaves the chips lit,
  // which reads as though something were still set.
  //
  // The chips are reset here rather than left to the re-render, because
  // `renderPicker` deliberately does not touch them when no binding exists —
  // see there.
  els.clear.addEventListener("click", () => {
    for (const chip of [els.ctrl, els.alt, els.shift, els.win]) chip.checked = false;
    syncClearEnabled(els);
    void api.setHotkey(false, false, false, false, null);
  });
}

/** Fills the dropdown once, on window open. Static for the process's life. */
export function renderKeyOptions(select: HTMLSelectElement, groups: KeyGroup[]) {
  const none = document.createElement("option");
  none.value = NONE;
  none.textContent = "None";

  const optgroups = groups.map((g) => {
    const group = document.createElement("optgroup");
    group.label = g.label;
    // `textContent`, not innerHTML: these are backend-owned strings, but the
    // same rule the device dropdown follows applies — nothing here is ever
    // parsed as markup.
    for (const key of g.keys) {
      const option = document.createElement("option");
      option.value = key;
      option.textContent = key;
      group.appendChild(option);
    }
    return group;
  });

  select.replaceChildren(none, ...optgroups);
}

export function renderPicker(
  els: PickerElements,
  opts: { hotkey: HotkeyParts | null; status: HotkeyStatus; bare: boolean },
) {
  // Only synced when a binding exists. With none, `AppState` has nothing to say
  // about modifiers, and overwriting them would destroy a combo mid-build: tick
  // Ctrl, then let any unrelated `state-changed` land — a volume nudge, a
  // device hotplug, a push-to-talk edge — and the chips would silently clear,
  // so choosing the key afterwards binds it bare. The `Clear` button resets
  // them explicitly instead.
  if (opts.hotkey !== null) {
    els.ctrl.checked = opts.hotkey.ctrl;
    els.alt.checked = opts.hotkey.alt;
    els.shift.checked = opts.hotkey.shift;
    els.win.checked = opts.hotkey.win;
  }

  // A saved key missing from the dropdown would silently snap the select to
  // "None" and read as though nothing were bound. It cannot happen — both ends
  // come from `hotkey::keys` — so if it ever does, say so rather than lie.
  //
  // Any previous orphan is removed first: left in place it would outlive the
  // binding that caused it and go on offering a key `set_hotkey` rejects.
  const key = opts.hotkey?.key ?? NONE;
  els.key.querySelector("option[data-orphan]")?.remove();
  const known = Array.from(els.key.options).some((o) => o.value === key);
  if (key !== NONE && !known) {
    const orphan = document.createElement("option");
    orphan.value = key;
    orphan.textContent = `${key} (unavailable)`;
    orphan.dataset.orphan = "";
    els.key.appendChild(orphan);
  }
  els.key.value = key;

  // Rendered verbatim, styled off `kind` (task-14-amendment §2). Deriving the
  // class from the label's wording would make a copy edit silently drop the
  // colour, with nothing failing on either side.
  els.status.textContent = opts.status.label;
  // The row is one fixed-width line and ellipsises. A long combo — "Bound —
  // press Ctrl + Alt + Shift + Win + BrowserRefresh to confirm" — would
  // otherwise tell the user to press something they cannot read. Same recovery
  // the error banner already uses for long COM strings.
  els.status.title = opts.status.label;
  els.status.classList.toggle("is-unconfirmed", opts.status.kind === "Bound");
  els.status.classList.toggle("is-inactive", opts.status.kind === "Inactive");

  syncClearEnabled(els);

  // Nothing to explain when there is no binding to use.
  els.hint.hidden = opts.hotkey === null;

  els.warning.hidden = !opts.bare;
}
