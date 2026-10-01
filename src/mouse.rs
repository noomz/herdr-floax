//! SGR mouse capture, in-box selection, and clipboard copy.
//!
//! Once the app enables SGR mouse capture on its terminal
//! (`?1000/?1002/?1006h`), the host terminal stops doing its own
//! full-canvas selection and instead delivers mouse events as byte
//! sequences on stdin. Without capture, that unowned full-canvas selection
//! is what floax used to get: a drag that started in the box kept selecting
//! across the border into the backdrop.
//!
//! This module pulls SGR mouse sequences out of the raw input stream so
//! every other byte still reaches the embedded PTY verbatim, then routes the
//! events:
//!
//! - embedded program asked for the mouse (tmux `mouse on`, vim
//!   `:set mouse=a`, …) → forward the event, coordinates re-based into the
//!   box interior;
//! - otherwise floax owns selection: a left drag inside the box selects the
//!   embedded screen's text and copies it to the clipboard on release.
//!   Drags outside the box are ignored, so the selection can never leave the
//!   box.

/// One SGR-mode mouse event (`ESC [ < b ; x ; y M|m`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SgrMouse {
    /// Button field: 0 left, 1 middle, 2 right, 3 "release", 64 wheel up,
    /// 65 wheel down (lower 5 bits before the motion bit).
    pub button: u16,
    /// Motion bit (32) set: this is a drag/motion event.
    pub motion: bool,
    /// `M` = press/drag, `m` = release.
    pub pressed: bool,
    /// 1-based column in the app's terminal.
    pub x: u16,
    /// 1-based row in the app's terminal.
    pub y: u16,
}

impl SgrMouse {
    /// Whether this event drives floax's own left-button selection: a left
    /// press, a left drag (button 32 = left + motion bit), or any release.
    /// Wheel events (bit 64) and buttonless motion (35) are excluded.
    pub fn is_left_select(&self) -> bool {
        (self.button & 0x40) == 0 && ((self.button & 0x03) == 0 || !self.pressed)
    }
}

const ESC: u8 = 0x1b;

/// Result of scanning a buffer for SGR mouse sequences.
enum Scan {
    /// Complete sequence: plain bytes before `plain`, sequence ends at `end`.
    Seq { plain: usize, end: usize, ev: SgrMouse },
    /// A mouse-sequence candidate starts at `idx` but the buffer ends
    /// mid-sequence; nothing from `idx` on may be passed through yet.
    Hold(usize),
    /// No candidate; bytes before `keep` are safe to pass through (a
    /// trailing lone `ESC` / `ESC[` is withheld — it may become `ESC[<…`).
    Pass(usize),
}

fn scan(buf: &[u8]) -> Scan {
    let mut i = 0;
    while i + 3 <= buf.len() {
        if buf[i] == ESC && buf[i + 1] == b'[' && buf[i + 2] == b'<' {
            if let Some((end, ev)) = parse_at(buf, i + 3) {
                return Scan::Seq { plain: i, end, ev };
            }
            // Either the buffer ends mid-sequence (keep holding) or the next
            // byte is not a digit (not a mouse sequence after all).
            if buf.get(i + 3).map_or(true, |b| b.is_ascii_digit()) {
                return Scan::Hold(i);
            }
            i += 3;
            continue;
        }
        i += 1;
    }
    // No candidate. Withhold a trailing `ESC[` / lone `ESC` so a keypress is
    // not confused with the start of a mouse sequence.
    let n = buf.len();
    let hold = if n >= 2 && buf[n - 2] == ESC && buf[n - 1] == b'[' {
        2
    } else if n >= 1 && buf[n - 1] == ESC {
        1
    } else {
        0
    };
    Scan::Pass(n - hold)
}

/// Parse an SGR mouse sequence starting at `buf[start]` (the byte after
/// `ESC[<`). Returns the end offset and the event when complete.
fn parse_at(buf: &[u8], start: usize) -> Option<(usize, SgrMouse)> {
    let mut j = start;
    let mut d = [0u16; 3];
    for k in 0..3 {
        if k > 0 {
            if j >= buf.len() || buf[j] != b';' {
                return None;
            }
            j += 1;
        }
        let digits = buf[j..]
            .iter()
            .take(4)
            .take_while(|b| b.is_ascii_digit())
            .count();
        if digits == 0 {
            return None;
        }
        let mut v: u16 = 0;
        for b in &buf[j..j + digits] {
            v = v.saturating_mul(10).saturating_add((b - b'0') as u16);
        }
        d[k] = v;
        j += digits;
    }
    let pressed = match buf.get(j)? {
        b'M' => true,
        b'm' => false,
        _ => return None,
    };
    Some((
        j + 1,
        SgrMouse {
            button: d[0],
            motion: d[0] & 32 != 0,
            pressed,
            x: d[1],
            y: d[2],
        },
    ))
}

/// Feed one stdin chunk into the pipeline: non-mouse bytes go to
/// `passthrough` (raw, to the PTY), complete mouse events go to `on_mouse`.
/// `carry` retains partial sequences across calls; the caller flushes it as
/// raw bytes when no continuation arrives.
pub fn process_input(
    carry: &mut Vec<u8>,
    chunk: &[u8],
    passthrough: &mut dyn FnMut(&[u8]),
    on_mouse: &mut dyn FnMut(SgrMouse),
) {
    carry.extend_from_slice(chunk);
    loop {
        match scan(carry) {
            Scan::Seq { plain, end, ev } => {
                let head: Vec<u8> = carry.drain(..end).collect();
                if plain > 0 {
                    passthrough(&head[..plain]);
                }
                on_mouse(ev);
            }
            Scan::Hold(idx) | Scan::Pass(idx) => {
                let rest: Vec<u8> = carry.drain(..idx).collect();
                if !rest.is_empty() {
                    passthrough(&rest);
                }
                return;
            }
        }
    }
}

/// Mouse modes the embedded program has switched on. vt100's parser does
/// not implement `?1000/?1002/?1003 h/l` (its `mouse_protocol_mode()` stays
/// `None` forever), so we track them from the PTY output stream.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MouseState {
    pub press: bool,  // ?1000
    pub drag: bool,   // ?1002
    pub any: bool,    // ?1003
    pub sgr: bool,    // ?1006 (SGR encoding)
    pub rxvt: bool,   // ?1015 (RXVT encoding)
}

impl MouseState {
    pub fn active(&self) -> bool {
        self.press || self.drag || self.any
    }
    /// The event encoding the embedded program will expect: SGR when `?1006`
    /// is on (tmux/vim default), RXVT when only `?1015` is on, SGR otherwise.
    pub fn sgr(&self) -> bool {
        self.sgr || !self.rxvt
    }
}

/// Scan a PTY output chunk for `CSI ? <modes> h|l` mouse-mode changes.
pub fn scan_mouse_modes(bytes: &[u8], state: &MouseState) -> MouseState {
    let mut s = *state;
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i] == ESC && bytes[i + 1] == b'[' && bytes[i + 2] == b'?' {
            let mut j = i + 3;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b';') {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'h' || bytes[j] == b'l') {
                let on = bytes[j] == b'h';
                let mut seg = &bytes[i + 3..j];
                while !seg.is_empty() {
                    let (p, rest) = match seg.splitn(2, |&b| b == b';').collect::<Vec<_>>() {
                        v if v.len() == 2 => (v[0], v[1]),
                        _ => (seg, &b""[..]),
                    };
                    if let Ok(text) = std::str::from_utf8(p) {
                        if let Ok(n) = text.parse::<u16>() {
                            match n {
                                1000 => s.press = on,
                                1002 => s.drag = on,
                                1003 => s.any = on,
                                1006 => s.sgr = on,
                                1015 => s.rxvt = on,
                                _ => {}
                            }
                        }
                    }
                    seg = rest;
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    s
}

/// Encode an event for forwarding to the embedded program. `x`/`y` are
/// 1-based coordinates in the embedded screen's own space.
pub fn encode_forward(m: &SgrMouse, x: u16, y: u16, sgr: bool) -> Vec<u8> {
    if sgr {
        format!("\x1b[<{};{};{}{}", m.button, x, y, if m.pressed { 'M' } else { 'm' })
            .into_bytes()
    } else {
        let b = m.button; // release arrives as 3 or the original button; keep as-is
        vec![
            ESC,
            b'[',
            b'M',
            (b as u8).saturating_add(32),
            (x as u8).saturating_add(32),
            (y as u8).saturating_add(32),
        ]
    }
}

/// A left-drag selection in the embedded screen's coordinates.
#[derive(Debug, Default, Clone, Copy)]
pub struct Selection {
    /// (col, row) of the anchor.
    pub anchor: (u16, u16),
    /// (col, row) of the current end.
    pub end: (u16, u16),
    pub active: bool,
}

impl Selection {
    pub fn reset(&mut self) {
        self.active = false;
    }

    /// Normalized span `(r0, r1, c_start_at_r0, c_end_at_r1)` (inclusive).
    /// Middle rows span the full width. `None` when inactive.
    pub fn span(&self, rows: u16, cols: u16) -> Option<(u16, u16, u16, u16)> {
        if !self.active || rows == 0 || cols == 0 {
            return None;
        }
        let a = (
            self.anchor.0.min(cols - 1),
            self.anchor.1.min(rows - 1),
        );
        let b = (self.end.0.min(cols - 1), self.end.1.min(rows - 1));
        if a.1 == b.1 {
            Some((a.1, a.1, a.0.min(b.0), a.0.max(b.0)))
        } else if a.1 < b.1 {
            Some((a.1, b.1, a.0, b.0))
        } else {
            Some((b.1, a.1, b.0, a.0))
        }
    }
}

/// Extract the selected text from the embedded screen. Rows are joined with
/// newlines; the start row begins at its start column, the end row ends at
/// its end column, and rows between span the full width (tmux behavior).
pub fn extract_text(screen: &vt100::Screen, sel: &Selection) -> String {
    let (rows, cols) = screen.size();
    let Some((r0, r1, c0, c1)) = sel.span(rows, cols) else {
        return String::new();
    };
    let mut out = String::new();
    for r in r0..=r1 {
        if r > r0 {
            out.push('\n');
        }
        let lo = if r == r0 { c0 } else { 0 };
        let hi = if r == r1 { c1 } else { cols - 1 };
        let mut skip_next = false;
        for c in 0..cols {
            if skip_next {
                skip_next = false;
                continue;
            }
            let Some(cell) = screen.cell(r, c) else {
                continue;
            };
            if c >= lo && c <= hi {
                let contents = cell.contents();
                out.push_str(if contents.is_empty() { " " } else { &contents });
            }
            if cell.is_wide() {
                skip_next = true;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(b: u16, x: u16, y: u16, pressed: bool) -> String {
        format!("\x1b[<{b};{x};{y}{}", if pressed { 'M' } else { 'm' })
    }

    fn run(chunks: &[&[u8]]) -> (Vec<u8>, Vec<SgrMouse>) {
        let mut carry = Vec::new();
        let mut out = Vec::new();
        let mut events = Vec::new();
        for c in chunks {
            process_input(
                &mut carry,
                c,
                &mut |b| out.extend_from_slice(b),
                &mut |m| events.push(m),
            );
        }
        (out, events)
    }

    #[test]
    fn plain_bytes_pass_through() {
        let (out, events) = run(&[b"hello"]);
        assert_eq!(out, b"hello");
        assert!(events.is_empty());
    }

    #[test]
    fn complete_sequence_extracted_mid_stream() {
        let (out, events) = run(&[b"hi\x1b[<0;12;34Mbye"]);
        assert_eq!(out, b"hibye");
        assert_eq!(
            events,
            vec![SgrMouse {
                button: 0,
                motion: false,
                pressed: true,
                x: 12,
                y: 34,
            }]
        );
    }

    #[test]
    fn sequence_split_across_chunks_is_held_then_emitted() {
        let (out, events) = run(&[b"a\x1b[<0;5;", b"7Mtail"]);
        assert_eq!(out, b"atail");
        assert_eq!(events.len(), 1);
        assert_eq!((events[0].x, events[0].y), (5, 7));
        assert!(events[0].pressed);
    }

    #[test]
    fn sequence_split_at_prefix_boundary() {
        // Lone ESC held, then the rest of the sequence arrives.
        let (out, events) = run(&[b"xy\x1b", b"[<0;1;2m"]);
        assert_eq!(out, b"xy");
        assert_eq!(events.len(), 1);
        assert!(!events[0].pressed);
        assert_eq!((events[0].x, events[0].y), (1, 2));
    }

    #[test]
    fn esc_bracket_prefix_is_held_for_mouse() {
        let (out, events) = run(&[b"q\x1b[", b"<32;9;9M"]);
        assert_eq!(out, b"q");
        assert_eq!(
            events,
            vec![SgrMouse {
                button: 32,
                motion: true,
                pressed: true,
                x: 9,
                y: 9,
            }]
        );
    }

    #[test]
    fn esc_bracket_that_is_not_mouse_passes_through() {
        // ESC[ followed by a non-`<` byte is just a key sequence (e.g.
        // arrow keys): nothing may be dropped.
        let (out, events) = run(&[b"\x1b[", b"A"]);
        assert_eq!(out, b"\x1b[A");
        assert!(events.is_empty());
    }

    #[test]
    fn esc_lt_with_non_digit_is_not_a_mouse_sequence() {
        let (out, events) = run(&[b"ESC\x1b[<xrest"]);
        assert_eq!(out, b"ESC\x1b[<xrest");
        assert!(events.is_empty());
    }

    #[test]
    fn motion_bit_and_wheel_buttons() {
        let (out, events) = run(&[seq(64, 3, 3, true).as_bytes()]);
        assert!(out.is_empty());
        assert_eq!(events[0].button, 64);
        assert!(!events[0].motion);
    }

    #[test]
    fn multiple_events_one_chunk() {
        let (out, events) = run(&[&format!("{}{}", seq(0, 1, 1, true), seq(0, 4, 4, false)).into_bytes()[..]]);
        assert!(out.is_empty());
        assert_eq!(events.len(), 2);
        assert!(events[0].pressed);
        assert!(!events[1].pressed);
    }

    #[test]
    fn span_single_row_reversed() {
        let s = Selection { anchor: (5, 2), end: (2, 2), active: true };
        assert_eq!(s.span(10, 20), Some((2, 2, 2, 5)));
    }

    #[test]
    fn span_multi_row_anchor_above() {
        let s = Selection { anchor: (3, 1), end: (7, 4), active: true };
        assert_eq!(s.span(10, 20), Some((1, 4, 3, 7)));
    }

    #[test]
    fn span_multi_row_anchor_below() {
        let s = Selection { anchor: (7, 4), end: (3, 1), active: true };
        assert_eq!(s.span(10, 20), Some((1, 4, 3, 7)));
    }

    #[test]
    fn span_inactive_or_empty_none() {
        assert_eq!(Selection::default().span(10, 20), None);
        let s = Selection { anchor: (0, 0), end: (0, 0), active: true };
        assert_eq!(s.span(0, 0), None);
    }

    #[test]
    fn span_clamped_to_screen() {
        let s = Selection { anchor: (99, 99), end: (5, 5), active: true };
        assert_eq!(s.span(10, 20), Some((5, 9, 5, 19)));
    }

    #[test]
    fn extract_text_single_and_multi_line() {
        let mut p = vt100::Parser::new(4, 12, 0);
        p.process(b"hello there\r\nworld foo\r\nbar baz qux");
        let screen = p.screen();

        let s = Selection { anchor: (0, 0), end: (4, 0), active: true };
        assert_eq!(extract_text(screen, &s), "hello");

        let s = Selection { anchor: (6, 1), end: (9, 1), active: true };
        assert_eq!(extract_text(screen, &s), "foo "); // selection includes the trailing cell

        // Multi-line: start row from its col, middle row full-width (empty
        // cells become spaces), last row truncated at its end col.
        let s = Selection { anchor: (6, 0), end: (2, 2), active: true };
        assert_eq!(extract_text(screen, &s), "there \nworld foo   \nbar");

        // Whole screen from (0,0) to the corner.
        let s = Selection { anchor: (0, 0), end: (11, 3), active: true };
        assert_eq!(
            extract_text(screen, &s),
            format!("hello there \nworld foo   \nbar baz qux \n{}", " ".repeat(12))
        );
    }

    #[test]
    fn mouse_modes_tracked() {
        let s = MouseState::default();
        let s = scan_mouse_modes(b"\x1b[?1002h\x1b[?1006h", &s);
        assert!(s.active());
        assert!(s.sgr());
        assert!(!s.press);
        let s = scan_mouse_modes(b"\x1b[?1006l", &s);
        assert!(s.active());
        assert!(s.sgr()); // no encoding left → SGR default
        let s = scan_mouse_modes(b"\x1b[?1002l\x1b[?1000l", &s);
        assert!(!s.active());
    }

    #[test]
    fn mouse_modes_rxvt_encoding() {
        let s = scan_mouse_modes(b"\x1b[?1002h\x1b[?1015h", &MouseState::default());
        assert!(s.active());
        assert!(!s.sgr()); // rxvt-only
        let s = scan_mouse_modes(b"\x1b[?1006h", &s);
        assert!(s.sgr()); // SGR wins when both are on
    }

    #[test]
    fn mouse_modes_multi_param() {
        let s = scan_mouse_modes(b"\x1b[?1000;1002;1006h", &MouseState::default());
        assert!(s.press && s.drag && s.sgr);
        let s = scan_mouse_modes(b"\x1b[?1000;1002l", &s);
        assert!(!s.press && !s.drag);
    }

    #[test]
    fn mouse_modes_ignore_non_mouse_csi() {
        let s = scan_mouse_modes(b"\x1b[?25h\x1b[?2004h", &MouseState::default());
        assert!(!s.active());
    }

    #[test]
    fn left_select_includes_drag_excludes_wheel_and_hover() {
        let ev = |button: u16, pressed| SgrMouse {
            button,
            motion: button & 32 != 0,
            pressed,
            x: 1,
            y: 1,
        };
        assert!(ev(0, true).is_left_select()); // left press
        assert!(ev(32, true).is_left_select()); // left drag
        assert!(ev(0, false).is_left_select()); // left release
        assert!(!ev(35, true).is_left_select()); // buttonless motion (?1003)
        assert!(!ev(34, true).is_left_select()); // right drag
        assert!(!ev(64, true).is_left_select()); // wheel up
        assert!(!ev(65, true).is_left_select()); // wheel down
    }

    #[test]
    fn encode_forward_sgr_and_rxvt() {
        let m = SgrMouse { button: 32, motion: true, pressed: true, x: 0, y: 0 };
        assert_eq!(encode_forward(&m, 2, 4, true), b"\x1b[<32;2;4M");
        let m = SgrMouse { button: 3, motion: false, pressed: false, x: 0, y: 0 };
        assert_eq!(encode_forward(&m, 2, 4, true), b"\x1b[<3;2;4m");
        let m = SgrMouse { button: 0, motion: false, pressed: true, x: 0, y: 0 };
        assert_eq!(encode_forward(&m, 2, 4, false), vec![0x1b, b'[', b'M', 32 + 0, 32 + 2, 32 + 4]);
    }

    #[test]
    fn extract_text_wide_char_not_split() {
        let mut p = vt100::Parser::new(2, 6, 0);
        p.process(b"a\xe4\xb8\xad b\r\ncdefgh"); // 中 is 2 cells wide
        let screen = p.screen();
        // Select from col 1 (the wide char) across the boundary.
        let s = Selection { anchor: (1, 0), end: (4, 0), active: true };
        let text = extract_text(screen, &s);
        assert_eq!(text, "\u{4e2d} b"); // wide char + space + 'b', no split glyph
        // Selecting the continuation cell alone yields nothing, not garbage.
        let s = Selection { anchor: (2, 0), end: (2, 0), active: true };
        assert_ne!(extract_text(screen, &s), " ");
    }
}
