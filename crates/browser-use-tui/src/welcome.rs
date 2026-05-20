//! Centered "Grok-style" welcome screen: branch+cwd top bar, animated 3D braille
//! BU logo, menu, tip, version footer. Port of mockup A from the HTML compare page.

use ratatui::text::{Line, Span};

use crate::theme::{bold, muted, text_style};

// ─────────────────────────── Geometry ───────────────────────────
const RING_SAMPLES: usize = 220;
const Y_SQUASH_BASE: f32 = 0.55; // monospace cell aspect for 2×2 supersample
const TILT: f32 = std::f32::consts::PI / 3.0;
const ROLL: f32 = std::f32::consts::PI / 4.0;

type M3 = [[f32; 3]; 3];
type V3 = [f32; 3];

fn rot_x(a: f32) -> M3 {
    let (c, s) = (a.cos(), a.sin());
    [[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]]
}
fn rot_y(a: f32) -> M3 {
    let (c, s) = (a.cos(), a.sin());
    [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]]
}
fn rot_z(a: f32) -> M3 {
    let (c, s) = (a.cos(), a.sin());
    [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]]
}
fn mul(a: &M3, b: &M3) -> M3 {
    let mut r = [[0.0_f32; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            for k in 0..3 {
                r[i][j] += a[i][k] * b[k][j];
            }
        }
    }
    r
}
fn apply(m: &M3, v: V3) -> V3 {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

fn base_orientations() -> (M3, M3) {
    (
        mul(&rot_z(ROLL), &rot_y(TILT)),
        mul(&rot_z(-ROLL), &rot_y(TILT)),
    )
}

fn ring_points(base: &M3, global: &M3, radius: f32, y_squash: f32) -> Vec<V3> {
    let m = mul(global, base);
    (0..RING_SAMPLES)
        .map(|i| {
            let t = (i as f32 / RING_SAMPLES as f32) * std::f32::consts::PI * 2.0;
            let p = apply(&m, [t.cos() * radius, t.sin() * radius, 0.0]);
            [p[0], p[1] * y_squash, p[2]]
        })
        .collect()
}

const BRAILLE_BITS: [[u32; 2]; 4] = [[1, 8], [2, 16], [4, 32], [64, 128]];

/// Render the BU logo as a vector of braille-encoded strings, one per cell row.
/// `rx`, `ry` are global rotations applied to the orbit-mark geometry.
pub fn render_braille_logo(
    w_cells: usize,
    h_cells: usize,
    radius: f32,
    stroke: f32,
    rx: f32,
    ry: f32,
) -> Vec<String> {
    let (base_a, base_b) = base_orientations();
    let sub_x = 2usize;
    let sub_y = 4usize;
    let sx = w_cells * sub_x;
    let sy = h_cells * sub_y;
    let cx = sx as f32 / 2.0;
    let cy = sy as f32 / 2.0;
    let y_squash = Y_SQUASH_BASE * (sub_y as f32 / 2.0);

    let global = mul(&rot_y(ry), &rot_x(rx));
    let pts_a = ring_points(&base_a, &global, radius, y_squash);
    let pts_b = ring_points(&base_b, &global, radius, y_squash);

    let stroke2 = stroke * stroke;
    let mut lines = Vec::with_capacity(h_cells);

    for cy_idx in 0..h_cells {
        let mut row = String::with_capacity(w_cells * 3);
        for cx_idx in 0..w_cells {
            let mut bits: u32 = 0;
            for dy in 0..sub_y {
                for dx in 0..sub_x {
                    let px = (cx_idx * sub_x + dx) as f32 - cx + 0.5;
                    let py = (cy_idx * sub_y + dy) as f32 - cy + 0.5;
                    let mut min2 = f32::INFINITY;
                    for p in &pts_a {
                        let d = (p[0] - px).powi(2) + (p[1] - py).powi(2);
                        if d < min2 {
                            min2 = d;
                        }
                    }
                    for p in &pts_b {
                        let d = (p[0] - px).powi(2) + (p[1] - py).powi(2);
                        if d < min2 {
                            min2 = d;
                        }
                    }
                    if min2 < stroke2 {
                        bits |= BRAILLE_BITS[dy][dx];
                    }
                }
            }
            let ch = char::from_u32(0x2800 + bits).unwrap_or(' ');
            row.push(ch);
        }
        lines.push(row);
    }
    lines
}

// ─────────────────────────── Layout ───────────────────────────

const LOGO_W: usize = 22;
const LOGO_H: usize = 9; // braille: 9 cells × 4 sub-rows = 36 sub-rows; ring max-y at R=14 is ~15.4, fits with margin
const LOGO_R: f32 = 14.0;
const LOGO_STROKE: f32 = 1.15;

/// Build the centered welcome screen lines. `elapsed_secs` drives the y-axis spin
/// for the logo; the tilt is constant (the logo "starts in the right position"
/// and slowly rotates around y).
pub fn welcome_lines(
    width: u16,
    _branch: &str,
    _cwd: &str,
    version: &str,
    elapsed_secs: f32,
    selected_idx: usize,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let width = width as usize;

    // (top branch+cwd bar removed — metadata will live in the input border instead)
    out.push(Line::from(""));

    // logo — constant tilt, slow y-axis drift
    let rx = 0.55_f32;
    let ry = elapsed_secs * 0.4;
    let logo_rows = render_braille_logo(LOGO_W, LOGO_H, LOGO_R, LOGO_STROKE, rx, ry);
    let pad_logo = " ".repeat(width.saturating_sub(LOGO_W) / 2);
    for row in logo_rows {
        let mut text = String::with_capacity(pad_logo.len() + row.len());
        text.push_str(&pad_logo);
        text.push_str(&row);
        out.push(Line::from(Span::styled(text, text_style())));
    }
    out.push(Line::from(""));
    out.push(Line::from(""));

    // menu — 3 rows, centered
    let menu_w: usize = 38;
    let pad_menu = " ".repeat(width.saturating_sub(menu_w) / 2);
    let items: [(&str, &str); 3] = [
        ("New worktree", "ctrl-w"),
        ("Resume session", "ctrl-s"),
        ("Quit", "ctrl-q"),
    ];
    for (i, (label, kbd)) in items.iter().enumerate() {
        let gap = menu_w.saturating_sub(label.len() + kbd.len());
        let label_style = if i == selected_idx { bold() } else { text_style() };
        out.push(Line::from(vec![
            Span::raw(pad_menu.clone()),
            Span::styled(label.to_string(), label_style),
            Span::raw(" ".repeat(gap)),
            Span::styled(kbd.to_string(), muted()),
        ]));
    }
    out.push(Line::from(""));
    out.push(Line::from(""));

    // breathing room above the version footer
    out.push(Line::from(""));
    out.push(Line::from(""));

    // version (right-aligned within width)
    let v_text = format!("{} Beta", version);
    let v_pad = " ".repeat(width.saturating_sub(v_text.len() + 2));
    out.push(Line::from(vec![
        Span::raw(v_pad),
        Span::styled(v_text, muted()),
    ]));

    out
}
