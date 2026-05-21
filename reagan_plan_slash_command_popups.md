# Slash command → popup overlays

## Goal

Slash commands currently expand into a "bottom pane" that grows up from the composer
and replaces inline content. Convert these into **centered floating popup overlays**
rendered on top of the unmodified main view. The popup is responsive to terminal size.

The `/` dropdown stays exactly as it is — it remains the command list. Picking a
command launches a popup view; it does not morph the dropdown.

## Affected files

- `crates/browser-use-tui/src/main.rs` — add `Surface::is_popup()`
- `crates/browser-use-tui/src/render.rs` — new `render_popup_overlay`, route popup
  surfaces through `render_main` + overlay
- `crates/browser-use-tui/src/theme.rs` — popup border / shadow color (reuse existing)

## Which surfaces become popups

All current `is_bottom_pane()` surfaces:

- `History` (`/history`, Tab)
- `Browser` (`/browser`, F2)
- `BrowserSelect`
- `Model` (`/model`)
- `Account` (`/auth`)
- `ApiKey` (sub-form of `/auth`)
- `Telemetry` (`/laminar`)
- `Developer` (Ctrl+E)

Fullscreen surfaces stay fullscreen: `Setup`.

## Design

- Popup is a centered floating rectangle with a 1-cell border.
- Dimensions are responsive:
  - `width = area.width` when the terminal is 40 columns or narrower; otherwise
    use `area.width.saturating_sub(8).min(84).max(40)`.
  - `height = area.height` when the terminal is 10 rows or shorter; otherwise
    clamp desired content height to `10..=26`, then cap to the terminal margin
    and re-raise to `min(10, area.height)`.
- Position: centered horizontally and vertically.
- Body uses the existing `surface_header_lines` + `surface_lines` + `surface_footer`
  so all picker logic stays untouched.
- `Clear` is rendered under the popup so the main view doesn't bleed through.

## Render flow

```
render(frame, app):
    if surface == Setup or is_first_run: render_surface (unchanged, fullscreen)
    else if surface.is_popup():
        render_main (treating surface as if it were Main for layout)
        render_popup_overlay(frame, area, app, state, surface)
    else if surface.uses_main_view():
        render_main (unchanged)
    else:
        render_surface
```

In `render_main`, when a popup is active we want the composer to render normally
(not expand into a bottom pane). We pass an effective surface of `Main` into the
layout-height calculation so the composer pane stays at its normal idle height.

## Selection / input / keys

Input handling stays as-is — `selected_row`, Esc, Enter all continue to work
because the `Surface` value is unchanged. Only the rendering moves.

## Responsiveness rules

- Min popup: 40×10. If terminal is smaller, popup fills the entire area.
- Max popup: 80 wide × 24 tall (subject to terminal cap).
- Long lists (History) scroll inside popup — keep the existing "skip rows to keep
  selection visible" logic in `render_bottom_pane`; move it into the popup body.

## Step-by-step

1. Add `Surface::is_popup(self) -> bool` (matches current `is_bottom_pane` set).
2. Refactor `render` to detect popup surfaces and call `render_main` followed by
   `render_popup_overlay`.
3. In `render_main`, allow an "effective surface for layout" override so popup
   surfaces don't drive the bottom pane height path.
4. Implement `render_popup_overlay` (centered rect, Clear, border, header, body,
   footer) reusing `surface_header_lines`/`surface_lines`/`surface_footer`.
5. Verify keyboard, Esc-close, selection scroll all still work.
6. `cargo build` + `cargo test` and inspect any snapshot/string-match test deltas.
