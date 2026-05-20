# Animated 3D ASCII Browser Use logo — plan

## What the logo actually is

Two circles in 3D, tilted in opposite directions so they cross like an
atom/orbit mark. The existing sprites in `reagan_logo_sprites.md` are
*static projections* of that 3D object. If we treat the logo as the real 3D
geometry (two parametric circles) and project it each frame, animation
becomes "free": rotate the geometry, re-project, re-render.

This matches the Alex Harri post's pipeline:
**scene → signed-distance field per pixel → brightness → character ramp**.
We're not tweening sprites; we're rendering.

## Geometry

Two circles of radius `R` centered at the origin, each in its own plane:

```text
Ring A: plane normal = rotate_y(+α) · z_hat     (α ≈ 35°)
Ring B: plane normal = rotate_y(−α) · z_hat
```

Parametric points:

```text
P_A(t) = R · (cos t · u_A + sin t · v_A)
P_B(t) = R · (cos t · u_B + sin t · v_B)
```

where `(u, v)` are orthonormal in each ring's plane. This gives the crossed-
ellipse look the static sprites approximate.

## Per-frame pipeline

For frame `f` at time `τ = f / fps`:

1. Build a global rotation `M(τ)` (the animation — see modes below).
2. Sample each ring at N points (N ≈ 240 is plenty), apply `M(τ)`,
   orthographically project `(x, y, z) → (x, y)` with a depth buffer for `z`.
3. For each terminal cell (using quadrant sub-pixels for 2× resolution like
   the existing sprites), compute the **min distance** to the nearest
   projected ring sample. Track the corresponding `z` for shading.
4. Map distance → brightness with a soft falloff: `b = clamp(1 - d/stroke, 0, 1)`.
   Add a directional light term using the local tangent's `z` (back-facing
   parts of the ring dim). This is what gives the "tube" feel.
5. Brightness → character ramp. Coarse but legible:
   ```
   " .:-=+*#%@"        # classic
   " ░▒▓█"             # blockier, works well in monospace
   ```
   For our existing aesthetic, keep quadrant blocks (`▘▝▖▗▀▄▌▐▙▟▛▜█`) and
   pick the block whose 2×2 sub-pixel mask best matches the cell's coverage.

The footprint stays the same as a static sprite (e.g. 16×8) — only the
character contents change per frame.

## Animation modes (pick one or chain them)

### 1. Coin flip — rotate around the **x-axis**

Rings compress vertically through zero, flip, come back. Reads as a literal
spinning coin.

```text
τ = 0.00            τ = 0.12             τ = 0.25 (edge-on)
 ▄████▙▄▄▟████▄     ▄▟████▄▄████▙▄      ▁▂▃▄▅▄▃▂▁
▟█▘ ▗▄█▀▀█▄▖ ▝█▙   ▟█▙▄▀▀▀▀▀▀▄▄█▛       ▂▃▄▅▆▅▄▃▂
██▗▟█▀    ▀█▙▖██   █████▄▄▄▄████        ▃▄▅▆▇▆▅▄▃
▝██▀        ▀██▘   ▝████▀▀▀▀████▘       ▄▅▆▇█▇▆▅▄
▗██▄        ▄██▖   ▗████▄▄▄▄████▖       ▃▄▅▆▇▆▅▄▃
██▝▜█▄    ▄█▛▘██   █████▀▀▀▀████        ▂▃▄▅▆▅▄▃▂
▜█▖ ▝▀█▄▄█▀▘ ▗█▛   ▜█▛▀▄▄▄▄▄▄▀▀█▛       ▁▂▃▄▅▄▃▂▁
 ▀████▛▀▀▜████▀     ▀▜████▀▀████▛▀
```

### 2. Y-axis spin — like a globe

Rings sweep horizontally; one ring eclipses the other through the cycle.
Most "logo-like" rotation since both rings stay visible most of the time.

```text
τ = 0.0              τ = 0.15             τ = 0.30
 ▄████▙▄▄▟████▄      ▄▟███▙▄▟██▙▄▖       ▟█▄▄███▖
▟█▘ ▗▄█▀▀█▄▖ ▝█▙    ▟█▘▗▟█▀▘ ▝█▙▘▘      █▘ ▗▟▀▀▙▖
██▗▟█▀    ▀█▙▖██    ██▟█▘     ▝█▙▖      █▟█▘   ▝▙
▝██▀        ▀██▘    ▝██        ▝██      ██      █
▗██▄        ▄██▖    ▗██        ▄██      ██      █
██▝▜█▄    ▄█▛▘██    ██▝▜█▖    ▗█▛       █▜█▖   ▗▛
▜█▖ ▝▀█▄▄█▀▘ ▗█▛    ▜█▖▝▜█▄ ▗█▛▘▘       █▖ ▝▜▄▄▟▘
 ▀████▛▀▀▜████▀      ▀▜███▛▀▜██▛▀▘       ▜█▀▀███▘
```

### 3. Tumble — combined `x + y` rotation

Looks like the logo is being thrown in the air. Visually richest, most
distracting. Save for special moments (boot splash, idle screensaver).

### 4. Independent ring spin — phase shift only

Keep the bounding box static; rotate ring A and ring B around their own
plane normals at different rates. The *envelope* of the logo doesn't move,
but the inner crossings shimmer. Subtle. Great for an idle ambient state.

### 5. Pulse — radius/stroke breathing

No 3D at all: animate stroke width (erode/dilate) or radius over a sine.
Cheap fallback if the SDF render is too heavy. Mentioned in the existing
sprite notes.

## Recommended sequence

Boot/splash: **coin flip** (#1) once → settle into **y-axis spin** (#2) on
loop at ~12 fps. When the agent is idle, downshift to **independent spin**
(#4) at 4–6 fps so it isn't visually loud.

## Render budget

- 16×8 cell footprint = 32×16 sub-pixels. Per-frame work: 240 ring samples
  × 2 rings = 480 projected points, then 32×16 = 512 nearest-distance
  lookups. Trivial. 60 fps easily, but we should cap at ~15 fps so it
  doesn't dominate the TUI redraw.
- Render off a wall-clock timer, not the input loop, so the spin keeps
  going while the user is idle.

## Implementation sketch (Rust, inside `crates/browser-use-tui`)

```rust
// new module: src/logo.rs
pub struct LogoFrame {
    pub rows: Vec<String>,    // pre-baked quadrant-block lines
}

pub struct LogoAnimator {
    mode: Mode,
    started: Instant,
    fps: f32,
    size: (u16, u16),   // cells
}

impl LogoAnimator {
    pub fn frame(&self, now: Instant) -> LogoFrame { /* SDF render */ }
}
```

Hook into `render.rs` wherever the static sprite is drawn today. Drive
repaints from the existing tick loop (whatever cadence already powers
status-bar updates) — don't add a second timer.

## Open questions

1. Which size do we animate? `16×8` reads best but eats more vertical
   space than the current header. `10×5` is the minimum where the two
   rings stay distinguishable mid-rotation.
2. Where does it live in the UI — splash only, persistent header, or
   idle-state easter egg?
3. ASCII ramp vs. quadrant blocks? Quadrant blocks match the existing
   sprite aesthetic; an ASCII ramp (`.:-=+*#%@`) would be a stylistic
   break but reads as more "3D-shaded".
