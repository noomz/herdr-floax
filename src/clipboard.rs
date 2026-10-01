//! Clipboard: set the host clipboard from floax.
//!
//! Preferred: a local clipboard tool on `PATH` (`wl-copy`, `xclip`,
//! `xsel`, `pbcopy`, `termux-clipboard-set`) — deterministic and testable.
//! Fallback: OSC 52 (`ESC ] 52 ; c ; <base64> ESC \`) returned to the caller
//! for emission on the app's stdout; whether it works depends on the host
//! terminal (and any multiplexer in between) honoring it, so it is
//! best-effort.

use std::io::Write;

/// A clipboard handle that detects the copy tool once.
#[derive(Clone, Default)]
pub struct Clipboard {
    tool: Option<Tool>,
}

#[derive(Clone, Copy)]
struct Tool {
    program: &'static str,
    args: &'static [&'static str],
}

impl Clipboard {
    /// Detect the first available clipboard tool on `PATH`.
    pub fn detect() -> Self {
        let tool = find_in("wl-copy")
            .then(|| Tool {
                program: "wl-copy",
                args: &[],
            })
            .or_else(|| {
                find_in("xclip").then(|| Tool {
                    program: "xclip",
                    args: &["-selection", "clipboard", "-i"],
                })
            })
            .or_else(|| {
                find_in("xsel").then(|| Tool {
                    program: "xsel",
                    args: &["--clipboard", "--input"],
                })
            })
            .or_else(|| {
                find_in("pbcopy").then(|| Tool {
                    program: "pbcopy",
                    args: &[],
                })
            })
            .or_else(|| {
                find_in("termux-clipboard-set").then(|| Tool {
                    program: "termux-clipboard-set",
                    args: &[],
                })
            });
        Self { tool }
    }

    /// Copy `text` to the clipboard. Returns `None` on success, or an OSC 52
    /// sequence for the caller to write on the app's stdout when no local
    /// tool is available or it failed.
    pub fn set(&self, text: &str) -> Option<String> {
        match self.tool {
            Some(t) if spawn_tool(t, text) => None,
            _ => Some(osc52(text)),
        }
    }
}

/// Whether `program` is an executable on `PATH`.
fn find_in(program: &str) -> bool {
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    for dir in std::env::split_paths(&path) {
        if dir.join(program).is_file() {
            return true;
        }
    }
    false
}

fn spawn_tool(tool: Tool, text: &str) -> bool {
    let mut child = match std::process::Command::new(tool.program)
        .args(tool.args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    {
        let Some(mut stdin) = child.stdin.take() else {
            return false;
        };
        if stdin.write_all(text.as_bytes()).is_err() {
            return false;
        }
    }
    child.wait().is_ok()
}

/// OSC 52 clipboard-set sequence, chunked (terminals cap per-write size).
/// Chunk 1 is plain; later chunks use the `+` append form.
pub fn osc52(text: &str) -> String {
    let b64 = base64(text.as_bytes());
    let mut out = String::new();
    let mut first = true;
    for chunk in b64.as_bytes().chunks(800) {
        let plus = if first { "" } else { "+" };
        out.push_str("\u{1b}]52;c;");
        out.push_str(plus);
        out.push_str(std::str::from_utf8(chunk).unwrap());
        out.push_str("\u{1b}\\");
        first = false;
    }
    out
}

const OSC52_INTRO: &[u8] = b"\x1b]52;";

/// Upper bound on a buffered, unterminated OSC 52 sequence; anything larger
/// is dropped instead of growing without limit.
const MAX_OSC52: usize = 8 * 1024 * 1024;

/// Pulls OSC 52 clipboard writes out of the embedded program's output so they
/// can be relayed to herdr — vt100 parses and drops them, so without this a
/// copy made by tmux/vim inside the box never reaches the host clipboard.
/// Sequences may span reads. Clipboard queries (`?`) are never relayed.
#[derive(Default)]
pub struct Osc52Relay {
    /// Tail of the previous read: an unterminated OSC 52 sequence, or a
    /// trailing prefix of its introducer.
    partial: Vec<u8>,
}

impl Osc52Relay {
    /// Append every complete OSC 52 write in `bytes` (joined with any partial
    /// sequence carried from the previous call) to `out`, verbatim.
    pub fn scan(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        let joined;
        let buf: &[u8] = if self.partial.is_empty() {
            bytes
        } else {
            let mut v = std::mem::take(&mut self.partial);
            v.extend_from_slice(bytes);
            joined = v;
            &joined
        };
        let mut i = 0;
        loop {
            let Some(start) = find(&buf[i..], OSC52_INTRO).map(|p| p + i) else {
                let keep = intro_prefix_len(&buf[i..]);
                self.partial = buf[buf.len() - keep..].to_vec();
                return;
            };
            match osc_end(&buf[start..]) {
                Some(len) => {
                    relay_write(&buf[start..start + len], out);
                    i = start + len;
                }
                None => {
                    if buf.len() - start <= MAX_OSC52 {
                        self.partial = buf[start..].to_vec();
                    }
                    return;
                }
            }
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Length of the longest proper prefix of the OSC 52 introducer that `tail`
/// ends with (the rest of it may arrive in the next read).
fn intro_prefix_len(tail: &[u8]) -> usize {
    (1..OSC52_INTRO.len())
        .rev()
        .find(|&k| tail.ends_with(&OSC52_INTRO[..k]))
        .unwrap_or(0)
}

/// Length of the OSC sequence at the start of `seq` through its terminator
/// (BEL or ST), or `None` if the terminator hasn't arrived yet. An ESC not
/// followed by `\` aborts the sequence; it is consumed up to that ESC.
fn osc_end(seq: &[u8]) -> Option<usize> {
    let mut j = OSC52_INTRO.len();
    while j < seq.len() {
        match seq[j] {
            0x07 => return Some(j + 1),
            0x1b => {
                return match seq.get(j + 1) {
                    Some(b'\\') => Some(j + 2),
                    Some(_) => Some(j),
                    None => None,
                }
            }
            _ => j += 1,
        }
    }
    None
}

/// Append `seq` to `out` if it is a well-terminated clipboard write
/// (`ESC ] 52 ; <selection> ; <base64> BEL|ST`), not a query or aborted.
fn relay_write(seq: &[u8], out: &mut Vec<u8>) {
    let term = if seq.ends_with(b"\x07") {
        1
    } else if seq.ends_with(b"\x1b\\") {
        2
    } else {
        return;
    };
    let body = &seq[OSC52_INTRO.len()..seq.len() - term];
    let Some(semi) = body.iter().position(|&b| b == b';') else {
        return;
    };
    if &body[semi + 1..] == b"?" {
        return;
    }
    out.extend_from_slice(seq);
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(B64[(b0 >> 2) as usize] as char);
        out.push(B64[((b0 & 3) << 4 | b1 >> 4) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64[((b1 & 0xf) << 2 | b2 >> 6) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"hello world"), "aGVsbG8gd29ybGQ=");
    }

    #[test]
    fn osc52_format_single_chunk() {
        assert_eq!(osc52("foo"), "\u{1b}]52;c;Zm9v\u{1b}\\");
    }

    #[test]
    fn osc52_chunks_with_append_marker() {
        let big = "x".repeat(2000); // > 800 base64 chars
        let s = osc52(&big);
        // 2000 bytes → 2668 base64 chars → 4 chunks of ≤ 800 (+ trailing "").
        let parts: Vec<&str> = s.split("\u{1b}\\").collect();
        assert_eq!(parts.len(), 5);
        assert!(parts[0].starts_with("\u{1b}]52;c;eH")); // b64 of 'xxx...'
        for p in &parts[1..4] {
            assert!(p.starts_with("\u{1b}]52;c;+"));
        }
        assert_eq!(parts[4], "");
    }

    fn relay(reads: &[&[u8]]) -> Vec<u8> {
        let mut r = Osc52Relay::default();
        let mut out = Vec::new();
        for read in reads {
            r.scan(read, &mut out);
        }
        out
    }

    #[test]
    fn relay_passes_complete_writes_verbatim() {
        let bel = b"\x1b]52;c;aGk=\x07";
        let st = b"\x1b]52;c;aGk=\x1b\\";
        assert_eq!(relay(&[b"before\x1b]52;c;aGk=\x07after"]), bel);
        assert_eq!(relay(&[st]), st);
        assert_eq!(
            relay(&[b"\x1b]52;c;YQ==\x07x\x1b]52;c;Yg==\x07"]),
            b"\x1b]52;c;YQ==\x07\x1b]52;c;Yg==\x07"
        );
    }

    #[test]
    fn relay_joins_sequences_split_across_reads() {
        let full = b"\x1b]52;c;aGVsbG8=\x1b\\";
        for cut in 1..full.len() {
            assert_eq!(relay(&[&full[..cut], &full[cut..]]), full, "cut at {cut}");
        }
    }

    #[test]
    fn relay_drops_queries_aborted_and_other_osc() {
        assert!(relay(&[b"\x1b]52;c;?\x07"]).is_empty());
        assert!(relay(&[b"\x1b]52;c;aGk=\x1b[0m"]).is_empty());
        assert!(relay(&[b"\x1b]0;title\x07\x1b]8;;http://x\x1b\\"]).is_empty());
        assert!(relay(&[b"\x1b]52;aGk=\x07"]).is_empty()); // no selection field
    }

    #[test]
    fn relay_recovers_after_aborted_sequence() {
        let out = relay(&[b"\x1b]52;c;aGk=\x1b[0m\x1b]52;c;YQ==\x07"]);
        assert_eq!(out, b"\x1b]52;c;YQ==\x07");
    }

    #[test]
    fn detect_without_tools_falls_back_to_osc52() {
        std::env::set_var("PATH", "/nonexistent-floax-test-dir");
        let clip = Clipboard::detect();
        assert_eq!(clip.set("hi"), Some(osc52("hi")));
    }
}
