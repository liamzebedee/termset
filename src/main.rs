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

/// Userland event: the PTY parser thread woke us up because the grid changed.
#[derive(Debug, Clone)]
enum UserEvent {
    Wakeup,
    Exit,
}

/// `EventListener` impl handed to `alacritty_terminal`. It just forwards the
/// few events we care about onto the winit event loop so the GUI thread can
/// react (redraw / quit). It must be `Clone + Send` because the PTY runs on
/// its own thread.
#[derive(Clone)]
struct Listener(EventLoopProxy<UserEvent>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        match event {
            TermEvent::Wakeup => {
                let _ = self.0.send_event(UserEvent::Wakeup);
            }
            TermEvent::Exit | TermEvent::ChildExit(_) => {
                let _ = self.0.send_event(UserEvent::Exit);
            }
            _ => {}
        }
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
    font: fontdue::Font,
    cell_w: usize,
    cell_h: usize,
    ascent: f32,
    cache: HashMap<char, Glyph>,
}

impl Renderer {
    fn new() -> Self {
        let bytes = load_font();
        let font =
            fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).expect("parse font");

        let lm = font
            .horizontal_line_metrics(FONT_PX)
            .expect("font line metrics");
        let cell_h = lm.new_line_size.ceil() as usize;
        // Monospace: every cell is the advance width of a representative glyph.
        let cell_w = font.metrics('M', FONT_PX).advance_width.ceil() as usize;

        Self {
            font,
            cell_w: cell_w.max(1),
            cell_h: cell_h.max(1),
            ascent: lm.ascent,
            cache: HashMap::new(),
        }
    }

    fn glyph(&mut self, c: char) -> &Glyph {
        let ascent = self.ascent;
        let font = &self.font;
        self.cache.entry(c).or_insert_with(|| {
            let (m, bitmap) = font.rasterize(c, FONT_PX);
            // fontdue bitmap is top-down; ymin is the offset of the bitmap
            // bottom below the baseline.
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

/// Look for a monospace TTF: ask fontconfig, then fall back to DejaVu.
fn load_font() -> Vec<u8> {
    if let Ok(out) = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", "monospace"])
        .output()
    {
        let path = String::from_utf8_lossy(&out.stdout);
        let path = path.trim();
        if !path.is_empty() {
            if let Ok(b) = std::fs::read(path) {
                return b;
            }
        }
    }
    std::fs::read("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf")
        .expect("no monospace font found (install fontconfig or DejaVu Sans Mono)")
}

const FG: u32 = 0xCC_CC_CC;
const BG: u32 = 0x10_10_14;
const SEL: u32 = 0x3a_3d_4d;

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

/// Everything that only exists once the window is created.
struct State {
    window: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    renderer: Renderer,
    term: Arc<FairMutex<Term<Listener>>>,
    pty_tx: EventLoopSender,
    size: TermSize,
    clipboard: Option<arboard::Clipboard>,
    mouse: (f64, f64),
    selecting: bool,
    mods: ModifiersState,
}

impl State {
    /// Convert a pixel position in the window to a grid `Point` (accounting
    /// for scrollback via the display offset) plus which half of the cell.
    fn pixel_to_point(&self, term: &Term<Listener>) -> (Point, Side) {
        let cw = self.renderer.cell_w as f64;
        let ch = self.renderer.cell_h as f64;
        let col = ((self.mouse.0 / cw) as usize).min(self.size.cols.saturating_sub(1));
        let row = ((self.mouse.1 / ch) as usize).min(self.size.lines.saturating_sub(1));
        let offset = term.grid().display_offset() as i32;
        let line = Line(row as i32 - offset);
        let frac = (self.mouse.0 / cw).fract();
        let side = if frac < 0.5 { Side::Left } else { Side::Right };
        (Point::new(line, Column(col)), side)
    }

    fn copy_selection(&mut self) {
        let text = self.term.lock().selection_to_string();
        if let (Some(text), Some(cb)) = (text, self.clipboard.as_mut()) {
            if !text.is_empty() {
                let _ = cb.set_text(text);
            }
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

        let term = st.term.lock();
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
            let y0 = line as usize * ch;

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
        buf.present().unwrap();
    }

    fn send(&self, bytes: Vec<u8>) {
        if let Some(st) = &self.state {
            let _ = st.pty_tx.send(Msg::Input(bytes.into()));
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

/// Alpha-blend `fg` over `bg` with coverage `a` (0..=255).
fn blend(fg: u32, bg: u32, a: u32) -> u32 {
    let mix = |s: u32, d: u32| ((s * a + d * (255 - a)) / 255) & 0xff;
    let r = mix((fg >> 16) & 0xff, (bg >> 16) & 0xff);
    let g = mix((fg >> 8) & 0xff, (bg >> 8) & 0xff);
    let b = mix(fg & 0xff, bg & 0xff);
    r << 16 | g << 8 | b
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
            lines: (inner.height as usize / renderer.cell_h).max(1),
        };
        let window_size = WindowSize {
            num_cols: size.cols as u16,
            num_lines: size.lines as u16,
            cell_width: renderer.cell_w as u16,
            cell_height: renderer.cell_h as u16,
        };

        let listener = Listener(self.proxy.clone());
        let term = Term::new(Config::default(), &size, listener.clone());
        let term = Arc::new(FairMutex::new(term));

        let pty = tty::new(&PtyOptions::default(), window_size, 0).expect("spawn pty");
        let pty_loop = PtyEventLoop::new(term.clone(), listener, pty, false, false)
            .expect("create pty event loop");
        let pty_tx = pty_loop.channel();
        pty_loop.spawn();

        self.state = Some(State {
            window,
            surface,
            renderer,
            term,
            pty_tx,
            size,
            clipboard: arboard::Clipboard::new().ok(),
            mouse: (0.0, 0.0),
            selecting: false,
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
            UserEvent::Exit => event_loop.exit(),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(new) => {
                if let Some(st) = self.state.as_mut() {
                    let cols = (new.width as usize / st.renderer.cell_w).max(1);
                    let lines = (new.height as usize / st.renderer.cell_h).max(1);
                    st.size = TermSize { cols, lines };
                    st.term.lock().resize(st.size);
                    let _ = st.pty_tx.send(Msg::Resize(WindowSize {
                        num_cols: cols as u16,
                        num_lines: lines as u16,
                        cell_width: st.renderer.cell_w as u16,
                        cell_height: st.renderer.cell_h as u16,
                    }));
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
                        let mut term = st.term.lock();
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
                            let mut term = st.term.lock();
                            let (point, side) = st.pixel_to_point(&term);
                            term.selection =
                                Some(Selection::new(SelectionType::Simple, point, side));
                            drop(term);
                            st.selecting = true;
                            st.window.request_redraw();
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
                if let Some(st) = &self.state {
                    if st.mods.control_key() && st.mods.shift_key() {
                        match &event.logical_key {
                            Key::Character(c) if c.eq_ignore_ascii_case("c") => {
                                self.copy_to_clipboard();
                                return;
                            }
                            Key::Character(c) if c.eq_ignore_ascii_case("v") => {
                                self.paste();
                                return;
                            }
                            _ => {}
                        }
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
