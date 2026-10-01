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

    #[test]
    fn detect_without_tools_falls_back_to_osc52() {
        std::env::set_var("PATH", "/nonexistent-floax-test-dir");
        let clip = Clipboard::detect();
        assert_eq!(clip.set("hi"), Some(osc52("hi")));
    }
}
