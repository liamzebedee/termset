//! termem — a single-file mini terminal emulator.
//!
//! Backend : `alacritty_terminal` (PTY + VT/ANSI state machine + parser thread)
//! Frontend: `winit` (window + keyboard) + `softbuffer` (CPU framebuffer)
//!           + `fontdue` (glyph rasterization)
//!
//! Run with: `cargo run --release`

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop as PtyEventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Options as PtyOptions};
use alacritty_terminal::vte::ansi::{Color, NamedColor};

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

const FONT_PX: f32 = 16.0;

/// Userland event from a PTY parser thread. Each event carries the id of the
/// tab it originated from so the GUI thread can route it to the right session.
#[derive(Debug, Clone)]
enum UserEvent {
    Wakeup,
    Exit(u64),
    Title(u64, String),
    ResetTitle(u64),
}

/// `EventListener` impl handed to `alacritty_terminal`. It forwards the few
/// events we care about onto the winit event loop so the GUI thread can react
/// (redraw / retitle / close tab). It must be `Clone + Send` because the PTY
/// runs on its own thread. `id` ties events back to the owning tab.
#[derive(Clone)]
struct Listener {
    proxy: EventLoopProxy<UserEvent>,
    id: u64,
}

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let _ = match event {
            TermEvent::Wakeup => self.proxy.send_event(UserEvent::Wakeup),
            TermEvent::Exit | TermEvent::ChildExit(_) => {
                self.proxy.send_event(UserEvent::Exit(self.id))
            }
            TermEvent::Title(t) => self.proxy.send_event(UserEvent::Title(self.id, t)),
            TermEvent::ResetTitle => self.proxy.send_event(UserEvent::ResetTitle(self.id)),
            _ => Ok(()),
        };
    }
}

/// Grid geometry. `alacritty_terminal` needs this to size the terminal and the
/// PTY (`Dimensions` for `Term`, `WindowSize` for the kernel PTY ioctl).
#[derive(Clone, Copy)]
struct TermSize {
    cols: usize,
    lines: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// A rasterized glyph: coverage bitmap plus placement metrics.
struct Glyph {
    w: usize,
    h: usize,
    left: i32,
    top: i32, // pixels from cell top to glyph top
    bitmap: Vec<u8>,
}

struct Renderer {
    /// `fonts[0]` is the primary monospace face (defines cell metrics); the
    /// rest are fallbacks consulted, in order, for glyphs it lacks.
    fonts: Vec<fontdue::Font>,
    cell_w: usize,
    cell_h: usize,
    ascent: f32,
    cache: HashMap<char, Glyph>,
}

impl Renderer {
    fn new() -> Self {
        let fonts: Vec<fontdue::Font> = load_fonts()
            .into_iter()
            .filter_map(|b| fontdue::Font::from_bytes(b, fontdue::FontSettings::default()).ok())
            .collect();
        assert!(!fonts.is_empty(), "no usable font found");
        let primary = &fonts[0];

        let lm = primary
            .horizontal_line_metrics(FONT_PX)
            .expect("font line metrics");
        let cell_h = lm.new_line_size.ceil() as usize;
        // Monospace: every cell is the advance width of a representative glyph.
        let cell_w = primary.metrics('M', FONT_PX).advance_width.ceil() as usize;

        Self {
            fonts,
            cell_w: cell_w.max(1),
            cell_h: cell_h.max(1),
            ascent: lm.ascent,
            cache: HashMap::new(),
        }
    }

    fn glyph(&mut self, c: char) -> &Glyph {
        let ascent = self.ascent;
        let fonts = &self.fonts;
        self.cache.entry(c).or_insert_with(|| {
            // Pick the first font that actually has this glyph; fall back to
            // the primary (renders .notdef) if none do.
            let font = fonts
                .iter()
                .find(|f| f.lookup_glyph_index(c) != 0)
                .unwrap_or(&fonts[0]);
            let (m, bitmap) = font.rasterize(c, FONT_PX);
            // fontdue bitmap is top-down; ymin is the offset of the bitmap
            // bottom below the baseline. Using the primary face's ascent for
            // every font keeps baselines aligned across faces.
            let top = (ascent - (m.height as f32 + m.ymin as f32)).round() as i32;
            Glyph {
                w: m.width,
                h: m.height,
                left: m.xmin,
                top,
                bitmap,
            }
        })
    }
}

/// Ask fontconfig for a primary monospace face plus a set of fallback faces
/// that cover symbols/emoji the monospace font is missing. De-dups by path
/// and always tries DejaVu as a last resort.
fn load_fonts() -> Vec<Vec<u8>> {
    // fontconfig patterns, in priority order. The first must be monospace
    // (it defines the cell grid); the rest are coverage fallbacks.
    let patterns = [
        "monospace",
        "Noto Sans Symbols2",
        "Symbola",
        "Noto Sans Symbols",
        "DejaVu Sans",
        "Noto Color Emoji",
    ];
    let mut paths: Vec<String> = Vec::new();
    for pat in patterns {
        if let Ok(out) = std::process::Command::new("fc-match")
            .args(["-f", "%{file}", pat])
            .output()
        {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() && !paths.contains(&p) {
                paths.push(p);
            }
        }
    }
    let mut fonts: Vec<Vec<u8>> = paths
        .iter()
        .filter_map(|p| std::fs::read(p).ok())
        .collect();
    if fonts.is_empty() {
        fonts.push(
            std::fs::read("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf")
                .expect("no monospace font found (install fontconfig or DejaVu Sans Mono)"),
        );
    }
    fonts
}

// Light mode by default.
const FG: u32 = 0x1a_1a_1a;
const BG: u32 = 0xff_ff_ff;
const SEL: u32 = 0xbf_d9_f2;

// --- Tab bar chrome ---------------------------------------------------------
// Windows 2000 design *principles* (not its colours): explicit borders and
// bevels, a subtle vertical gradient, compact fixed height, dense layout,
// left-aligned titles.
const TAB_BAR_H: usize = 26; // fixed height, in pixels
const TAB_W: usize = 168; // fixed per-tab width
const TAB_PAD_X: usize = 8; // text inset from the tab's left edge

const STRIP_BG: u32 = 0xc0_c0_c0; // tab-bar background behind the tabs
const TAB_HI: u32 = 0xff_ff_ff; // top of an active tab's gradient
const TAB_LO: u32 = 0xe4_e4_e4; // bottom of an active tab's gradient
const TAB_INACT_HI: u32 = 0xd9_d9_d9; // top of an inactive tab's gradient
const TAB_INACT_LO: u32 = 0xbe_be_be; // bottom of an inactive tab's gradient
const BEVEL_LT: u32 = 0xff_ff_ff; // raised highlight (top/left)
const BEVEL_DK: u32 = 0x80_80_80; // raised shadow (bottom/right)
const TAB_FG: u32 = 0x1a_1a_1a; // active tab title
const TAB_FG_DIM: u32 = 0x5a_5a_5a; // inactive tab title

/// Map a terminal color to RGB. A compact, good-enough palette.
fn rgb(color: Color, default: u32) -> u32 {
    match color {
        Color::Spec(c) => pack(c.r, c.g, c.b),
        Color::Named(n) => named_rgb(n).unwrap_or(default),
        Color::Indexed(i) => indexed_rgb(i),
    }
}

fn pack(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

fn named_rgb(n: NamedColor) -> Option<u32> {
    Some(match n {
        NamedColor::Black => 0x1d1f21,
        NamedColor::Red => 0xcc6666,
        NamedColor::Green => 0xb5bd68,
        NamedColor::Yellow => 0xf0c674,
        NamedColor::Blue => 0x81a2be,
        NamedColor::Magenta => 0xb294bb,
        NamedColor::Cyan => 0x8abeb7,
        NamedColor::White => 0xc5c8c6,
        NamedColor::BrightBlack => 0x666666,
        NamedColor::BrightRed => 0xd54e53,
        NamedColor::BrightGreen => 0xb9ca4a,
        NamedColor::BrightYellow => 0xe7c547,
        NamedColor::BrightBlue => 0x7aa6da,
        NamedColor::BrightMagenta => 0xc397d8,
        NamedColor::BrightCyan => 0x70c0b1,
        NamedColor::BrightWhite => 0xeaeaea,
        NamedColor::Foreground | NamedColor::BrightForeground => FG,
        NamedColor::Background => BG,
        NamedColor::Cursor => FG,
        _ => return None,
    })
}

/// Standard xterm 256-color cube + grayscale ramp.
fn indexed_rgb(i: u8) -> u32 {
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

const ANSI16: [NamedColor; 16] = [
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

/// One terminal session: its own PTY thread + VT state machine + grid size.
struct Tab {
    id: u64,
    term: Arc<FairMutex<Term<Listener>>>,
    pty_tx: EventLoopSender,
    size: TermSize,
    title: String,
}

/// Spawn a fresh PTY-backed terminal session sized to the current window.
fn spawn_tab(
    proxy: &EventLoopProxy<UserEvent>,
    id: u64,
    label_n: u64,
    size: TermSize,
    cell_w: usize,
    cell_h: usize,
) -> Tab {
    let window_size = WindowSize {
        num_cols: size.cols as u16,
        num_lines: size.lines as u16,
        cell_width: cell_w as u16,
        cell_height: cell_h as u16,
    };
    let listener = Listener {
        proxy: proxy.clone(),
        id,
    };
    let term = Term::new(Config::default(), &size, listener.clone());
    let term = Arc::new(FairMutex::new(term));
    let pty = tty::new(&PtyOptions::default(), window_size, 0).expect("spawn pty");
    let pty_loop = PtyEventLoop::new(term.clone(), listener, pty, false, false)
        .expect("create pty event loop");
    let pty_tx = pty_loop.channel();
    pty_loop.spawn();
    Tab {
        id,
        term,
        pty_tx,
        size,
        title: format!("Terminal {label_n}"),
    }
}

/// Everything that only exists once the window is created. The terminal area
/// is offset down by `TAB_BAR_H` to make room for the tab strip.
struct State {
    window: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    renderer: Renderer,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    next_label: u64,
    clipboard: Option<arboard::Clipboard>,
    mouse: (f64, f64),
    selecting: bool,
    last_click: Option<(std::time::Instant, (f64, f64))>,
    mods: ModifiersState,
}

impl State {
    fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    /// Pixel rect of tab `i` in the strip: `(x0, width)`.
    fn tab_rect(i: usize) -> (usize, usize) {
        (i * TAB_W, TAB_W)
    }

    /// If `x,y` falls on a tab in the strip, return its index.
    fn tab_at(&self, x: f64, y: f64) -> Option<usize> {
        if y < 0.0 || y >= TAB_BAR_H as f64 {
            return None;
        }
        let i = (x as usize) / TAB_W;
        (i < self.tabs.len()).then_some(i)
    }

    /// Convert a pixel position in the window to a grid `Point` (accounting
    /// for the tab bar offset and scrollback display offset) plus cell half.
    fn pixel_to_point(&self, term: &Term<Listener>) -> (Point, Side) {
        let size = self.tab().size;
        let cw = self.renderer.cell_w as f64;
        let ch = self.renderer.cell_h as f64;
        let my = (self.mouse.1 - TAB_BAR_H as f64).max(0.0);
        let col = ((self.mouse.0 / cw) as usize).min(size.cols.saturating_sub(1));
        let row = ((my / ch) as usize).min(size.lines.saturating_sub(1));
        let offset = term.grid().display_offset() as i32;
        let line = Line(row as i32 - offset);
        let frac = (self.mouse.0 / cw).fract();
        let side = if frac < 0.5 { Side::Left } else { Side::Right };
        (Point::new(line, Column(col)), side)
    }

    fn copy_selection(&mut self) {
        let text = self.tabs[self.active].term.lock().selection_to_string();
        if let (Some(text), Some(cb)) = (text, self.clipboard.as_mut()) {
            if !text.is_empty() {
                let _ = cb.set_text(text);
            }
        }
    }

    /// Grid size for the current window, with the tab bar carved off the top.
    fn grid_size(&self, win_w: usize, win_h: usize) -> TermSize {
        TermSize {
            cols: (win_w / self.renderer.cell_w).max(1),
            lines: (win_h.saturating_sub(TAB_BAR_H) / self.renderer.cell_h).max(1),
        }
    }
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    state: Option<State>,
}

impl App {
    fn redraw(&mut self) {
        let Some(st) = self.state.as_mut() else { return };
        let win = st.window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(win.width), NonZeroU32::new(win.height)) else {
            return;
        };
        st.surface.resize(w, h).unwrap();
        let mut buf = st.surface.buffer_mut().unwrap();
        let (pw, ph) = (win.width as usize, win.height as usize);
        buf.fill(BG);

        let term = st.tabs[st.active].term.lock();
        let content = term.renderable_content();
        let cursor = content.cursor.point;
        let selection = content.selection;
        let cw = st.renderer.cell_w;
        let ch = st.renderer.cell_h;

        for cell in content.display_iter {
            let line = cell.point.line.0;
            if line < 0 {
                continue; // scrollback above viewport
            }
            let col = cell.point.column.0;
            let x0 = col * cw;
            let y0 = TAB_BAR_H + line as usize * ch;

            let is_cursor = cell.point.line == cursor.line && cell.point.column == cursor.column;
            let selected = selection.map_or(false, |s| s.contains(cell.point));
            let mut fg = rgb(cell.fg, FG);
            let mut bg = rgb(cell.bg, BG);
            if is_cursor {
                std::mem::swap(&mut fg, &mut bg);
            } else if selected {
                bg = SEL;
            }

            // Cell background.
            if bg != BG {
                fill_rect(&mut buf, pw, ph, x0, y0, cw, ch, bg);
            }

            let c = cell.c;
            if c == ' ' || c == '\0' {
                continue;
            }
            let g = st.renderer.glyph(c);
            for gy in 0..g.h {
                let py = y0 as i32 + g.top + gy as i32;
                if py < 0 || py as usize >= ph {
                    continue;
                }
                for gx in 0..g.w {
                    let px = x0 as i32 + g.left + gx as i32;
                    if px < 0 || px as usize >= pw {
                        continue;
                    }
                    let a = g.bitmap[gy * g.w + gx] as u32;
                    if a == 0 {
                        continue;
                    }
                    let idx = py as usize * pw + px as usize;
                    buf[idx] = blend(fg, buf[idx], a);
                }
            }
        }

        drop(term);

        draw_tab_bar(
            &mut buf,
            pw,
            ph,
            &mut st.renderer,
            &st.tabs,
            st.active,
        );

        buf.present().unwrap();
    }

    fn send(&self, bytes: Vec<u8>) {
        if let Some(st) = &self.state {
            let _ = st.tabs[st.active].pty_tx.send(Msg::Input(bytes.into()));
        }
    }

    /// Open a new tab next to the active one and focus it.
    fn new_tab(&mut self) {
        let Some(st) = self.state.as_mut() else { return };
        let win = st.window.inner_size();
        let size = st.grid_size(win.width as usize, win.height as usize);
        let id = st.next_id;
        let label = st.next_label;
        st.next_id += 1;
        st.next_label += 1;
        let tab = spawn_tab(
            &self.proxy,
            id,
            label,
            size,
            st.renderer.cell_w,
            st.renderer.cell_h,
        );
        let at = st.active + 1;
        st.tabs.insert(at, tab);
        st.active = at;
        st.window.request_redraw();
    }

    /// Close a tab by index. Shuts its PTY down. Returns `true` when that was
    /// the last tab and the caller should exit the app.
    fn close_tab(&mut self, idx: usize) -> bool {
        let Some(st) = self.state.as_mut() else {
            return false;
        };
        if idx >= st.tabs.len() {
            return false;
        }
        let tab = st.tabs.remove(idx);
        let _ = tab.pty_tx.send(Msg::Shutdown);
        if st.tabs.is_empty() {
            return true;
        }
        if st.active >= st.tabs.len() {
            st.active = st.tabs.len() - 1;
        } else if idx < st.active {
            st.active -= 1;
        }
        st.window.request_redraw();
        false
    }

    fn select_tab(&mut self, idx: usize) {
        if let Some(st) = self.state.as_mut() {
            if idx < st.tabs.len() && idx != st.active {
                st.active = idx;
                st.window.request_redraw();
            }
        }
    }

    /// Cycle the focused tab by `delta` (+1 next, -1 previous), wrapping.
    fn cycle_tab(&mut self, delta: isize) {
        if let Some(st) = self.state.as_mut() {
            let n = st.tabs.len() as isize;
            if n > 1 {
                st.active = (((st.active as isize + delta) % n + n) % n) as usize;
                st.window.request_redraw();
            }
        }
    }

    fn copy_to_clipboard(&mut self) {
        if let Some(st) = self.state.as_mut() {
            st.copy_selection();
        }
    }

    fn paste(&mut self) {
        let text = self
            .state
            .as_mut()
            .and_then(|st| st.clipboard.as_mut())
            .and_then(|cb| cb.get_text().ok());
        if let Some(text) = text {
            // Bracketed paste keeps shells from interpreting newlines as Enter
            // is left to the app; send raw, which is fine for a demo.
            self.send(text.replace('\n', "\r").into_bytes());
        }
    }
}

fn fill_rect(
    buf: &mut [u32],
    pw: usize,
    ph: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    color: u32,
) {
    for yy in y..(y + h).min(ph) {
        for xx in x..(x + w).min(pw) {
            buf[yy * pw + xx] = color;
        }
    }
}

/// Vertical gradient fill: row `y` lerps from `top` to `bot`.
fn vgradient(buf: &mut [u32], pw: usize, ph: usize, x: usize, y: usize, w: usize, h: usize, top: u32, bot: u32) {
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

/// 1px rectangle outline.
fn stroke_rect(buf: &mut [u32], pw: usize, ph: usize, x: usize, y: usize, w: usize, h: usize, color: u32) {
    if w == 0 || h == 0 {
        return;
    }
    fill_rect(buf, pw, ph, x, y, w, 1, color); // top
    fill_rect(buf, pw, ph, x, y + h - 1, w, 1, color); // bottom
    fill_rect(buf, pw, ph, x, y, 1, h, color); // left
    fill_rect(buf, pw, ph, x + w - 1, y, 1, h, color); // right
}

/// Draw a left-aligned monospace string, clipped to `max_w` pixels.
fn draw_text(
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
    let cw = r.cell_w;
    let mut pen = x;
    for c in text.chars() {
        if pen + cw > x + max_w {
            break;
        }
        if c != ' ' {
            let g = r.glyph(c);
            let (gw, gh, gl, gt) = (g.w, g.h, g.left, g.top);
            for gy in 0..gh {
                let py = y as i32 + gt + gy as i32;
                if py < 0 || py as usize >= ph {
                    continue;
                }
                for gx in 0..gw {
                    let px = pen as i32 + gl + gx as i32;
                    if px < 0 || px as usize >= pw {
                        continue;
                    }
                    let a = r.cache[&c].bitmap[gy * gw + gx] as u32;
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

/// Render the horizontal tab strip: a recessed background, then one bevelled,
/// gradient-filled tab per session with a left-aligned title. The active tab
/// reads "raised", inactive tabs sit flush and dimmed.
fn draw_tab_bar(
    buf: &mut [u32],
    pw: usize,
    ph: usize,
    r: &mut Renderer,
    tabs: &[Tab],
    active: usize,
) {
    // Recessed strip background + a hard bottom border under the whole bar.
    fill_rect(buf, pw, ph, 0, 0, pw, TAB_BAR_H, STRIP_BG);
    fill_rect(buf, pw, ph, 0, TAB_BAR_H - 1, pw, 1, BEVEL_DK);

    let text_y = (TAB_BAR_H.saturating_sub(r.cell_h)) / 2;
    for (i, tab) in tabs.iter().enumerate() {
        let (x0, w) = State::tab_rect(i);
        if x0 >= pw {
            break;
        }
        let is_active = i == active;
        let h = if is_active { TAB_BAR_H } else { TAB_BAR_H - 2 };
        let (hi, lo) = if is_active {
            (TAB_HI, TAB_LO)
        } else {
            (TAB_INACT_HI, TAB_INACT_LO)
        };
        vgradient(buf, pw, ph, x0, 0, w, h, hi, lo);
        // Bevel: light top/left, dark bottom/right (classic raised look).
        stroke_rect(buf, pw, ph, x0, 0, w, h, BEVEL_DK);
        fill_rect(buf, pw, ph, x0, 0, w, 1, BEVEL_LT);
        fill_rect(buf, pw, ph, x0, 0, 1, h, BEVEL_LT);

        let fg = if is_active { TAB_FG } else { TAB_FG_DIM };
        draw_text(
            buf,
            pw,
            ph,
            r,
            x0 + TAB_PAD_X,
            text_y,
            w.saturating_sub(TAB_PAD_X * 2),
            &tab.title,
            fg,
        );
    }
}

/// sRGB(0..=255) -> linear-light(0..=1) lookup table. Blending glyph coverage
/// in linear light (not raw sRGB) is what makes anti-aliased text crisp and
/// correctly weighted instead of muddy — the single biggest text-quality win
/// available with a CPU rasterizer.
fn srgb_lut() -> &'static [f32; 256] {
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
fn lin_to_srgb(v: f32) -> u32 {
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0 + 0.5) as u32
}

/// Alpha-blend `fg` over `bg` with coverage `a` (0..=255), gamma-correct.
fn blend(fg: u32, bg: u32, a: u32) -> u32 {
    let lut = srgb_lut();
    let t = a as f32 / 255.0;
    let mix = |shift: u32| -> u32 {
        let s = lut[((fg >> shift) & 0xff) as usize];
        let d = lut[((bg >> shift) & 0xff) as usize];
        lin_to_srgb(d + (s - d) * t)
    };
    mix(16) << 16 | mix(8) << 8 | mix(0)
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let window = Rc::new(
            event_loop
                .create_window(Window::default_attributes().with_title("termem"))
                .expect("create window"),
        );

        let renderer = Renderer::new();
        let ctx = softbuffer::Context::new(window.clone()).unwrap();
        let surface = softbuffer::Surface::new(&ctx, window.clone()).unwrap();

        let inner = window.inner_size();
        let size = TermSize {
            cols: (inner.width as usize / renderer.cell_w).max(1),
            lines: (inner.height as usize).saturating_sub(TAB_BAR_H) / renderer.cell_h.max(1),
        };
        let size = TermSize {
            lines: size.lines.max(1),
            ..size
        };

        let first = spawn_tab(
            &self.proxy,
            0,
            1,
            size,
            renderer.cell_w,
            renderer.cell_h,
        );

        self.state = Some(State {
            window,
            surface,
            renderer,
            tabs: vec![first],
            active: 0,
            next_id: 1,
            next_label: 2,
            clipboard: arboard::Clipboard::new().ok(),
            mouse: (0.0, 0.0),
            selecting: false,
            last_click: None,
            mods: ModifiersState::empty(),
        });
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Wakeup => {
                if let Some(st) = &self.state {
                    st.window.request_redraw();
                }
            }
            UserEvent::Exit(id) => {
                let idx = self
                    .state
                    .as_ref()
                    .and_then(|st| st.tabs.iter().position(|t| t.id == id));
                if let Some(idx) = idx {
                    if self.close_tab(idx) {
                        event_loop.exit();
                    }
                }
            }
            UserEvent::Title(id, title) => {
                if let Some(st) = self.state.as_mut() {
                    if let Some(t) = st.tabs.iter_mut().find(|t| t.id == id) {
                        t.title = title;
                        st.window.request_redraw();
                    }
                }
            }
            UserEvent::ResetTitle(id) => {
                if let Some(st) = self.state.as_mut() {
                    if let Some(t) = st.tabs.iter_mut().find(|t| t.id == id) {
                        t.title = format!("Terminal {}", t.id + 1);
                        st.window.request_redraw();
                    }
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(new) => {
                if let Some(st) = self.state.as_mut() {
                    let size = st.grid_size(new.width as usize, new.height as usize);
                    let ws = WindowSize {
                        num_cols: size.cols as u16,
                        num_lines: size.lines as u16,
                        cell_width: st.renderer.cell_w as u16,
                        cell_height: st.renderer.cell_h as u16,
                    };
                    // Every tab shares the window, so resize them all — not
                    // just the focused one — to keep background sessions sane.
                    for tab in &mut st.tabs {
                        tab.size = size;
                        tab.term.lock().resize(size);
                        let _ = tab.pty_tx.send(Msg::Resize(ws));
                    }
                    st.window.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(m) => {
                if let Some(st) = self.state.as_mut() {
                    st.mods = m.state();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(st) = self.state.as_mut() {
                    st.mouse = (position.x, position.y);
                    if st.selecting {
                        let mut term = st.tabs[st.active].term.lock();
                        let (point, side) = st.pixel_to_point(&term);
                        if let Some(sel) = term.selection.as_mut() {
                            sel.update(point, side);
                        }
                        drop(term);
                        st.window.request_redraw();
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(st) = self.state.as_mut() {
                    match (button, state) {
                        (MouseButton::Left, ElementState::Pressed) => {
                            // A click on the tab strip switches tabs; only
                            // clicks in the terminal area start a selection.
                            if let Some(i) = st.tab_at(st.mouse.0, st.mouse.1) {
                                self.select_tab(i);
                            } else if st.mouse.1 >= TAB_BAR_H as f64 {
                                // A second click in the same spot within
                                // 400ms is a double-click: select the word.
                                let now = std::time::Instant::now();
                                let double = st.last_click.is_some_and(|(t, p)| {
                                    now.duration_since(t).as_millis() < 400
                                        && (p.0 - st.mouse.0).abs() < 4.0
                                        && (p.1 - st.mouse.1).abs() < 4.0
                                });
                                st.last_click = Some((now, st.mouse));
                                let ty = if double {
                                    SelectionType::Semantic
                                } else {
                                    SelectionType::Simple
                                };
                                let mut term = st.tabs[st.active].term.lock();
                                let (point, side) = st.pixel_to_point(&term);
                                term.selection = Some(Selection::new(ty, point, side));
                                drop(term);
                                st.selecting = true;
                                st.window.request_redraw();
                            }
                        }
                        (MouseButton::Left, ElementState::Released) => {
                            st.selecting = false;
                            self.copy_to_clipboard();
                        }
                        (MouseButton::Middle, ElementState::Pressed) => {
                            self.paste();
                        }
                        _ => {}
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                let kmods = self.state.as_ref().map(|s| s.mods).unwrap_or_default();
                if kmods.control_key() {
                    // Ctrl+Tab / Ctrl+Shift+Tab cycle tabs (most-recent-style).
                    if let Key::Named(NamedKey::Tab) = &event.logical_key {
                        self.cycle_tab(if kmods.shift_key() { -1 } else { 1 });
                        return;
                    }
                }
                if kmods.control_key() && kmods.shift_key() {
                    match &event.logical_key {
                        Key::Character(c) if c.eq_ignore_ascii_case("c") => {
                            self.copy_to_clipboard();
                            return;
                        }
                        Key::Character(c) if c.eq_ignore_ascii_case("v") => {
                            self.paste();
                            return;
                        }
                        Key::Character(c) if c.eq_ignore_ascii_case("t") => {
                            self.new_tab();
                            return;
                        }
                        Key::Character(c) if c.eq_ignore_ascii_case("w") => {
                            let idx = self.state.as_ref().map(|s| s.active);
                            if let Some(idx) = idx {
                                if self.close_tab(idx) {
                                    event_loop.exit();
                                }
                            }
                            return;
                        }
                        Key::Character(c) => {
                            // Ctrl+Shift+<1..9> jumps straight to that tab.
                            if let Some(d) = c.chars().next().and_then(|d| d.to_digit(10)) {
                                if d >= 1 {
                                    self.select_tab(d as usize - 1);
                                    return;
                                }
                            }
                        }
                        Key::Named(NamedKey::PageUp) => {
                            self.cycle_tab(-1);
                            return;
                        }
                        Key::Named(NamedKey::PageDown) => {
                            self.cycle_tab(1);
                            return;
                        }
                        _ => {}
                    }
                }
                let mods = self.state.as_ref().map(|s| s.mods).unwrap_or_default();
                let (ctrl, alt, shift) =
                    (mods.control_key(), mods.alt_key(), mods.shift_key());
                // xterm modifier parameter: 1 + shift + 2*alt + 4*ctrl.
                let m = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
                // CSI sequence for a cursor/edit key, with modifier encoding.
                let csi = |tail: &str| -> Vec<u8> {
                    if m > 1 {
                        format!("\x1b[1;{m}{tail}").into_bytes()
                    } else {
                        format!("\x1b[{tail}").into_bytes()
                    }
                };
                // Tilde-style keys (Home/End/Delete/Page): \x1b[N~ or \x1b[N;m~.
                let tilde = |n: u8| -> Vec<u8> {
                    if m > 1 {
                        format!("\x1b[{n};{m}~").into_bytes()
                    } else {
                        format!("\x1b[{n}~").into_bytes()
                    }
                };
                let bytes: Vec<u8> = match &event.logical_key {
                    Key::Named(NamedKey::Enter) => vec![b'\r'],
                    Key::Named(NamedKey::Backspace) => {
                        if ctrl { b"\x17".to_vec() } else { vec![0x7f] }
                    }
                    Key::Named(NamedKey::Tab) => {
                        if shift { b"\x1b[Z".to_vec() } else { vec![b'\t'] }
                    }
                    Key::Named(NamedKey::Escape) => vec![0x1b],
                    Key::Named(NamedKey::ArrowUp) => csi("A"),
                    Key::Named(NamedKey::ArrowDown) => csi("B"),
                    Key::Named(NamedKey::ArrowRight) => csi("C"),
                    Key::Named(NamedKey::ArrowLeft) => csi("D"),
                    Key::Named(NamedKey::Home) => csi("H"),
                    Key::Named(NamedKey::End) => csi("F"),
                    Key::Named(NamedKey::Delete) => tilde(3),
                    Key::Named(NamedKey::PageUp) => tilde(5),
                    Key::Named(NamedKey::PageDown) => tilde(6),
                    Key::Named(NamedKey::Space) => {
                        if ctrl { vec![0] } else { vec![b' '] }
                    }
                    Key::Character(c) if ctrl => {
                        // Ctrl+letter -> C0 control byte (ctrl-a = 0x01, …).
                        // Covers ctrl-a/ctrl-e/ctrl-w/ctrl-u/ctrl-c, etc.
                        match c.chars().next() {
                            Some(ch) if ch.is_ascii() => {
                                let b = (ch as u8).to_ascii_uppercase();
                                let ctl = match b {
                                    b'@'..=b'_' => b & 0x1f,
                                    b' ' => 0,
                                    b'?' => 0x7f,
                                    _ => return,
                                };
                                if alt {
                                    vec![0x1b, ctl]
                                } else {
                                    vec![ctl]
                                }
                            }
                            _ => return,
                        }
                    }
                    _ => match event.text {
                        // Alt+<char> is sent as ESC-prefixed (Meta) for
                        // word-wise readline bindings like Alt+b / Alt+f.
                        Some(ref t) if alt => {
                            let mut v = vec![0x1b];
                            v.extend_from_slice(t.as_bytes());
                            v
                        }
                        Some(ref t) => t.as_bytes().to_vec(),
                        None => return,
                    },
                };
                self.send(bytes);
            }
            _ => {}
        }
    }
}

fn main() {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("build event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut app = App { proxy, state: None };
    event_loop.run_app(&mut app).expect("run");
}
