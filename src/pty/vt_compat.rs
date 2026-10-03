//! Rewrites CSI sequences the `vt100` crate ignores into ones it handles.
//!
//! The screen tracker's parser only sees this rewritten copy; the terminal
//! receives the tool's original bytes. Without it, the tracker's cursor drifts
//! from the real terminal for tools whose renderers emit these sequences
//! (Antigravity uses REP for runs of spaces and box rules), and every later
//! relative cursor move lands in the wrong cell: injected text renders
//! scrambled on the tracked screen while the real terminal is correct.
//!
//! - HPA `CSI n \`` (horizontal position absolute) → CHA `CSI n G`
//! - REP `CSI n b` (repeat preceding graphic character) → the character n times

/// Upper bound on a single REP expansion, so a hostile or corrupt count can't
/// balloon memory. Far wider than any real terminal row.
const MAX_REPEAT: usize = 4096;

/// Longest CSI sequence buffered before giving up and passing it through.
const MAX_CSI_LEN: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    /// OSC/DCS/SOS/PM/APC payload, terminated by BEL or ST (`ESC \`).
    String,
    StringEscape,
}

/// Streaming rewriter. Sequences split across reads are buffered until complete.
pub(crate) struct VtCompat {
    state: State,
    csi: Vec<u8>,
    /// UTF-8 bytes of the last graphic character printed in ground state.
    last_char: Vec<u8>,
}

impl VtCompat {
    pub(crate) fn new() -> Self {
        Self {
            state: State::Ground,
            csi: Vec::with_capacity(MAX_CSI_LEN),
            last_char: Vec::with_capacity(4),
        }
    }

    pub(crate) fn normalize(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        for &b in data {
            match self.state {
                State::Ground => {
                    if b == 0x1b {
                        // Emitted together with the byte that follows it.
                        self.state = State::Escape;
                        continue;
                    } else if b >= 0x80 && b & 0xC0 == 0x80 {
                        // UTF-8 continuation byte of the current character.
                        if !self.last_char.is_empty() && self.last_char.len() < 4 {
                            self.last_char.push(b);
                        }
                    } else if b >= 0x20 && b != 0x7f {
                        self.last_char.clear();
                        self.last_char.push(b);
                    }
                    out.push(b);
                }
                State::Escape => {
                    if b == b'[' {
                        self.state = State::Csi;
                        self.csi.clear();
                        continue;
                    }
                    self.state = match b {
                        b']' | b'P' | b'X' | b'^' | b'_' => State::String,
                        // Intermediates (e.g. `ESC ( B`) precede a final byte.
                        0x20..=0x2f => State::EscapeIntermediate,
                        _ => State::Ground,
                    };
                    out.extend_from_slice(&[0x1b, b]);
                }
                State::EscapeIntermediate => {
                    if !(0x20..=0x2f).contains(&b) {
                        self.state = State::Ground;
                    }
                    out.push(b);
                }
                State::Csi => {
                    self.csi.push(b);
                    if (0x40..=0x7e).contains(&b) {
                        self.finish_csi(&mut out);
                        self.state = State::Ground;
                    } else if self.csi.len() >= MAX_CSI_LEN {
                        out.extend_from_slice(b"\x1b[");
                        out.extend_from_slice(&self.csi);
                        self.state = State::Ground;
                    }
                }
                State::String => {
                    if b == 0x07 {
                        self.state = State::Ground;
                    } else if b == 0x1b {
                        self.state = State::StringEscape;
                    }
                    out.push(b);
                }
                State::StringEscape => {
                    self.state = if b == b'\\' {
                        State::Ground
                    } else {
                        State::String
                    };
                    out.push(b);
                }
            }
        }
        // A trailing partial CSI stays buffered in `self.csi` for the next call.
        out
    }

    fn finish_csi(&mut self, out: &mut Vec<u8>) {
        let (params, final_byte) = self.csi.split_at(self.csi.len() - 1);
        let plain = params.iter().all(|b| b.is_ascii_digit());
        match final_byte[0] {
            b'`' if plain => {
                out.extend_from_slice(b"\x1b[");
                out.extend_from_slice(params);
                out.push(b'G');
            }
            b'b' if plain => {
                let count = std::str::from_utf8(params)
                    .ok()
                    .and_then(|s| s.parse::<usize>().ok())
                    .filter(|&n| n > 0)
                    .unwrap_or(1)
                    .min(MAX_REPEAT);
                for _ in 0..count {
                    out.extend_from_slice(&self.last_char);
                }
            }
            _ => {
                out.extend_from_slice(b"\x1b[");
                out.extend_from_slice(&self.csi);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(chunks: &[&[u8]]) -> Vec<u8> {
        let mut c = VtCompat::new();
        chunks.iter().flat_map(|d| c.normalize(d)).collect()
    }

    #[test]
    fn expands_rep_with_last_ascii_char() {
        assert_eq!(norm(&[b"a\x1b[3b|"]), b"aaaa|");
        assert_eq!(norm(&[b"x\x1b[b"]), b"xx");
    }

    #[test]
    fn expands_rep_with_last_multibyte_char_across_sgr() {
        assert_eq!(
            norm(&["─\x1b[38;5;1m\x1b[2b".as_bytes()]),
            "─\x1b[38;5;1m──".as_bytes()
        );
    }

    #[test]
    fn rewrites_hpa_to_cha() {
        assert_eq!(norm(&[b"\x1b[12`x"]), b"\x1b[12Gx");
        assert_eq!(norm(&[b"\x1b[`"]), b"\x1b[G");
    }

    #[test]
    fn handles_sequences_split_across_reads() {
        assert_eq!(norm(&[b"ab\x1b", b"[4", b"b"]), b"abbbbb");
        assert_eq!(
            norm(&[
                "\u{2500}".as_bytes()[..1].as_ref(),
                &"\u{2500}".as_bytes()[1..],
                b"\x1b[1b"
            ]),
            "──".as_bytes()
        );
    }

    #[test]
    fn passes_other_sequences_through() {
        let input: &[u8] = b"\x1b[?25l\x1b[2A\x1b[42D\x1b[>4;2m\x1b(B\x1b]0;t b\x07z";
        assert_eq!(norm(&[input]), input);
        // Charset designation's final byte is not a printed character.
        assert_eq!(norm(&[b"a\x1b(B\x1b[2b"]), b"a\x1b(Baa");
    }

    #[test]
    fn osc_payload_does_not_become_rep_source() {
        assert_eq!(
            norm(&[b"q\x1b]2;title\x1b\\\x1b[2b"]),
            b"q\x1b]2;title\x1b\\qq"
        );
    }

    #[test]
    fn private_rep_is_untouched_and_count_is_capped() {
        assert_eq!(norm(&[b"a\x1b[?3b"]), b"a\x1b[?3b");
        assert_eq!(norm(&[b"a\x1b[999999b"]).len(), 1 + MAX_REPEAT);
    }
}
