//! herdr-floax — a floating scratch shell for herdr, à la tmux-floax.
//!
//! Runs as one persistent herdr pane (opened as a zoomed split by the toggle
//! script) and draws a centered, sized box hosting a real shell PTY. herdr has
//! no sized floating-pane primitive that survives a keybinding — its
//! `overlay`/`zoomed` placements are transient views torn down when the
//! invoking action finishes — so the floating look is drawn here, in-process,
//! the same way herdr-file-viewer draws its help overlay. The box hosts a live
//! PTY rather than static text, which is what makes it a shell and not a modal.
//!
//! KNOWN LIMITATION: the backdrop around the box is a solid fill this app
//! paints, NOT the user's live panes dimmed behind it (tmux-floax shows the
//! real session through its popup). A plugin only controls its own pane's
//! canvas — herdr owns the other panes' PTYs and has no primitive for
//! compositing a persistent popup over them. See README.md "Limitations".
//!
//! Input is raw stdin passthrough: every byte herdr delivers to this pane goes
//! to the embedded shell verbatim (no lossy key-event translation), so vim,
//! REPLs, paste, and modifier chords all behave. herdr's prefix key never
//! reaches us — herdr intercepts it — so the toggle keybinding keeps working
//! while the shell is focused.
//!
//! Mouse: the app owns it (SGR capture). A left drag inside the box selects
//! the embedded screen's text and copies it to the clipboard on release —
//! selection is clipped to the box and can never spill into the backdrop.
//! When the embedded program asks for the mouse itself (tmux `mouse on`,
//! vim `:set mouse=a`, …), events are forwarded to it, coordinates re-based
//! into the box interior.
//!
//! The embedded program is scripts/floating-shell.sh (a login shell wrapped in
//! a per-workspace dtach/abduco/tmux session when available), so the session
//! survives the pane being closed on dismiss and re-attaches on reopen.

mod clipboard;
mod config;
mod mouse;
mod ui;

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;

/// Events that wake the render loop. Input never appears here — it flows
/// straight from stdin to the PTY on its own thread (mouse events are
/// intercepted first; see `mouse`).
enum Ev {
    /// The embedded terminal produced output; redraw.
    Output,
    /// SIGWINCH: recompute the box and resize the PTY.
    Winch,
    /// The embedded program exited (or the PTY hit EOF); quit.
    Exit,
}

fn io_err(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
}

fn pty_size(inner: Rect) -> PtySize {
    PtySize {
        rows: inner.height,
        cols: inner.width,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Track the embedded program's bracketed-paste mode (CSI `?2004 h/l`) so the
/// middle-click repaste is only synthesized when the program understands it.
fn scan_bracketed_paste(bytes: &[u8], flag: &AtomicBool) {
    let mut i = 0;
    while i + 7 <= bytes.len() {
        if bytes[i] == 0x1b
            && bytes[i + 1] == b'['
            && bytes[i + 2] == b'?'
            && bytes[i + 3..i + 6] == *b"2004"
        {
            match bytes[i + 6] {
                b'h' => flag.store(true, Ordering::Relaxed),
                b'l' => flag.store(false, Ordering::Relaxed),
                _ => {}
            }
            i += 7;
            continue;
        }
        i += 1;
    }
}

/// Route one SGR mouse event. If the embedded program owns the mouse, the
/// event is forwarded to it (coordinates re-based into the box interior).
/// Otherwise floax runs its own selection, strictly confined to the box:
/// drags outside are ignored, drags that leave the box clamp at the border.
fn handle_mouse(
    m: mouse::SgrMouse,
    parser: &Arc<Mutex<vt100::Parser>>,
    inner: &Arc<Mutex<Rect>>,
    sel: &Arc<Mutex<mouse::Selection>>,
    last_copy: &Arc<Mutex<String>>,
    clip: &Arc<clipboard::Clipboard>,
    pending_osc: &Arc<Mutex<Vec<u8>>>,
    bracketed: &Arc<AtomicBool>,
    mouse_state: &Arc<Mutex<mouse::MouseState>>,
    tx: &mpsc::Sender<Ev>,
    writer: &Arc<Mutex<Box<dyn Write + Send>>>,
) {
    let inner = *inner.lock().unwrap();
    let (px, py) = (m.x.saturating_sub(1), m.y.saturating_sub(1));
    let inside = px >= inner.x
        && px < inner.x + inner.width
        && py >= inner.y
        && py < inner.y + inner.height;

    // Embedded program owns the mouse (tmux `mouse on`, vim `:set mouse=a`,
    // …): forward the event and let it handle selection/scrolling.
    if mouse_state.lock().unwrap().active() {
        if inside {
            let fx = px - inner.x + 1;
            let fy = py - inner.y + 1;
            let sgr = mouse_state.lock().unwrap().sgr();
            let seq = mouse::encode_forward(&m, fx, fy, sgr);
            let mut w = writer.lock().unwrap();
            let _ = w.write_all(&seq);
            let _ = w.flush();
        }
        return;
    }

    // Left button: floax's own selection, clipped to the box interior.
    // Button bits: 0-2 button, 32 motion (so left-drag arrives as 32),
    // 64 wheel. Release arrives as button+3 (left = 3) or the original
    // button with the final letter 'm'; any left release finalizes a drag.
    if m.is_left_select() {
        let col = (px.saturating_sub(inner.x)).min(inner.width.saturating_sub(1));
        let row = (py.saturating_sub(inner.y)).min(inner.height.saturating_sub(1));
        let mut s = sel.lock().unwrap();
        if m.pressed {
            if !s.active && inside {
                s.anchor = (col, row);
                s.end = (col, row);
                s.active = true;
            } else if s.active {
                s.end = (col, row);
            } else {
                return; // press in the backdrop: never selects
            }
            let _ = tx.send(Ev::Output);
        } else if s.active {
            s.end = (col, row);
            let text = {
                let guard = parser.lock().unwrap();
                mouse::extract_text(guard.screen(), &s)
            };
            s.reset();
            let _ = tx.send(Ev::Output);
            if !text.is_empty() {
                *last_copy.lock().unwrap() = text.clone();
                let clip = Arc::clone(clip);
                let po = Arc::clone(pending_osc);
                let tx = tx.clone();
                // Don't block the input thread on a process spawn.
                std::thread::spawn(move || {
                    if let Some(osc) = clip.set(&text) {
                        po.lock().unwrap().extend_from_slice(osc.as_bytes());
                        let _ = tx.send(Ev::Output);
                    }
                });
            }
        }
        return;
    }

    // Middle-button paste: mouse capture took selection away from the host
    // terminal, so repaste the last copy ourselves (bracketed, when the
    // embedded program supports it).
    if m.button == 1 && m.pressed && bracketed.load(Ordering::Relaxed) {
        let txt = last_copy.lock().unwrap().clone();
        if !txt.is_empty() {
            let mut w = writer.lock().unwrap();
            let _ = write!(w, "\u{1b}[200h{txt}\u{1b}[201l");
            let _ = w.flush();
        }
    }
    // Right button and wheel are ignored unless the embedded program owns the
    // mouse (no vt100 scrollback, and the host context menu is gone while we
    // capture).
}

fn main() -> std::io::Result<()> {
    let cfg = config::Config::load();

    let (cols, rows) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
    let inner = ui::box_inner(Rect::new(0, 0, cols, rows), &cfg);

    // Shared state between input, output, and render threads.
    let inner_rect = Arc::new(Mutex::new(inner));
    let sel = Arc::new(Mutex::new(mouse::Selection::default()));
    let last_copy = Arc::new(Mutex::new(String::new()));
    // OSC 52 clipboard writes queue here; only the render thread writes
    // stdout, so emitting them there can never tear a frame.
    let pending_osc = Arc::new(Mutex::new(Vec::<u8>::new()));
    let clip = Arc::new(clipboard::Clipboard::detect());
    let bracketed = Arc::new(AtomicBool::new(false));
    let mouse_state = Arc::new(Mutex::new(mouse::MouseState::default()));

    // Spawn the embedded shell sized to the box interior.
    let pty = native_pty_system();
    let pair = pty.openpty(pty_size(inner)).map_err(io_err)?;

    let root = std::env::var("HERDR_PLUGIN_ROOT").unwrap_or_else(|_| ".".into());
    let mut cmd = CommandBuilder::new("bash");
    cmd.arg(format!("{root}/scripts/floating-shell.sh"));
    for (k, v) in std::env::vars() {
        cmd.env(k, v);
    }
    cmd.env("TERM", "xterm-256color");
    if let Ok(d) = std::env::var("HERDR_FLOAX_CWD") {
        if !d.is_empty() && std::path::Path::new(&d).is_dir() {
            cmd.cwd(d);
        }
    }
    let mut child = pair.slave.spawn_command(cmd).map_err(io_err)?;
    drop(pair.slave);

    let parser = Arc::new(Mutex::new(vt100::Parser::new(inner.height, inner.width, 0)));
    let (tx, rx) = mpsc::channel::<Ev>();

    // PTY output → vt100 parser → redraw.
    {
        let parser = Arc::clone(&parser);
        let tx = tx.clone();
        let bracketed = Arc::clone(&bracketed);
        let mouse_state = Arc::clone(&mouse_state);
        let pending_osc = Arc::clone(&pending_osc);
        let mut reader = pair.master.try_clone_reader().map_err(io_err)?;
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut osc52 = clipboard::Osc52Relay::default();
            let mut copied = Vec::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(Ev::Exit);
                        break;
                    }
                    Ok(n) => {
                        scan_bracketed_paste(&buf[..n], &bracketed);
                        {
                            let mut ms = mouse_state.lock().unwrap();
                            *ms = mouse::scan_mouse_modes(&buf[..n], &ms);
                        }
                        // vt100 drops OSC 52; relay the embedded program's
                        // clipboard writes (tmux, vim, ssh) to herdr.
                        osc52.scan(&buf[..n], &mut copied);
                        if !copied.is_empty() {
                            pending_osc.lock().unwrap().append(&mut copied);
                        }
                        parser.lock().unwrap().process(&buf[..n]);
                        if tx.send(Ev::Output).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }

    // stdin → (mouse intercept) → PTY, raw byte passthrough. A reader thread
    // blocks on stdin; the processor holds back bytes that could start a
    // partial SGR mouse sequence and flushes them if no continuation arrives.
    {
        let (itx, irx) = mpsc::channel::<std::io::Result<Vec<u8>>>();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 2048];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if itx.send(Ok(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let writer = Arc::new(Mutex::new(pair.master.take_writer().map_err(io_err)?));
        let parser = Arc::clone(&parser);
        let inner_rect = Arc::clone(&inner_rect);
        let sel = Arc::clone(&sel);
        let last_copy = Arc::clone(&last_copy);
        let clip = Arc::clone(&clip);
        let pending_osc = Arc::clone(&pending_osc);
        let bracketed = Arc::clone(&bracketed);
        let mouse_state = Arc::clone(&mouse_state);
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut carry = Vec::new();
            loop {
                match irx.recv_timeout(Duration::from_millis(20)) {
                    Ok(Ok(chunk)) => {
                        mouse::process_input(
                            &mut carry,
                            &chunk,
                            &mut |b: &[u8]| {
                                let mut w = writer.lock().unwrap();
                                let _ = w.write_all(b);
                                let _ = w.flush();
                            },
                            &mut |m| {
                                handle_mouse(
                                    m, &parser, &inner_rect, &sel, &last_copy, &clip,
                                    &pending_osc, &bracketed, &mouse_state, &tx, &writer,
                                );
                            },
                        );
                    }
                    // Held-back bytes (a possible partial mouse sequence) with
                    // no continuation: flush them raw to the PTY.
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if !carry.is_empty() {
                            let held = std::mem::take(&mut carry);
                            let mut w = writer.lock().unwrap();
                            let _ = w.write_all(&held);
                            let _ = w.flush();
                        }
                    }
                    Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
    }

    // Embedded program exit → quit.
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = child.wait();
            let _ = tx.send(Ev::Exit);
        });
    }

    // SIGWINCH → re-layout.
    {
        let tx = tx.clone();
        let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])?;
        std::thread::spawn(move || {
            for _ in signals.forever() {
                if tx.send(Ev::Winch).is_err() {
                    break;
                }
            }
        });
    }

    // Terminal up. Restore on panic too, so a bug never wedges the pane raw.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));
    enable_raw_mode()?;

    execute!(std::io::stdout(), EnterAlternateScreen)?;
    // Take the mouse so the host terminal's full-canvas selection cannot
    // spill across the box border (see `mouse`).
    execute!(std::io::stdout(), EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;

    let master = pair.master;
    loop {
        // Emit any queued OSC 52 clipboard write here: this is the only
        // thread writing stdout, so it can't interleave with a frame.
        let osc = std::mem::take(&mut *pending_osc.lock().unwrap());
        if !osc.is_empty() {
            let backend = terminal.backend_mut();
            let _ = backend.write_all(&osc);
            let _ = std::io::Write::flush(backend);
        }
        {
            let parser = parser.lock().unwrap();
            let sel = sel.lock().unwrap();
            terminal.draw(|f| ui::draw(f, &cfg, parser.screen(), &sel))?;
        }
        let Ok(first) = rx.recv() else { break };
        let mut exit = matches!(first, Ev::Exit);
        let mut winch = matches!(first, Ev::Winch);
        // Coalesce bursts: one redraw per batch of PTY output.
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Ev::Exit => exit = true,
                Ev::Winch => winch = true,
                Ev::Output => {}
            }
        }
        if exit {
            break;
        }
        if winch {
            let (c, r) = ratatui::crossterm::terminal::size().unwrap_or((cols, rows));
            let inner = ui::box_inner(Rect::new(0, 0, c, r), &cfg);
            *inner_rect.lock().unwrap() = inner;
            let _ = master.resize(pty_size(inner));
            parser.lock().unwrap().set_size(inner.height, inner.width);
            // The box moved; a stale selection would point at the wrong cells.
            sel.lock().unwrap().reset();
        }
    }

    restore_terminal();
    Ok(())
}
