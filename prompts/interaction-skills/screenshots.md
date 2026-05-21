# Screenshots

`screenshot(label)` and `capture_screenshot(...)` write a PNG of the current viewport and attach it to the next model turn. The user does not see the pixels inline in the terminal; inspect the image yourself and summarize what it shows, or provide the saved artifact path when the user asks for the screenshot. The file is in **device pixels** — on a 2x display a 2296x1143 CSS viewport produces a 4592x2286 PNG.

That matters for two reasons:

1. **Click coordinates are CSS pixels.** Don't read a target off the image and pass it to `click_at_xy()` directly without dividing by `devicePixelRatio`. The simplest workflow is to take the screenshot, look at it in a viewer that shows CSS coordinates, or measure relative positions and use `js("window.devicePixelRatio")` to convert.

2. **Some LLMs reject images > 2000 px per side.** Long sessions on 2x displays can hit this. Prefer `screenshot_clip(...)` for a smaller CSS-pixel region when only part of the page matters.

```python
screenshot("before_submit")
screenshot_clip("menu_area", x=20, y=80, width=460, height=520)
```

Use full-page screenshots (`full=True`) only when you need to see content below the fold — they are much larger and slower than viewport-only.
