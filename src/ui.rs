//! Rendering and chrome: the colour palette, the embedded backdrop image, the
//! low-level framebuffer drawing primitives, the Win2k-style widgets (sidebar,
//! inspector, context menu, buttons, fields) and the chrome geometry/hit-test
//! helpers shared by draw and click handling.

use std::collections::HashSet;

use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

use winit::window::{CursorIcon, ResizeDirection};

use crate::{
    CtxMenu, FONT_PX, Field, FontStyle, Listener, NodeId, Renderer, Row, TextCmd, Tree, font_style,
};

// Dark theme: white text over the teal/green backdrop image, black chrome.
pub(crate) const FG: u32 = 0xea_ea_ea; // terminal default text (white)
// `BG` doubles as the "default background" sentinel — cells with this bg are
// left unfilled so the backdrop image shows through. Its value only shows if a
// cell is *explicitly* filled with the default bg, so it's a dark teal that
// blends with the image.
pub(crate) const BG: u32 = 0x0b_1d_1d;
// Ctrl+hover link affordance: the hovered URL's cells get a bright blue
// underline so it reads as clickable.
pub(crate) const LINK: u32 = 0x5c_9c_ff;
pub(crate) const SEL: u32 = 0x2f_5d_6e; // selection fill (white text stays readable)
pub(crate) const HOVER: u32 = 0x1c_1c_1c; // sidebar row hover fill (a hair above STRIP_BG; text stays readable)

// --- Win2k chrome ----------------------------------------------------------
// Windows 2000 design *principles* (not its colours): explicit borders and
// bevels, a subtle vertical gradient, compact fixed heights, dense layout,
// left-aligned titles.
// The sidebar auto-sizes to its content: the longest label plus this fixed
// right margin (see `State::sidebar_w`). It is never narrower than `WBTN_W`. A
// toggle (⌘B / Ctrl+Shift+B) hides it entirely (width 0).
pub(crate) const SIDEBAR_MARGIN: usize = 16; // fixed gap right of the longest label
pub(crate) const SIDEBAR_PAD_L: usize = 6; // small left inset before each tree label
pub(crate) const HEADER_H: usize = 16; // title bar; one content row tall (= cell_h at FONT_PX) for uniform heights
pub(crate) const ROW_H: usize = 20; // context-menu item height
pub(crate) const CTX_W: usize = 150; // context-menu width (fits "Search Google")
pub(crate) const RPANEL_W: usize = 252; // right inspector pane width
pub(crate) const WBTN_W: usize = 30; // minimum sidebar width (also the old info-button width)
pub(crate) const TLIGHT_CELL: usize = 18; // per-dot hit cell for the window controls
pub(crate) const TLIGHT_R: f32 = 5.0; // traffic-light dot radius (px); diameter 10 in a 16px row
pub(crate) const EDGE: f64 = 9.0; // borderless-window resize-grip thickness (edges)
pub(crate) const CORNER: f64 = 22.0; // larger square grab zone at each window corner

// Scrollbar: a wide, draggable bar in its own gutter at the right of the
// terminal viewport — always present, separate from the text. Sizes are logical
// (1×); the Retina pass scales them.
pub(crate) const SBAR_GUTTER: usize = 14; // reserved column the bar lives in (content stops before it)
pub(crate) const SBAR_W: usize = 8; // thumb/track width, centred in the gutter
pub(crate) const SBAR_MIN: usize = 24; // minimum thumb height so it stays grabbable

// macOS-style "traffic light" window controls (bitmap dots, not glyphs).
pub(crate) const TLIGHT_MIN: u32 = 0xfe_bc_2e; // minimize — amber
pub(crate) const TLIGHT_MAX: u32 = 0x28_c8_40; // maximize / restore — green
pub(crate) const TLIGHT_CLOSE: u32 = 0xff_5f_57; // close — red

// Black chrome with white text. The Win2k bevel structure is kept but recolored
// to dark grays so panels read as raised/inset without any light surfaces.
pub(crate) const STRIP_BG: u32 = 0x0a_0a_0a; // chrome background (near-black)
pub(crate) const PANEL_HI: u32 = 0x33_33_33; // top of a raised gradient (selected row/button)
pub(crate) const PANEL_LO: u32 = 0x1f_1f_1f; // bottom of a raised gradient
pub(crate) const HEAD_HI: u32 = 0x16_16_16; // header gradient top
pub(crate) const HEAD_LO: u32 = 0x08_08_08; // header gradient bottom
pub(crate) const BEVEL_LT: u32 = 0x3a_3a_3a; // raised highlight (top/left)
pub(crate) const BEVEL_DK: u32 = 0x00_00_00; // raised shadow (bottom/right)
pub(crate) const INK: u32 = 0xf0_f0_f0; // primary chrome text (white)
pub(crate) const INK_DIM: u32 = 0xa0_a0_a0; // secondary chrome text (gray)

/// Map a terminal color to RGB. A compact, good-enough palette.
pub(crate) fn rgb(color: Color, default: u32) -> u32 {
    match color {
        Color::Spec(c) => pack(c.r, c.g, c.b),
        Color::Named(n) => named_rgb(n).unwrap_or(default),
        Color::Indexed(i) => indexed_rgb(i),
    }
}

pub(crate) fn pack(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// Resolve a color *index* (as carried by `alacritty_terminal`'s
/// `Event::ColorRequest`) to its actual RGB, so OSC 10/11/4 queries can be
/// answered. Indices 0..=255 are the palette; 256/257/258 are the special
/// foreground/background/cursor colors (see `vte::ansi::NamedColor`). Reporting
/// our real dark `BG` is what lets apps (e.g. Claude Code) detect a dark
/// terminal and emit light, readable text over the backdrop image.
pub(crate) fn osc_color(index: usize) -> Rgb {
    let packed = match index {
        0..=255 => indexed_rgb(index as u8),
        256 => FG, // NamedColor::Foreground
        257 => BG, // NamedColor::Background
        258 => FG, // NamedColor::Cursor (drawn in the default ink)
        _ => FG,
    };
    Rgb { r: (packed >> 16) as u8, g: (packed >> 8) as u8, b: packed as u8 }
}

// ANSI palette tuned for the **dark** backdrop: bright, saturated hues that pop
// against the teal/green image, with white text as the default foreground.
pub(crate) fn named_rgb(n: NamedColor) -> Option<u32> {
    Some(match n {
        NamedColor::Black => 0x2b2b2b,
        NamedColor::Red => 0xff6d67,
        NamedColor::Green => 0x8ae234,
        NamedColor::Yellow => 0xfce94f,
        NamedColor::Blue => 0x78b6ff,
        NamedColor::Magenta => 0xe48bff,
        NamedColor::Cyan => 0x54e1d6,
        NamedColor::White => 0xeaeaea,
        NamedColor::BrightBlack => 0x6a6a6a,
        NamedColor::BrightRed => 0xff8b86,
        NamedColor::BrightGreen => 0xadf85f,
        NamedColor::BrightYellow => 0xfff27a,
        NamedColor::BrightBlue => 0x9ccbff,
        NamedColor::BrightMagenta => 0xf0a8ff,
        NamedColor::BrightCyan => 0x7defe4,
        NamedColor::BrightWhite => 0xffffff,
        NamedColor::Foreground | NamedColor::BrightForeground => FG,
        NamedColor::Background => BG,
        NamedColor::Cursor => FG,
        _ => return None,
    })
}

/// Standard xterm 256-color cube + grayscale ramp.
pub(crate) fn indexed_rgb(i: u8) -> u32 {
    match i {
        0..=15 => named_rgb(ANSI16[i as usize]).unwrap_or(FG),
        16..=231 => {
            let i = i - 16;
            let levels = [0u8, 95, 135, 175, 215, 255];
            let r = levels[(i / 36) as usize];
            let g = levels[((i / 6) % 6) as usize];
            let b = levels[(i % 6) as usize];
            pack(r, g, b)
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            pack(v, v, v)
        }
    }
}

pub(crate) const ANSI16: [NamedColor; 16] = [
    NamedColor::Black,
    NamedColor::Red,
    NamedColor::Green,
    NamedColor::Yellow,
    NamedColor::Blue,
    NamedColor::Magenta,
    NamedColor::Cyan,
    NamedColor::White,
    NamedColor::BrightBlack,
    NamedColor::BrightRed,
    NamedColor::BrightGreen,
    NamedColor::BrightYellow,
    NamedColor::BrightBlue,
    NamedColor::BrightMagenta,
    NamedColor::BrightCyan,
    NamedColor::BrightWhite,
];

// --- backdrop image --------------------------------------------------------
// The terminal area is drawn over the user's profile background image (the same
// teal/green gradient as their iTerm). It's embedded so it travels with the
// binary, decoded once, and pre-darkened so white text and the bright ANSI
// palette stay readable over it (this is iTerm's "background blend").

/// Embedded background image bytes (PNG).
pub(crate) const BACKDROP_PNG: &[u8] = include_bytes!("../assets/profile-default-bg.png");

/// How far to blend the image toward black, 0..=255 (readability dimming).
pub(crate) const BACKDROP_DIM: u32 = 96;

/// A decoded, pre-darkened RGB image: `px[y * w + x]` packed `0x00RRGGBB`.
pub(crate) struct Backdrop {
    w: usize,
    h: usize,
    px: Vec<u32>,
}

/// Decode the embedded image once (lazily) and cache it for the process. Decode
/// failure degrades to a 1×1 dark tile, so the app still runs (just a flat dark
/// terminal background) rather than panicking.
pub(crate) fn backdrop() -> &'static Backdrop {
    static BACKDROP: std::sync::OnceLock<Backdrop> = std::sync::OnceLock::new();
    BACKDROP.get_or_init(|| {
        match image::load_from_memory(BACKDROP_PNG) {
            Ok(img) => {
                let rgb = img.to_rgb8();
                let (w, h) = (rgb.width() as usize, rgb.height() as usize);
                let px = rgb
                    .pixels()
                    .map(|p| darken(pack(p[0], p[1], p[2]), BACKDROP_DIM))
                    .collect();
                Backdrop { w, h, px }
            }
            Err(_) => Backdrop {
                w: 1,
                h: 1,
                px: vec![darken(BG, BACKDROP_DIM)],
            },
        }
    })
}

/// Fast integer blend of `color` toward black by `k`/255 (no gamma — this is a
/// bulk fill, not glyph anti-aliasing). `k = 0` keeps the color, `255` = black.
pub(crate) fn darken(color: u32, k: u32) -> u32 {
    let f = 255 - k;
    let r = (((color >> 16) & 0xff) * f) / 255;
    let g = (((color >> 8) & 0xff) * f) / 255;
    let b = ((color & 0xff) * f) / 255;
    r << 16 | g << 8 | b
}

/// Paint the backdrop image, stretched to cover the rect `(x, y, w, h)`, into
/// `buf`. Used wherever the terminal background is laid down (the logical
/// compose and the crisp Retina overdraw) so the image shows under every cell
/// that doesn't set its own background. Stretching a smooth gradient is
/// visually lossless, so no aspect-correct cropping is needed.
pub(crate) fn fill_backdrop(buf: &mut [u32], bw: usize, bh: usize, x: usize, y: usize, w: usize, h: usize) {
    let img = backdrop();
    if w == 0 || h == 0 || img.w == 0 || img.h == 0 {
        return;
    }
    for ry in 0..h {
        let py = y + ry;
        if py >= bh {
            break;
        }
        let sy = (ry * img.h / h).min(img.h - 1);
        let srow = sy * img.w;
        let drow = py * bw;
        for rx in 0..w {
            let px = x + rx;
            if px >= bw {
                break;
            }
            let sx = (rx * img.w / w).min(img.w - 1);
            buf[drow + px] = img.px[srow + sx];
        }
    }
}

// --- drawing helpers (unchanged) -------------------------------------------

pub(crate) fn fill_rect(buf: &mut [u32], pw: usize, ph: usize, x: usize, y: usize, w: usize, h: usize, color: u32) {
    for yy in y..(y + h).min(ph) {
        for xx in x..(x + w).min(pw) {
            buf[yy * pw + xx] = color;
        }
    }
}

/// Vertical gradient fill: row `y` lerps from `top` to `bot`.
pub(crate) fn vgradient(buf: &mut [u32], pw: usize, ph: usize, x: usize, y: usize, w: usize, h: usize, top: u32, bot: u32) {
    for row in 0..h {
        let yy = y + row;
        if yy >= ph {
            break;
        }
        let t = if h > 1 { (row * 255 / (h - 1)) as u32 } else { 0 };
        let color = blend(bot, top, t);
        for col in 0..w {
            let xx = x + col;
            if xx >= pw {
                break;
            }
            buf[yy * pw + xx] = color;
        }
    }
}

/// A filled, anti-aliased disc centred at (`cx`,`cy`) with radius `r`, blended
/// over whatever is already in `buf`. A 1px-soft edge keeps the small chrome
/// dots from looking jagged at logical resolution.
pub(crate) fn fill_circle(buf: &mut [u32], pw: usize, ph: usize, cx: f32, cy: f32, r: f32, color: u32) {
    let x0 = (cx - r - 1.0).floor().max(0.0) as usize;
    let y0 = (cy - r - 1.0).floor().max(0.0) as usize;
    let x1 = ((cx + r + 1.0).ceil().max(0.0) as usize).min(pw);
    let y1 = ((cy + r + 1.0).ceil().max(0.0) as usize).min(ph);
    for yy in y0..y1 {
        for xx in x0..x1 {
            let dx = xx as f32 + 0.5 - cx;
            let dy = yy as f32 + 0.5 - cy;
            let d = (dx * dx + dy * dy).sqrt();
            // Full coverage inside, fading linearly to 0 over the outer 1px.
            let cov = (r + 0.5 - d).clamp(0.0, 1.0);
            if cov <= 0.0 {
                continue;
            }
            let idx = yy * pw + xx;
            buf[idx] = blend(color, buf[idx], (cov * 255.0 + 0.5) as u32);
        }
    }
}

/// The dark symbol revealed inside a traffic-light dot on hover.
#[derive(Clone, Copy)]
pub(crate) enum TlGlyph {
    Minus, // minimize
    Plus,  // maximize / restore
    Cross, // close
}

/// Distance from point (`px`,`py`) to the segment (`ax`,`ay`)–(`bx`,`by`).
pub(crate) fn seg_dist(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 {
        (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (cx, cy) = (ax + t * dx, ay + t * dy);
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

/// Stamp the dark macOS-style glyph inside a traffic-light dot centred at
/// (`cx`,`cy`) with radius `r`. Strokes are anti-aliased line segments blended
/// over the dot, so the symbol reads as an inset cut-out.
pub(crate) fn draw_tlight_glyph(buf: &mut [u32], pw: usize, ph: usize, cx: f32, cy: f32, r: f32, g: TlGlyph) {
    let e = r * 0.5; // arm half-length
    let segs: &[(f32, f32, f32, f32)] = match g {
        TlGlyph::Minus => &[(-1.0, 0.0, 1.0, 0.0)],
        TlGlyph::Plus => &[(-1.0, 0.0, 1.0, 0.0), (0.0, -1.0, 0.0, 1.0)],
        TlGlyph::Cross => &[(-1.0, -1.0, 1.0, 1.0), (-1.0, 1.0, 1.0, -1.0)],
    };
    let half = 0.7; // stroke half-width (px)
    let x0 = (cx - r).floor().max(0.0) as usize;
    let y0 = (cy - r).floor().max(0.0) as usize;
    let x1 = ((cx + r).ceil().max(0.0) as usize + 1).min(pw);
    let y1 = ((cy + r).ceil().max(0.0) as usize + 1).min(ph);
    for yy in y0..y1 {
        for xx in x0..x1 {
            let (fx, fy) = (xx as f32 + 0.5, yy as f32 + 0.5);
            let d = segs
                .iter()
                .map(|&(ax, ay, bx, by)| {
                    seg_dist(fx, fy, cx + ax * e, cy + ay * e, cx + bx * e, cy + by * e)
                })
                .fold(f32::MAX, f32::min);
            let cov = (half + 0.5 - d).clamp(0.0, 1.0);
            if cov <= 0.0 {
                continue;
            }
            let idx = yy * pw + xx;
            // Dark, semi-opaque cut-out (a darkened tint of the dot beneath).
            buf[idx] = blend(0x00_00_00, buf[idx], (cov * 200.0 + 0.5) as u32);
        }
    }
}

/// 1px rectangle outline.
pub(crate) fn stroke_rect(buf: &mut [u32], pw: usize, ph: usize, x: usize, y: usize, w: usize, h: usize, color: u32) {
    if w == 0 || h == 0 {
        return;
    }
    fill_rect(buf, pw, ph, x, y, w, 1, color);
    fill_rect(buf, pw, ph, x, y + h - 1, w, 1, color);
    fill_rect(buf, pw, ph, x, y, 1, h, color);
    fill_rect(buf, pw, ph, x + w - 1, y, 1, h, color);
}

/// Draw a left-aligned monospace string, clipped to `max_w` pixels. Chrome
/// text is always the Regular face at the logical size. When the renderer's
/// `text_log` is armed (the GUI's Retina path), the string is *recorded* in
/// logical coordinates and replayed crisply later by [`render_text_cmds`]
/// instead of being rasterized here.
pub(crate) fn draw_text(
    buf: &mut [u32],
    pw: usize,
    ph: usize,
    r: &mut Renderer,
    x: usize,
    y: usize,
    max_w: usize,
    text: &str,
    color: u32,
) {
    if let Some(log) = &mut r.text_log {
        log.push(TextCmd {
            x,
            y,
            max_w,
            text: text.to_string(),
            color,
        });
        return;
    }
    let cw = r.cell_w;
    let mut pen = x;
    for c in text.chars() {
        if pen + cw > x + max_w {
            break;
        }
        if c != ' ' {
            let g = r.glyph(c, FontStyle::Regular, FONT_PX);
            for gy in 0..g.h {
                let py = y as i32 + g.top + gy as i32;
                if py < 0 || py as usize >= ph {
                    continue;
                }
                for gx in 0..g.w {
                    let px = pen as i32 + g.left + gx as i32;
                    if px < 0 || px as usize >= pw {
                        continue;
                    }
                    let a = g.bitmap[gy * g.w + gx] as u32;
                    if a == 0 {
                        continue;
                    }
                    let idx = py as usize * pw + px as usize;
                    buf[idx] = blend(color, buf[idx], a);
                }
            }
        }
        pen += cw;
    }
}

/// Replay captured chrome-text commands onto `buf` at device resolution: each
/// logical command is re-laid at `(sx, sy)` with glyphs rasterized at
/// `FONT_PX × sy`, so chrome text comes out as crisp as the terminal grid on a
/// Retina display (rather than being nearest-neighbour-doubled and chunky).
pub(crate) fn render_text_cmds(buf: &mut [u32], bw: usize, bh: usize, r: &mut Renderer, cmds: Vec<TextCmd>, sx: f64, sy: f64) {
    let font_px = FONT_PX * sy as f32;
    let cell_w = ((r.cell_w as f64 * sx).round() as usize).max(1);
    for cmd in cmds {
        let x0 = (cmd.x as f64 * sx).round() as usize;
        let y0 = (cmd.y as f64 * sy).round() as usize;
        let max_w = (cmd.max_w as f64 * sx).round() as usize;
        let mut pen = x0;
        for c in cmd.text.chars() {
            if pen + cell_w > x0 + max_w {
                break;
            }
            if c != ' ' {
                let g = r.glyph(c, FontStyle::Regular, font_px);
                for gy in 0..g.h {
                    let py = y0 as i32 + g.top + gy as i32;
                    if py < 0 || py as usize >= bh {
                        continue;
                    }
                    for gx in 0..g.w {
                        let px = pen as i32 + g.left + gx as i32;
                        if px < 0 || px as usize >= bw {
                            continue;
                        }
                        let a = g.bitmap[gy * g.w + gx] as u32;
                        if a == 0 {
                            continue;
                        }
                        let idx = py as usize * bw + px as usize;
                        buf[idx] = blend(cmd.color, buf[idx], a);
                    }
                }
            }
            pen += cell_w;
        }
    }
}

/// Render the visible grid of one terminal into `buf` at an arbitrary scale.
/// All geometry is in *target* pixels, so the same routine serves the logical
/// 1× compose (`paint`) and the crisp device-resolution Retina pass (`redraw`):
/// pass cell/origin sizes already multiplied by the device scale and `font_px =
/// FONT_PX × scale`. Honours per-cell colour, the cursor block, selection,
/// bold/italic faces, dim, underline/strikeout and Unicode-9 wide cells.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_terminal_cells(
    buf: &mut [u32],
    bw: usize,
    bh: usize,
    r: &mut Renderer,
    term: &Term<Listener>,
    lines: i32,
    origin_x: usize,
    origin_y: usize,
    clip_right: usize,
    cell_w: usize,
    cell_h: usize,
    font_px: f32,
    links: &HashSet<(i32, usize)>,
) {
    // `display_iter` numbers scrollback with negative grid lines; the visible
    // viewport is shifted by the scroll offset, so a grid line maps to on-screen
    // row `line + display_offset`.
    let offset = term.grid().display_offset() as i32;
    let content = term.renderable_content();
    let cursor = content.cursor.point;
    let selection = content.selection;
    for cell in content.display_iter {
        let row = cell.point.line.0 + offset;
        if row < 0 || row >= lines {
            continue;
        }
        // The trailing half of a wide (double-width) char carries no glyph of
        // its own — the lead cell's glyph spans into it.
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let col = cell.point.column.0;
        let x0 = origin_x + col * cell_w;
        let y0 = origin_y + row as usize * cell_h;
        let wide = cell.flags.contains(Flags::WIDE_CHAR);
        let span = if wide { cell_w * 2 } else { cell_w };

        let is_cursor = cell.point.line == cursor.line && cell.point.column == cursor.column;
        let selected = selection.is_some_and(|s| s.contains(cell.point));
        let mut fg = rgb(cell.fg, FG);
        let mut bg = rgb(cell.bg, BG);
        if cell.flags.contains(Flags::DIM) {
            fg = blend(fg, bg, 150); // dim = pull the ink toward its background
        }
        if is_cursor {
            std::mem::swap(&mut fg, &mut bg);
        } else if selected {
            bg = SEL;
        }
        if bg != BG {
            fill_rect(buf, bw, bh, x0, y0, span.min(clip_right.saturating_sub(x0)), cell_h, bg);
        }

        let c = cell.c;
        let drawable = c != ' ' && c != '\0' && !cell.flags.contains(Flags::HIDDEN);
        if drawable {
            let g = r.glyph(c, font_style(cell.flags), font_px);
            for gy in 0..g.h {
                let py = y0 as i32 + g.top + gy as i32;
                if py < 0 || py as usize >= bh {
                    continue;
                }
                for gx in 0..g.w {
                    let px = x0 as i32 + g.left + gx as i32;
                    if px < origin_x as i32 || px as usize >= clip_right {
                        continue;
                    }
                    let a = g.bitmap[gy * g.w + gx] as u32;
                    if a == 0 {
                        continue;
                    }
                    let idx = py as usize * bw + px as usize;
                    buf[idx] = blend(fg, buf[idx], a);
                }
            }
        }

        // Decorations sit on the baseline-ish; thickness scales with the cell.
        let thick = (cell_h / 14).max(1);
        if cell.flags.contains(Flags::UNDERLINE) {
            let uy = y0 + cell_h.saturating_sub(thick + 1);
            hline(buf, bw, bh, x0, uy, span, clip_right, thick, fg);
        }
        if cell.flags.contains(Flags::STRIKEOUT) {
            let sy = y0 + cell_h / 2;
            hline(buf, bw, bh, x0, sy, span, clip_right, thick, fg);
        }
        // Ctrl+hover link: underline the URL's cells in the link colour so the
        // pointer's blue underline tracks the link under the cursor.
        if !links.is_empty() && links.contains(&(cell.point.line.0, cell.point.column.0)) {
            let uy = y0 + cell_h.saturating_sub(thick + 1);
            hline(buf, bw, bh, x0, uy, span, clip_right, thick, LINK);
        }
    }
}

/// A clipped horizontal rule `thick` px tall (terminal underline/strikeout).
pub(crate) fn hline(
    buf: &mut [u32],
    bw: usize,
    bh: usize,
    x: usize,
    y: usize,
    w: usize,
    clip_right: usize,
    thick: usize,
    color: u32,
) {
    let right = (x + w).min(clip_right).min(bw);
    for yy in y..(y + thick).min(bh) {
        for xx in x..right {
            buf[yy * bw + xx] = color;
        }
    }
}

/// Fill a rect by alpha-blending `color` over what's already in `buf` (clipped).
/// Used for the translucent scrollbar so the backdrop reads through it.
pub(crate) fn blend_rect(buf: &mut [u32], pw: usize, ph: usize, x: usize, y: usize, w: usize, h: usize, color: u32, a: u32) {
    for yy in y..(y + h).min(ph) {
        for xx in x..(x + w).min(pw) {
            let idx = yy * pw + xx;
            buf[idx] = blend(color, buf[idx], a);
        }
    }
}

/// The thumb's top-y and height inside a track of height `area_h` starting at
/// `area_y`, for a grid scrolled `offset` lines up out of `history`, showing
/// `screen` lines. Single source of truth for [`draw_scrollbar`] and the
/// drag/click hit-testing in `lib.rs`, so the picture and the grab can't
/// disagree. With no scrollback the thumb fills the whole track.
pub(crate) fn scrollbar_thumb(area_y: usize, area_h: usize, offset: usize, history: usize, screen: usize, min_thumb: usize) -> (usize, usize) {
    let total = (history + screen).max(1);
    let thumb_h = (area_h * screen / total).max(min_thumb).min(area_h);
    let span = area_h - thumb_h;
    // `offset == history` is the top (fully scrolled back), `offset == 0` the
    // bottom (live tail).
    let top = if history == 0 { area_y } else { area_y + span * (history - offset) / history };
    (top, thumb_h)
}

/// The scrollbar in its gutter at the right of the terminal viewport: an
/// always-present faint track with a brighter, draggable thumb sized and
/// positioned from the grid's scrollback. `rect` is the track column in *target*
/// pixels (like [`draw_terminal_cells`]) so the same routine serves the logical
/// compose and the crisp Retina overdraw; pass `min_thumb` already scaled.
/// `active` brightens the thumb while it's being dragged.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_scrollbar(
    buf: &mut [u32],
    bw: usize,
    bh: usize,
    rect: Rect,
    offset: usize,
    history: usize,
    screen: usize,
    min_thumb: usize,
    active: bool,
) {
    let (x, y, w, h) = rect;
    if w == 0 || h == 0 {
        return;
    }
    // Track: always drawn so the gutter reads as "the scrollbar lives here".
    blend_rect(buf, bw, bh, x, y, w, h, 0xff_ff_ff, 14);
    let (top, thumb_h) = scrollbar_thumb(y, h, offset, history, screen, min_thumb);
    // Dim and full-height when there's nothing to scroll; brighter when there
    // is, brightest while dragging.
    let a = if history == 0 { 28 } else if active { 150 } else { 92 };
    blend_rect(buf, bw, bh, x, top, w, thumb_h, 0xff_ff_ff, a);
}

/// The left tree pane, rendered as plain monospace text on the terminal's own
/// cell grid: one row per visible node at the terminal line height, indented by
/// depth with no markers, and a filled background for the selected row.
pub(crate) fn draw_sidebar(
    buf: &mut [u32],
    pw: usize,
    ph: usize,
    r: &mut Renderer,
    rows: &[Row],
    selected: NodeId,
    hovered: Option<NodeId>,
    sidebar_w: usize,
) {
    fill_rect(buf, pw, ph, 0, 0, sidebar_w, ph, STRIP_BG);
    // Title-bar band, shared with the terminal header so the top strip reads as
    // one continuous bar.
    vgradient(buf, pw, ph, 0, 0, sidebar_w, HEADER_H, HEAD_HI, HEAD_LO);
    fill_rect(buf, pw, ph, 0, 0, sidebar_w, 1, BEVEL_LT);
    fill_rect(buf, pw, ph, 0, HEADER_H - 1, sidebar_w, 1, BEVEL_DK);
    // 1px divider between the pane and the terminal.
    fill_rect(buf, pw, ph, sidebar_w - 1, 0, 1, ph, BEVEL_DK);

    // The tree is just monospace text on the terminal's own cell grid: one row
    // per node at the terminal line height, columns on the cell width, drawn
    // flush-left with no bevels, boxes or padding. The list begins at HEADER_H,
    // so a sidebar row lines up pixel-for-pixel with the terminal row beside it.
    // Selection is a filled background, exactly how the terminal highlights its
    // own selected cells.
    let rh = r.cell_h;
    let tops = sidebar_row_tops(rows, rh);
    for (i, row) in rows.iter().enumerate() {
        let y = HEADER_H + tops[i];
        if y + rh > ph {
            break;
        }
        let is_sel = row.id == selected;
        if is_sel {
            // The selection band still spans the full pane; only the label text
            // is inset, so the highlight stays edge-to-edge.
            fill_rect(buf, pw, ph, 0, y, sidebar_w - 1, rh, SEL);
        } else if hovered == Some(row.id) {
            // A faint lightening of the unselected row under the pointer — just
            // enough to read as "this is the target", not enough to compete with
            // the selection band. Same edge-to-edge span as the selection fill.
            fill_rect(buf, pw, ph, 0, y, sidebar_w - 1, rh, HOVER);
        }
        // No markers: hierarchy reads from colour alone — top-level rows
        // (sections, and the standalone "Edit Config" session) full-ink, nested
        // sessions dim. Labels carry a small left inset (`SIDEBAR_PAD_L`) so the
        // text doesn't sit flush against the pane edge.
        let color = if is_sel || row.depth == 0 { INK } else { INK_DIM };
        draw_text(
            buf,
            pw,
            ph,
            r,
            SIDEBAR_PAD_L,
            y,
            sidebar_w.saturating_sub(SIDEBAR_PAD_L),
            &row.name,
            color,
        );
    }
}

/// Top y of each sidebar row, relative to `HEADER_H`. A one-row blank spacer
/// precedes every top-level row except the first, so sections (and the
/// standalone "Edit Config" session) read as separated blocks. Shared by
/// [`draw_sidebar`] and `sidebar_hit` so the picture on screen and the click map
/// can never disagree.
pub(crate) fn sidebar_row_tops(rows: &[Row], rh: usize) -> Vec<usize> {
    let mut tops = Vec::with_capacity(rows.len());
    let mut y = 0;
    for (i, row) in rows.iter().enumerate() {
        if i > 0 && row.depth == 0 {
            y += rh; // blank line between top-level blocks
        }
        tops.push(y);
        y += rh;
    }
    tops
}

/// A small bevelled popup at the cursor, one row per item (e.g. Start/Stop,
/// Copy/Paste/Search, or Open/Copy/Paste). `hovered` is the item the pointer is
/// currently over, drawn with a highlight bar so it behaves like a normal menu.
pub(crate) fn draw_ctx_menu(
    buf: &mut [u32],
    pw: usize,
    ph: usize,
    r: &mut Renderer,
    x: usize,
    y: usize,
    items: &[&str],
    hovered: Option<usize>,
) {
    let n = items.len().max(1);
    let h = ROW_H * n + 2;
    vgradient(buf, pw, ph, x, y, CTX_W, h, PANEL_HI, PANEL_LO);
    stroke_rect(buf, pw, ph, x, y, CTX_W, h, BEVEL_DK);
    fill_rect(buf, pw, ph, x, y, CTX_W, 1, BEVEL_LT);
    fill_rect(buf, pw, ph, x, y, 1, h, BEVEL_LT);
    let ty = ROW_H.saturating_sub(r.cell_h) / 2;
    for (i, label) in items.iter().enumerate() {
        let ry = y + 1 + i * ROW_H;
        if hovered == Some(i) {
            fill_rect(buf, pw, ph, x + 1, ry, CTX_W - 2, ROW_H, SEL);
        }
        draw_text(buf, pw, ph, r, x + 10, ry + ty, CTX_W - 12, label, INK);
    }
    // Dividers between items (drawn after, so the hover bar sits under them).
    for i in 1..items.len() {
        fill_rect(buf, pw, ph, x + 4, y + i * ROW_H, CTX_W - 8, 1, BEVEL_DK);
    }
}

/// Which context-menu item (if any) the point falls on, or `None` when outside
/// the menu (which dismisses it).
pub(crate) fn ctx_item_at(m: &CtxMenu, x: f64, y: f64) -> Option<usize> {
    let n = m.items.len().max(1);
    let (mx, my) = (m.x as f64, m.y as f64);
    let h = (ROW_H * n + 2) as f64;
    if x < mx || x >= mx + CTX_W as f64 || y < my || y >= my + h {
        return None;
    }
    Some((((y - my) / ROW_H as f64) as usize).min(n - 1))
}

/// A Win2k push-button: raised by default, sunken+inset when `pressed`, dim
/// when `!enabled`. Used for the window controls and the use-cwd action.
pub(crate) fn draw_button(
    buf: &mut [u32],
    pw: usize,
    ph: usize,
    r: &mut Renderer,
    rect: Rect,
    label: &str,
    pressed: bool,
    enabled: bool,
) {
    let (x, y, w, h) = rect;
    if pressed {
        fill_rect(buf, pw, ph, x, y, w, h, PANEL_LO);
        fill_rect(buf, pw, ph, x, y, w, 1, BEVEL_DK);
        fill_rect(buf, pw, ph, x, y, 1, h, BEVEL_DK);
        fill_rect(buf, pw, ph, x, y + h - 1, w, 1, BEVEL_LT);
        fill_rect(buf, pw, ph, x + w - 1, y, 1, h, BEVEL_LT);
    } else {
        vgradient(buf, pw, ph, x, y, w, h, PANEL_HI, PANEL_LO);
        stroke_rect(buf, pw, ph, x, y, w, h, BEVEL_DK);
        fill_rect(buf, pw, ph, x, y, w, 1, BEVEL_LT);
        fill_rect(buf, pw, ph, x, y, 1, h, BEVEL_LT);
    }
    let tw = label.chars().count() * r.cell_w;
    let off = pressed as usize;
    let tx = x + w.saturating_sub(tw) / 2 + off;
    let ty = y + h.saturating_sub(r.cell_h) / 2 + off;
    draw_text(buf, pw, ph, r, tx, ty, w, label, if enabled { INK } else { INK_DIM });
}

/// A sunken Win2k text box. `focused` draws the caret; `enabled` is false for
/// the command field on a group (no command to edit).
pub(crate) fn draw_field(
    buf: &mut [u32],
    pw: usize,
    ph: usize,
    r: &mut Renderer,
    rect: Rect,
    text: &str,
    focused: bool,
    caret: usize,
    enabled: bool,
) {
    let (x, y, w, h) = rect;
    fill_rect(buf, pw, ph, x, y, w, h, if enabled { BG } else { STRIP_BG });
    // Inset bevel: shadow on top/left, highlight on bottom/right.
    fill_rect(buf, pw, ph, x, y, w, 1, BEVEL_DK);
    fill_rect(buf, pw, ph, x, y, 1, h, BEVEL_DK);
    fill_rect(buf, pw, ph, x, y + h - 1, w, 1, BEVEL_LT);
    fill_rect(buf, pw, ph, x + w - 1, y, 1, h, BEVEL_LT);
    let tx = x + 5;
    let ty = y + h.saturating_sub(r.cell_h) / 2;
    draw_text(
        buf,
        pw,
        ph,
        r,
        tx,
        ty,
        w.saturating_sub(10),
        text,
        if enabled { INK } else { INK_DIM },
    );
    if focused {
        let cx = tx + caret.min(text.chars().count()) * r.cell_w;
        fill_rect(buf, pw, ph, cx, y + 3, 1, h.saturating_sub(6), INK);
    }
}

/// The right inspector pane: a recessed panel echoing the sidebar, with the
/// selected node's path and editable Title / Default-command / Working-dir
/// fields, plus a "use current working dir" button.
pub(crate) fn draw_inspector(
    buf: &mut [u32],
    pw: usize,
    ph: usize,
    r: &mut Renderer,
    tree: &Tree,
    sel: NodeId,
    focus: Option<Field>,
    caret: usize,
    can_use_cwd: bool,
) {
    let px = panel_x(pw);
    fill_rect(buf, pw, ph, px, 0, RPANEL_W, ph, STRIP_BG);
    fill_rect(buf, pw, ph, px, 0, 1, ph, BEVEL_DK); // hard divider
    vgradient(buf, pw, ph, px, 0, RPANEL_W, HEADER_H, HEAD_HI, HEAD_LO);
    fill_rect(buf, pw, ph, px, 0, RPANEL_W, 1, BEVEL_LT);
    fill_rect(buf, pw, ph, px, HEADER_H - 1, RPANEL_W, 1, BEVEL_DK);
    let hty = HEADER_H.saturating_sub(r.cell_h) / 2;
    draw_text(buf, pw, ph, r, px + 10, hty, RPANEL_W - 20, "PROPERTIES", INK);
    draw_text(
        buf,
        pw,
        ph,
        r,
        px + 12,
        HEADER_H + 8,
        RPANEL_W - 24,
        &tree.path(sel),
        INK_DIM,
    );

    let cell_h = r.cell_h;
    let rects = field_rects(pw, cell_h);
    let lab_y = |by: usize| by.saturating_sub(cell_h + 4);
    for f in Field::ALL {
        let rect = rects[f.index()];
        let label = match f {
            Field::Title => "Title",
            Field::Command => "Default command",
            Field::Dir => "Working directory",
        };
        draw_text(buf, pw, ph, r, rect.0, lab_y(rect.1), RPANEL_W, label, INK);
        match tree.field_text(sel, f) {
            Some(t) => draw_field(buf, pw, ph, r, rect, &t, focus == Some(f), caret, true),
            None => {
                let dis = match f {
                    Field::Command => "(group \u{2014} no command)",
                    Field::Dir => "(group \u{2014} no directory)",
                    Field::Title => "",
                };
                draw_field(buf, pw, ph, r, rect, dis, false, 0, false);
            }
        }
    }
    draw_button(
        buf,
        pw,
        ph,
        r,
        usecwd_btn(pw, cell_h),
        "Use current working dir",
        false,
        can_use_cwd,
    );
}

// --- chrome geometry -------------------------------------------------------
// One source of truth for every clickable chrome rect, shared by draw and
// hit-test (the same discipline as `Tree::rows`): a pixel can't be drawn one
// place and clicked another.

pub(crate) type Rect = (usize, usize, usize, usize); // x, y, w, h

pub(crate) fn hit(r: Rect, x: f64, y: f64) -> bool {
    x >= r.0 as f64 && x < (r.0 + r.2) as f64 && y >= r.1 as f64 && y < (r.1 + r.3) as f64
}

/// Left edge of the right inspector pane.
pub(crate) fn panel_x(pw: usize) -> usize {
    pw.saturating_sub(RPANEL_W)
}

/// Right edge of the live terminal area (shrinks when the inspector is open).
pub(crate) fn term_right(pw: usize, inspector: bool) -> usize {
    if inspector { panel_x(pw) } else { pw }
}

/// Right edge of the terminal *content* (the cell grid): the viewport minus the
/// scrollbar gutter, so the bar always has its own column separate from text.
pub(crate) fn term_content_right(pw: usize, inspector: bool) -> usize {
    term_right(pw, inspector).saturating_sub(SBAR_GUTTER)
}

/// The scrollbar's track rect (full terminal height, in its gutter), in logical
/// pixels. Shared by draw and the click/drag hit-test. `x` is the thumb's left
/// edge, centred within the gutter.
pub(crate) fn scrollbar_rect(pw: usize, ph: usize, inspector: bool) -> Rect {
    let gx = term_right(pw, inspector).saturating_sub(SBAR_GUTTER);
    let x = gx + SBAR_GUTTER.saturating_sub(SBAR_W) / 2;
    (x, HEADER_H, SBAR_W, ph.saturating_sub(HEADER_H))
}

/// `[minimize, maximize, close]` traffic-light hit cells, full header height and
/// `TLIGHT_CELL` wide, flush to the window's top-right with a small edge gap.
pub(crate) fn win_btns(pw: usize) -> [Rect; 3] {
    let pad = 4; // gap from the right edge
    let right = pw.saturating_sub(pad);
    let w = TLIGHT_CELL;
    [
        (right.saturating_sub(3 * w), 0, w, HEADER_H), // minimize (leftmost)
        (right.saturating_sub(2 * w), 0, w, HEADER_H), // maximize
        (right.saturating_sub(w), 0, w, HEADER_H),     // close (rightmost)
    ]
}

/// `[title, command, directory]` boxes inside the inspector pane, in
/// `Field::ALL` order (so `Field::index` is the row index here).
pub(crate) fn field_rects(pw: usize, cell_h: usize) -> [Rect; 3] {
    let px = panel_x(pw);
    let pad = 12;
    let fx = px + pad;
    let fw = RPANEL_W - pad * 2;
    let bh = cell_h + 8;
    // Each box sits exactly `cell_h + 4` below its label (see `lab_y`), and
    // the first label clears the path line under the PROPERTIES head.
    let lab0 = HEADER_H + 8 + cell_h + 10;
    let y0 = lab0 + cell_h + 4;
    let step = bh + 16 + cell_h + 4; // box -> next label -> next box
    [
        (fx, y0, fw, bh),
        (fx, y0 + step, fw, bh),
        (fx, y0 + 2 * step, fw, bh),
    ]
}

/// The "Use current working dir" button, just below the directory box.
pub(crate) fn usecwd_btn(pw: usize, cell_h: usize) -> Rect {
    let (x, y, w, h) = field_rects(pw, cell_h)[Field::Dir.index()];
    (x, y + h + 8, w, h)
}

/// Which resize grip (if any) the point is in, for a borderless window. Side and
/// bottom edges are a thin `EDGE` strip; the two *bottom* corners use a much
/// larger `CORNER` square so the diagonal grips are easy to hit. The **top has
/// no resize at all** — it's the title bar (drag + window controls), so there's
/// no North / NorthWest / NorthEast grip to fight dragging.
pub(crate) fn resize_dir(pw: usize, ph: usize, x: f64, y: f64) -> Option<ResizeDirection> {
    let (w, h) = (pw as f64, ph as f64);
    let (l, r, b) = (x < EDGE, x >= w - EDGE, y >= h - EDGE);
    // Enlarged squares at the two bottom corners only.
    let (cl, cr) = (x < CORNER, x >= w - CORNER);
    let cb = y >= h - CORNER;
    Some(match () {
        _ if cb && cl => ResizeDirection::SouthWest,
        _ if cb && cr => ResizeDirection::SouthEast,
        _ if b => ResizeDirection::South,
        _ if l => ResizeDirection::West,
        _ if r => ResizeDirection::East,
        _ => return None,
    })
}

/// The OS cursor that signals each resize direction, so the pointer turns into
/// the matching double-headed arrow when it enters a window edge or corner.
pub(crate) fn resize_cursor(dir: ResizeDirection) -> CursorIcon {
    match dir {
        ResizeDirection::North => CursorIcon::NResize,
        ResizeDirection::South => CursorIcon::SResize,
        ResizeDirection::East => CursorIcon::EResize,
        ResizeDirection::West => CursorIcon::WResize,
        ResizeDirection::NorthEast => CursorIcon::NeResize,
        ResizeDirection::NorthWest => CursorIcon::NwResize,
        ResizeDirection::SouthEast => CursorIcon::SeResize,
        ResizeDirection::SouthWest => CursorIcon::SwResize,
    }
}

// --- gamma-correct alpha blending (unchanged) ------------------------------

/// sRGB(0..=255) -> linear-light(0..=1) lookup table. Blending glyph coverage
/// in linear light (not raw sRGB) is what makes anti-aliased text crisp and
/// correctly weighted instead of muddy.
pub(crate) fn srgb_lut() -> &'static [f32; 256] {
    static LUT: std::sync::OnceLock<[f32; 256]> = std::sync::OnceLock::new();
    LUT.get_or_init(|| {
        let mut t = [0.0f32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            let c = i as f32 / 255.0;
            *v = if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            };
        }
        t
    })
}

/// linear-light(0..=1) -> sRGB(0..=255), rounded.
pub(crate) fn lin_to_srgb(v: f32) -> u32 {
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0 + 0.5) as u32
}

/// Alpha-blend `fg` over `bg` with coverage `a` (0..=255), gamma-correct.
pub(crate) fn blend(fg: u32, bg: u32, a: u32) -> u32 {
    let lut = srgb_lut();
    let t = a as f32 / 255.0;
    let mix = |shift: u32| -> u32 {
        let s = lut[((fg >> shift) & 0xff) as usize];
        let d = lut[((bg >> shift) & 0xff) as usize];
        lin_to_srgb(d + (s - d) * t)
    };
    mix(16) << 16 | mix(8) << 8 | mix(0)
}

