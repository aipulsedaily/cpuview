//! Input parser: keys, SGR (and X10) mouse, and silent skipping of every other escape
//! sequence (late terminal replies: DA1, DECRQSS, XTVERSION, kitty APC …). Sequences
//! split across reads are kept until complete.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Key {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Enter,
    Esc,
    Backspace,
    Tab,
    Home,
    End,
    PgUp,
    PgDn,
    Delete,
    CtrlC,
    CtrlZ,
    CtrlU,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ev {
    Key(Key),
    /// Mouse press: 0-based cell, button (0 left, 1 middle, 2 right, 64/65 wheel up/down).
    Mouse {
        x: u16,
        y: u16,
        btn: u16,
    },
}

#[derive(Default)]
pub struct Parser {
    pend: Vec<u8>,
}

const MAX_PENDING: usize = 1 << 16;

fn num(b: &[u8]) -> u32 {
    b.iter().filter(|c| c.is_ascii_digit()).fold(0u32, |a, &c| {
        a.saturating_mul(10).saturating_add((c - b'0') as u32)
    })
}

impl Parser {
    pub fn pending(&self) -> bool {
        !self.pend.is_empty()
    }

    /// Only an ESC (or an unfinished CSI/SS3) is waiting: after a short timeout it is a
    /// real Esc key press.
    pub fn flush_timeout(&mut self, out: &mut Vec<Ev>) {
        if self.pend.first() == Some(&0x1b) {
            let second = self.pend.get(1).copied();
            match second {
                None => out.push(Ev::Key(Key::Esc)),
                Some(b'P' | b'_' | b']' | b'X' | b'^') if self.pend.len() < MAX_PENDING => return, // string: wait for ST
                _ => {}
            }
        }
        self.pend.clear();
    }

    pub fn feed(&mut self, data: &[u8], out: &mut Vec<Ev>) {
        self.pend.extend_from_slice(data);
        let b = std::mem::take(&mut self.pend);
        let mut i = 0;
        while i < b.len() {
            match self.one(&b[i..], out) {
                Some(n) => i += n,
                None => {
                    // incomplete: keep the rest for the next read
                    self.pend.extend_from_slice(&b[i..]);
                    if self.pend.len() > MAX_PENDING {
                        self.pend.clear();
                    }
                    return;
                }
            }
        }
    }

    /// Parse one token at the start of `b`. Returns bytes consumed, or None if incomplete.
    fn one(&mut self, b: &[u8], out: &mut Vec<Ev>) -> Option<usize> {
        let c = b[0];
        let key = |k| Ev::Key(k);
        match c {
            0x1b => {
                let n = *b.get(1)?;
                match n {
                    b'[' => self.csi(b, out),
                    b'O' => {
                        let f = *b.get(2)?;
                        match f {
                            b'A' => out.push(key(Key::Up)),
                            b'B' => out.push(key(Key::Down)),
                            b'C' => out.push(key(Key::Right)),
                            b'D' => out.push(key(Key::Left)),
                            b'H' => out.push(key(Key::Home)),
                            b'F' => out.push(key(Key::End)),
                            b'M' => out.push(key(Key::Enter)),
                            _ => {}
                        }
                        Some(3)
                    }
                    b'P' | b'_' | b']' | b'X' | b'^' => {
                        // string sequence: skip to ST (ESC \) or BEL
                        let mut j = 2;
                        loop {
                            let x = *b.get(j)?;
                            if x == 0x07 {
                                return Some(j + 1);
                            }
                            if x == 0x1b {
                                let y = *b.get(j + 1)?;
                                if y == b'\\' {
                                    return Some(j + 2);
                                }
                            }
                            j += 1;
                        }
                    }
                    0x1b => {
                        out.push(key(Key::Esc));
                        Some(1)
                    }
                    _ => Some(1), // Alt+key: drop the ESC, keep the key
                }
            }
            b'\r' | b'\n' => {
                out.push(key(Key::Enter));
                Some(1)
            }
            0x7f | 0x08 => {
                out.push(key(Key::Backspace));
                Some(1)
            }
            b'\t' => {
                out.push(key(Key::Tab));
                Some(1)
            }
            0x03 => {
                out.push(key(Key::CtrlC));
                Some(1)
            }
            0x1a => {
                out.push(key(Key::CtrlZ));
                Some(1)
            }
            0x15 => {
                out.push(key(Key::CtrlU));
                Some(1)
            }
            0..=0x1f => Some(1),
            0x20..=0x7e => {
                out.push(key(Key::Char(c as char)));
                Some(1)
            }
            _ => {
                let len = match c {
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf7 => 4,
                    _ => return Some(1),
                };
                if b.len() < len {
                    return None;
                }
                if let Some(ch) = std::str::from_utf8(&b[..len])
                    .ok()
                    .and_then(|s| s.chars().next())
                {
                    out.push(key(Key::Char(ch)));
                }
                Some(len)
            }
        }
    }

    fn csi(&mut self, b: &[u8], out: &mut Vec<Ev>) -> Option<usize> {
        let mut j = 2;
        loop {
            let x = *b.get(j)?;
            if (0x40..=0x7e).contains(&x) {
                break;
            }
            if !(0x20..=0x3f).contains(&x) {
                return Some(j); // malformed: drop what we saw
            }
            j += 1;
        }
        let fin = b[j];
        let params = &b[2..j];
        let k = |k| Ev::Key(k);
        if params.first() == Some(&b'<') {
            // SGR mouse: CSI < b ; x ; y M/m
            let mut it = params[1..].split(|&c| c == b';');
            let (bt, x, y) = (
                num(it.next().unwrap_or(b"")),
                num(it.next().unwrap_or(b"")),
                num(it.next().unwrap_or(b"")),
            );
            if fin == b'M' && bt & 32 == 0 && x > 0 && y > 0 {
                out.push(Ev::Mouse {
                    x: (x - 1) as u16,
                    y: (y - 1) as u16,
                    btn: (bt & !(4 | 8 | 16)) as u16,
                });
            }
            return Some(j + 1);
        }
        if fin == b'M' && params.is_empty() {
            // X10 mouse: CSI M Cb Cx Cy
            if b.len() < j + 4 {
                return None;
            }
            let (bt, x, y) = (
                b[j + 1].wrapping_sub(32),
                b[j + 2].wrapping_sub(33),
                b[j + 3].wrapping_sub(33),
            );
            if bt & 3 != 3 && bt & 32 == 0 {
                out.push(Ev::Mouse {
                    x: x as u16,
                    y: y as u16,
                    btn: (bt & !(4 | 8 | 16)) as u16,
                });
            }
            return Some(j + 4);
        }
        let private = params.first().is_some_and(|c| (0x3c..=0x3f).contains(c));
        if !private {
            match fin {
                b'A' => out.push(k(Key::Up)),
                b'B' => out.push(k(Key::Down)),
                b'C' => out.push(k(Key::Right)),
                b'D' => out.push(k(Key::Left)),
                b'H' => out.push(k(Key::Home)),
                b'F' => out.push(k(Key::End)),
                b'Z' => out.push(k(Key::Tab)),
                b'~' => match num(params.split(|&c| c == b';').next().unwrap_or(b"")) {
                    1 | 7 => out.push(k(Key::Home)),
                    4 | 8 => out.push(k(Key::End)),
                    3 => out.push(k(Key::Delete)),
                    5 => out.push(k(Key::PgUp)),
                    6 => out.push(k(Key::PgDn)),
                    _ => {}
                },
                _ => {} // replies (…t, …c, …R) and anything unknown
            }
        }
        Some(j + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(chunks: &[&[u8]]) -> Vec<Ev> {
        let mut p = Parser::default();
        let mut out = Vec::new();
        for c in chunks {
            p.feed(c, &mut out);
        }
        out
    }

    #[test]
    fn keys() {
        let e = parse(&[b"q s\r\x1b[A\x1b[B\x1bOC\x1b[D\x7f"]);
        assert_eq!(
            e,
            vec![
                Ev::Key(Key::Char('q')),
                Ev::Key(Key::Char(' ')),
                Ev::Key(Key::Char('s')),
                Ev::Key(Key::Enter),
                Ev::Key(Key::Up),
                Ev::Key(Key::Down),
                Ev::Key(Key::Right),
                Ev::Key(Key::Left),
                Ev::Key(Key::Backspace)
            ]
        );
        assert_eq!(
            parse(&[b"\x1b[1;5A\x1b[5~\x1b[3~"]),
            vec![Ev::Key(Key::Up), Ev::Key(Key::PgUp), Ev::Key(Key::Delete)]
        );
        assert_eq!(
            parse(&["é◆".as_bytes()]),
            vec![Ev::Key(Key::Char('é')), Ev::Key(Key::Char('◆'))]
        );
    }

    #[test]
    fn mouse() {
        assert_eq!(
            parse(&[b"\x1b[<0;10;5M\x1b[<0;10;5m"]),
            vec![Ev::Mouse { x: 9, y: 4, btn: 0 }]
        );
        assert_eq!(
            parse(&[b"\x1b[<65;1;1M"]),
            vec![Ev::Mouse {
                x: 0,
                y: 0,
                btn: 65
            }]
        );
        // drag (motion bit) ignored
        assert_eq!(parse(&[b"\x1b[<32;3;3M"]), vec![]);
        // X10
        assert_eq!(
            parse(&[b"\x1b[M !!"]),
            vec![Ev::Mouse { x: 0, y: 0, btn: 0 }]
        );
    }

    #[test]
    fn split_sequences() {
        assert_eq!(
            parse(&[b"\x1b", b"[", b"<0;2", b"0;7M"]),
            vec![Ev::Mouse {
                x: 19,
                y: 6,
                btn: 0
            }]
        );
        assert_eq!(parse(&[b"\x1b[", b"A"]), vec![Ev::Key(Key::Up)]);
        let utf = "◆".as_bytes();
        assert_eq!(
            parse(&[&utf[..1], &utf[1..]]),
            vec![Ev::Key(Key::Char('◆'))]
        );
    }

    #[test]
    fn replies_are_ignored() {
        let r: &[&[u8]] = &[
            b"\x1bP>|kitty(0.36.4)\x1b\\",
            b"\x1b_Gi=31;OK\x1b\\",
            b"\x1b[6;21;10t\x1b[4;840;1700t",
            b"\x1bP1$r0;48:2:1:2:3m\x1b\\",
            b"\x1b[?62;4;22c",
            b"\x1b]11;rgb:0000/0000/0000\x07",
        ];
        assert_eq!(parse(r), vec![]);
        // a reply split across reads, followed by a key
        assert_eq!(
            parse(&[b"\x1bP>|Windows Term", b"inal\x1b", b"\\q"]),
            vec![Ev::Key(Key::Char('q'))]
        );
    }

    #[test]
    fn lone_escape() {
        let mut p = Parser::default();
        let mut out = Vec::new();
        p.feed(b"\x1b", &mut out);
        assert!(out.is_empty() && p.pending());
        p.flush_timeout(&mut out);
        assert_eq!(out, vec![Ev::Key(Key::Esc)]);
        // an unfinished DCS reply is not turned into Esc
        let mut out = Vec::new();
        p.feed(b"\x1bP1$r0;48", &mut out);
        p.flush_timeout(&mut out);
        assert!(out.is_empty() && p.pending());
        p.feed(b"m\x1b\\x", &mut out);
        assert_eq!(out, vec![Ev::Key(Key::Char('x'))]);
    }
}
