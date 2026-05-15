# tui-style-proposal.html — v1 → v2 changelog

This is a note to whichever agent (or human) opens `tui-style-proposal.html` next.
v1 looked good in a browser but lied about depth that Ratatui can't render, used
too many colors, and decided 13 screens before settling the grammar. v2 fixes
that. Below is what changed and *why*.

The owner explicitly decided to **keep the ASCII text glyphs** (`[box]`, `[x]`,
`[>]`, `[ ]`, `[!]`, `+----+`, `<..>`, `[ button ]`) instead of switching to
Unicode tree/checkbox glyphs (`├ └ ☑ ▸ ▣`). Do not "improve" them back to
Unicode in a future pass without asking.

---

## What changed

### 1. Stripped CSS depth that can't port to a terminal
- **Removed:** `box-shadow` on every panel, `linear-gradient` overlays on `.terminal` and the body grid texture, every `border-radius`, `clamp()` typography.
- **Why:** the mock has to look like what Ratatui can actually produce. If you can't render it in a terminal, it shouldn't be in the spec. v1's depth made the layout *look* designed when it wasn't — the underlying structure was mediocre once you imagined the shadows gone.
- **Where:** `:root`, `body`, `.note`, `.screen`, `.terminal` rules in the `<style>` block.

### 2. Palette cut from 8 to 4
- **Removed:** `--blue`, `--red`, `--yellow`, `--green-2`, `--orange-2` tokens. Removed the `.b .r .y` CSS classes entirely. Chrome colors (`--page`, `--surface`, `--panel`, `--line`, `--dim`) are kept but no longer advertised as "tokens" — they're structure, not vocabulary.
- **Why:** v1 declared "one active accent" as a rule then used five accents in the same screen. The inspiration uses one loud color (orange) and one desaturated functional color (green), full stop. Errors become orange + `[!]`, links get underline. Less to learn, less to fight with.
- **Where:** Color Tokens section + every screen mock. Also added a `.palette-note` paragraph stating the rule explicitly.

### 3. Kept the ASCII glyphs (NOT a glyph swap)
- **Did NOT do:** the `[box] / [x] / [>] / [ ]` → `┌ ├ └ ☑ ▸ ☐` swap that the v2 plan originally proposed.
- **Why:** owner preference. The ASCII style was deliberate. Don't switch.
- **Implication:** the `+----+` panel boxes around inputs also stay. Only used for the composer input though — *not* for grouping general content (that's hairlines now).

### 4. Screen set cut from 13 to 6
- **Removed:** the separate API-key, model-picker, browser-picker, browser-detail, history, developer, telemetry-key screens.
- **Replaced with:** one **Setup** screen template that explicitly annotates "the same template renders for: account, model, browser, api key." Running/Result/Failed are now visibly the same layout in three states. Palette is the modal overlay.
- **Why:** v1 designed 13 screens before deciding the grammar. That's premature commitment. With 6 screens you can see whether the primitives actually compose; with 13 you're just decorating different content. The collapse also exposes the truth that failure isn't a new screen, it's a state.

### 5. Per-screen layout fixes
- **Home (02):** giant `BROWSER USE` pixel wordmark deleted. Replaced with one-line header `browser-use ─── Local Chrome · idle`. Recent work is plain rows (one `[x]`, prompt, right-aligned time), not rounded-rect tiles. Button row `[ Run task ] [ History ] [ Browser ]` replaced with chip row using the same `[ name ]` notation but used consistently across all screens.
- **Setup (01):** the v1 browser picker used a 3-column table (`name / behavior / best-for`). Reduced to 2 columns (name + description) and the whole thing now lives under a single `CHOOSE BROWSER` label, matching the label-above-stack pattern used everywhere else.
- **Running (03):** tab strip now has a hairline rule directly under it (was floating). Agent-context band (`AGENT / MODEL / BROWSER / TASK`) sits between the tab strip and the body — it tells you what the agent *is*. Bottom band tells you what the runtime *looks like*. Same primitive, different intent.
- **Result (04):** floating `+-- BROWSER --+` sticker in the right column **deleted**. That info now lives in the BROWSER column of the bottom metric band. Same fact, one place. Link is underlined, not coloured (no blue exists).
- **Failed (05):** identical to Running. Only the glyph (`[>]` → `[!]`), the status word, and the progress-bar pattern change.
- **Palette (06):** outer `+----+` box removed. Frame is two hairline rules (top and bottom) only. Background screen stays visible underneath.
- **Why on all of these:** the v1 fix-list said "every screen reinvents its layout." Now every screen pulls from the same four primitives in the same vertical order: objective stack → tab strip → context band → body → composer → chip row → metric band → progress bar. The data changes, the shape doesn't.

### 6. States rows under every primitive
- **Added:** a 4-cell `.states` row under each of the four primitives (Tree, Tabs, Band, Progress) showing empty/working/done/failed (or inactive/active/unread/disabled for tabs).
- **Why:** v1 showed each primitive in its happy state. You can't build a widget from one frame — you need to see what it looks like when it's empty, when it's loading, when it succeeds, when it breaks. Now there's no ambiguity about how to render failure or idle.
- **Where:** Component Grammar section, four `.component` blocks.

### 7. Tabular numerals + no ligatures
- **Added to `body`:** `font-variant-numeric: tabular-nums` and `font-feature-settings: "calt" 0, "liga" 0`.
- **Why:** without this, SF Mono ligatures hijack `..`, `->`, `<=` and rearrange them at render time, and proportional digits make the metric band's numbers wobble between rows. Tabular nums + no ligatures = the "looks like a real readout" feel from the inspiration.
- **Implication for Ratatui:** the terminal must use a font with the same properties. JetBrains Mono works; Cascadia Code works; some "Nerd Fonts" patched builds re-enable ligatures and break this — flag in any setup doc.

### 8. Width comparison section added
- **Added:** new "Width Reflow" section showing the Running screen at 80 cols (narrow) vs 140 cols (wide), side by side.
- **Why:** v1 implicitly assumed everyone has 140+ columns. Ghostty splits, tmux panes, and small laptop screens will routinely give you 80. At 80, tab labels abbreviate (`TERMINAL` → `TERM`, `RUNTIME` → `RUN`), columns shrink, but the four regions stay in the same order — nothing collapses or disappears. This is the budget that every primitive has to fit into.

### 9. Footer collapsed to `/`
- **Removed:** the per-screen footer keymap (`enter send  shift+enter newline  f2 browser  tab history  / actions`).
- **Replaced with:** a single `/` glyph in the bottom-right corner of every Running/Result/Failed/Palette screen. The long keymap moves *into* the palette.
- **Kept:** `enter select   esc back` on the Setup screen, because those are genuinely contextual to the choice you're making (and there's no palette in setup flow).
- **Why:** the footer competed visually with the composer and the chip row, and most users only need it the first time. Hide the long list behind the palette.

---

## What we deliberately did NOT change

- **Glyph style.** ASCII text glyphs are an owner decision. See section 3 above.
- **Document structure.** v1 puts Component Grammar between Palette and Screens; some agents would argue Grammar should come first. We did not reorder. It's a small win not worth the diff.
- **HTML as the artifact medium.** Still HTML. A future spike (Ratatui prototype) might supersede it, but for design conversation HTML side-by-side comparisons are still the fastest medium.
- **Hero / wordmark in the doc itself.** The doc has a big "RUNTIME COCKPIT, NOT CHAT LOG" heading at the top. That's the *doc's* hero, not the TUI's. The TUI has no wordmark anymore (Home screen 02 confirms this).

---

## What still needs work

These are open and intentionally not addressed in v2:

1. **Motion plans.** We say "spinner = `<..>`" and "progress = orange partial" but never specify the cycle (frames, FPS, easing). A future pass should add a `motion` section.
2. **No real Ratatui prototype yet.** This is still a static HTML mock. The next step is a spike: implement just the four primitives (tree, tabs, band, progress) as Ratatui widgets, take terminal screenshots, embed them in this doc to *prove* it ports.
3. **Empty-state copy.** Home screen's `Tell the browser what to do...` placeholder is fine; Failed screen's `Tell the agent how to recover...` is okay. The other inputs reuse "Type to steer the agent..." which is workable but not loved.
4. **Setup template doesn't show api-key variant.** The annotation under Setup *describes* it but doesn't render it. Acceptable for now (the layout is obvious) but should be added once the primitive set is locked.
5. **Failed progress-bar pattern.** Currently a broken stutter (`|||...||..|`). It conveys "interrupted" but might read as noise. Worth A/B-ing against a simple "stop at where it died, no animation."

---

## Quick reference: the four primitives

If you only remember one section, remember this:

1. **Objective Tree** — top, fixed, never scrolls. Tells the user what is happening.
2. **Runtime Tabs** — workspace switcher. Active tab gets green, unread gets `*`.
3. **Metric Band** — small-caps label above value, stable columns, tabular nums.
4. **Progress Rail** — always present. Idle muted, working orange, done green, failed broken.

Every screen is these four plus a composer and a chip row. If a new screen
wants a new primitive, that's a strong signal we're either solving the wrong
problem or the existing primitives need to flex.
