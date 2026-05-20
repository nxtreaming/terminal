# New Welcome Screen Explorations — Plan

**Date:** 2026-04 (current session)
**Goal:** Add fresh welcome screen layout ideas to the living compare page (`reagan_logo_animation_compare.html`) without removing or altering any of the existing A–L mockups. The compare page remains the single source of truth for visual direction.

## Why new examples?
The current 12 layouts (A–L) cover a good spectrum:
- Grok-centric (A, G)
- Information-dense (D, H, L)
- Minimal / question-forward (F, J, K)
- Backdrop / atmospheric (C, I)

We still lack strong representatives for:
1. **Activity / history forward** power-user view (people who live in the tool all day want to see "what happened since I left" at a glance).
2. **Ultra-focused "intent capture"** view — when the user just wants to type and the chrome disappears.
3. **Bimodal launcher** — explicit two-path decision (new vs resume) that feels like two doors rather than a menu list.

These three new directions (M, N, O) close the gap and give us better coverage for A/B testing later in the real TUI.

## The three new mockups to add

### M · Activity feed (left content, right logo)
- Left column: short vertical feed of recent agent runs + worktree switches with timestamps and one-line outcome.
- Right: the braille logo (smaller footprint than A).
- Still shows the prompt at the very bottom.
- Feels like "I just landed, show me the pulse".
- Good for users who treat Browser Use as their daily driver.

### N · Zen focus mode
- Tiny dim logo as a watermark near the top or in a corner.
- Massive centered prompt area.
- Three "starter chips" (soft pills) with example natural-language tasks.
- Almost zero persistent chrome except the prompt line itself.
- The ultimate "get out of my way" variant. Inspired by the best single-purpose CLIs.

### O · Explicit dual launcher
- Logo + title at top.
- Two equal-width, card-like columns side-by-side:
  - Left: "New worktree" + big key hint
  - Right: "Resume last session" + last-used info
- Visually says "two primary intents" rather than a list of three.
- Stronger visual affordance than the current vertical menu.

All three will:
- Use the exact same 18×9 braille animated logo (so fair visual weight comparison).
- Be appended inside the existing `.mockups` grid (so they appear as rows 5–6 of the 3-column layout).
- Receive live animation wiring in the JS `panels` array.
- Get their own CSS rules (`.mockup-feed`, `.mockup-zen`, `.mockup-dual`) following the existing pattern (no !important, monospace everywhere inside the terminal frame, etc.).

## Non-goals for this iteration
- Do not port any of M/N/O to Rust yet.
- Do not change the currently-shipping welcome (A) in `crates/browser-use-tui/src/welcome.rs`.
- Do not remove or restyle A–L.
- Keep the file self-contained (single HTML).

## Implementation order
1. Write this plan (`reagan_plan_new_welcome_screens.md`).
2. Add the three new CSS blocks for the mockup internals.
3. Insert the three new `<div class="mockup ...">` blocks right before the closing `</div>` of the first `.mockups` container.
4. Wire the three new `id="welcome-..."` elements into the `panels` JS array.
5. Add click handlers if any of the new ones deserve interactivity (M and O are good candidates for "click logo still throws").
6. Quick manual visual pass in a browser: open the HTML, confirm old A–L are untouched and new M–O render with spinning logos and reasonable spacing.
7. (Optional later) Once we pick a winner from M/N/O, open a follow-up task to implement the chosen layout in Ratatui + wire it behind a feature flag or settings toggle.

## Success criteria
- The compare page still contains every original mockup exactly as before.
- Three new, clearly labeled, fully animated mockups are visible and can be compared at a glance.
- No console errors, no layout breakage on the existing cards.
- All glyphs inside the mockup frames remain 12px monospace (the "honest render" rule).

This keeps the compare page as the evolving design notebook rather than a frozen artifact.
