# Browser Use logo — ASCII sprite catalog

Reference renders of the Browser Use orbit mark (two crossed elliptical rings)
as block-character sprites. Source: `~/Desktop/Browser Use Assets/bu-logo-black.svg`.

All renders are folded across both mirror axes so they're symmetric, and
verified as a single connected component (no floating pieces). Pipeline:
LANCZOS-resize the SVG hi-res → symmetrize alpha → threshold → (optional
erode/skeletonize) → downsample to quadrant block characters.

Future idea: animated ASCII art (OpenAI-style). Likely approach — phase-shift
the two rings against each other, or pulse stroke weight (erode/dilate over
time), rendered each frame to the same footprint.

## Sprites

### 8×4 — smallest connected (last header version)
```
▄▀▜▙▟▛▀▄
▜▟▘  ▝▙▛
▟▜▖  ▗▛▙
▀▄▟▛▜▙▄▀
```

### 8×4 — original symmetric fill (thicker, more solid)
```
▟▀▜██▛▀▙
▜▟▀  ▀▙▛
▟▜▄  ▄▛▙
▜▄▟██▙▄▛
```

### 10×4 — skeleton-traced (thin centerlines, looser rings)
```
▗▀▀▜▌▐▛▀▀▖
▜▄▀    ▀▄▛
▟▀▄    ▄▀▙
▝▄▄▟▌▐▙▄▄▘
```

### 10×5 — thinned, connected
```
▗▞▀▜▄▄▛▀▚▖
█ ▟▀  ▀▙ █
▐█      █▌
█ ▜▄  ▄▛ █
▝▚▄▟▀▀▙▄▞▘
```

### 10×5 — filled accurate, gap-free
```
▄█▀████▀█▄
█▙▟▛▘▝▜▙▟█
▐█▌    ▐█▌
█▛▜▙▖▗▟▛▜█
▀█▄████▄█▀
```

### 12×6 — compact accurate
```
▗████▄▄████▖
█▌▗▟▛▘▝▜▙▖▐█
▜█▛▘    ▝▜█▛
▟█▙▖    ▗▟█▙
█▌▝▜▙▖▗▟▛▘▐█
▝████▀▀████▘
```

### 14×7 — clean accurate, crossings clearly read
```
▗▟████▄▄████▙▖
█▛ ▗▟█▀▀█▙▖ ▜█
▜▙▟█▀    ▀█▙▟▛
 ██        ██
▟▛▜█▄    ▄█▛▜▙
█▙ ▝▜█▄▄█▛▘ ▟█
▝▜████▀▀████▛▘
```

### 16×8 — high fidelity
```
 ▄████▙▄▄▟████▄
▟█▘ ▗▄█▀▀█▄▖ ▝█▙
██▗▟█▀    ▀█▙▖██
▝██▀        ▀██▘
▗██▄        ▄██▖
██▝▜█▄    ▄█▛▘██
▜█▖ ▝▀█▄▄█▀▘ ▗█▛
 ▀████▛▀▀▜████▀
```

## Constraints learned

With quadrant block characters there are three dials — **size**, **stroke
weight**, **connectedness** — and you can only have two. The two crossed
rings of the Browser Use mark need ~10+ columns to show their structure;
below that the bands fuse into a rounded blob with a hole. Skeletonizing
buys thinner strokes but widens the central void and loosens the bands away
from the real logo proportions.
