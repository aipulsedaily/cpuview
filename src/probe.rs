//! Terminal capability probe and image-mode selection (port of docs/reference/termgfx.py).

use crate::mosaic::Glyphs;
use std::os::raw::c_int;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Kitty,
    Sixel,
    Iterm,
    Blocks,
    Blocks256,
    Terminal,
    Off,
}

pub const MODES: [Mode; 8] = [
    Mode::Auto,
    Mode::Kitty,
    Mode::Sixel,
    Mode::Iterm,
    Mode::Blocks,
    Mode::Blocks256,
    Mode::Terminal,
    Mode::Off,
];

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Kitty => "kitty",
            Mode::Sixel => "sixel",
            Mode::Iterm => "iterm",
            Mode::Blocks => "blocks",
            Mode::Blocks256 => "blocks256",
            Mode::Terminal => "terminal",
            Mode::Off => "off",
        }
    }
    pub fn parse(s: &str) -> Option<Mode> {
        MODES.iter().copied().find(|m| m.name() == s)
    }
    pub fn pixels(self) -> bool {
        matches!(self, Mode::Kitty | Mode::Sixel | Mode::Iterm)
    }
}

#[derive(Default, Clone, Debug)]
pub struct Caps {
    pub name: String,
    pub kitty: bool,
    pub sixel: bool,
    pub da1: Vec<u32>,
    pub cell_px: Option<(u32, u32)>,
    pub window_px: Option<(u32, u32)>,
    pub truecolor_echo: bool,
    pub answered: bool,
    pub raw: Vec<u8>,
    pub ms: u32,
}

const ESC: &str = "\x1b";
pub const KITTY_QUERY: &str = "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";

pub fn queries(tmux: bool) -> String {
    let kq = if tmux {
        tmux_wrap(KITTY_QUERY)
    } else {
        KITTY_QUERY.to_string()
    };
    format!("{ESC}[>0q{kq}{ESC}[16t{ESC}[14t{ESC}[48;2;1;2;3m{ESC}P$qm{ESC}\\{ESC}[0m{ESC}[c")
}

pub fn tmux_wrap(seq: &str) -> String {
    format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// Content of a string sequence starting with `start` and ending with ST.
fn string_after<'a>(buf: &'a [u8], start: &[u8]) -> Option<&'a [u8]> {
    let i = find(buf, start)? + start.len();
    let j = find(&buf[i..], b"\x1b\\")?;
    Some(&buf[i..i + j])
}

fn nums(b: &[u8]) -> Vec<u32> {
    b.split(|&c| c == b';')
        .filter(|p| !p.is_empty())
        .map(|p| {
            std::str::from_utf8(p)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        })
        .collect()
}

/// CSI <prefix> digits/semicolons <fin>
fn csi_params(buf: &[u8], prefix: &[u8], fin: u8) -> Option<Vec<u32>> {
    let mut from = 0;
    while let Some(p) = find(&buf[from..], prefix) {
        let st = from + p + prefix.len();
        let mut j = st;
        while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
            j += 1;
        }
        if j < buf.len() && buf[j] == fin {
            return Some(nums(&buf[st..j]));
        }
        from = st;
    }
    None
}

pub fn parse_replies(buf: &[u8]) -> Caps {
    let mut c = Caps {
        raw: buf.to_vec(),
        ..Default::default()
    };
    if let Some(n) = string_after(buf, b"\x1bP>|") {
        c.name = String::from_utf8_lossy(n).into_owned();
    }
    if let Some(k) = string_after(buf, b"\x1b_Gi=31;") {
        c.kitty = k.starts_with(b"OK");
    }
    if let Some(p) = csi_params(buf, b"\x1b[6;", b't') {
        if let [h, w] = p[..] {
            c.cell_px = (w > 0 && h > 0).then_some((w, h));
        }
    }
    if let Some(p) = csi_params(buf, b"\x1b[4;", b't') {
        if let [h, w] = p[..] {
            c.window_px = (w > 0 && h > 0).then_some((w, h));
        }
    }
    if let Some(s) = string_after(buf, b"\x1bP1$r") {
        let mut norm = String::new();
        for ch in String::from_utf8_lossy(s).chars() {
            let ch = if ch == ':' { ';' } else { ch };
            if !(ch == ';' && norm.ends_with(';')) {
                norm.push(ch);
            }
        }
        c.truecolor_echo = norm.contains("48;2;1;2;3");
    }
    if let Some(p) = csi_params(buf, b"\x1b[?", b'c') {
        c.answered = true;
        c.sixel = p.iter().skip(1).any(|&v| v == 4);
        c.da1 = p;
    }
    c
}

// WezTerm speaks the kitty protocol but text written over an image erases it there
// (wezterm#7222): it gets sixel, which it handles like the other sixel terminals
const KITTY_UNDER_TEXT: [&str; 2] = ["kitty", "ghostty"];
const ITERM_INLINE: [&str; 4] = ["iterm2", "wezterm", "mintty", "vscode"];
const MODERN: [&str; 15] = [
    "kitty",
    "ghostty",
    "wezterm",
    "foot",
    "konsole",
    "iterm2",
    "alacritty",
    "contour",
    "mintty",
    "xterm",
    "vscode",
    "tabby",
    "rio",
    "windows terminal",
    "tmux",
];

pub struct Env {
    pub term: String,
    pub colorterm: String,
    pub tmux: bool,
}

impl Env {
    pub fn get() -> Env {
        let g = |k: &str| std::env::var(k).unwrap_or_default();
        Env {
            term: g("TERM"),
            colorterm: g("COLORTERM").to_ascii_lowercase(),
            tmux: !g("TMUX").is_empty(),
        }
    }
}

pub fn in_tmux(c: &Caps, e: &Env) -> bool {
    e.tmux || c.name.to_ascii_lowercase().starts_with("tmux")
}

/// Is the terminal truecolor? (COLORTERM, DECRQSS echo, or a known modern terminal name)
pub fn truecolor(c: &Caps, e: &Env) -> bool {
    let name = c.name.to_ascii_lowercase();
    e.colorterm == "truecolor"
        || e.colorterm == "24bit"
        || c.truecolor_echo
        || MODERN.iter().any(|n| name.contains(n))
}

pub fn choose_mode(c: &Caps, e: &Env, req: Mode) -> (Mode, String) {
    let name = c.name.to_ascii_lowercase();
    let tmux = in_tmux(c, e);
    let screen = e.term.starts_with("screen") && !tmux;
    let colorterm = e.colorterm == "truecolor" || e.colorterm == "24bit";
    let tc = truecolor(c, e);
    let c256 = tc || e.term.contains("256color");
    if req != Mode::Auto {
        if req.pixels() && screen {
            return (
                if tc { Mode::Blocks } else { Mode::Blocks256 },
                format!("{} is not possible inside GNU screen", req.name()),
            );
        }
        return (req, "requested explicitly".into());
    }
    if !c.answered {
        if tc {
            return (
                Mode::Blocks,
                "terminal did not answer queries; COLORTERM says truecolor".into(),
            );
        }
        return if c256 {
            (Mode::Blocks256, "no replies; TERM says 256 colours".into())
        } else {
            (Mode::Off, "no replies, basic TERM".into())
        };
    }
    let label = if c.name.is_empty() {
        "terminal".to_string()
    } else {
        c.name.clone()
    };
    if c.kitty && !tmux && KITTY_UNDER_TEXT.iter().any(|n| name.contains(n)) {
        return (
            Mode::Kitty,
            format!("kitty graphics OK and {label} layers images under text"),
        );
    }
    if c.sixel {
        let mut why = "DA1 lists 4 (sixel)".to_string();
        if tmux {
            why += " — tmux renders sixel itself";
        }
        return (Mode::Sixel, why);
    }
    if c.kitty && !tmux {
        return (Mode::Kitty, "kitty graphics OK (no sixel)".into());
    }
    if ITERM_INLINE.iter().any(|n| name.contains(n)) && !tmux {
        return (
            Mode::Iterm,
            format!("{} supports iTerm2 inline images", c.name),
        );
    }
    if tc {
        let src = if colorterm {
            "COLORTERM".to_string()
        } else if c.truecolor_echo {
            "DECRQSS echo".to_string()
        } else {
            c.name.clone()
        };
        return (
            Mode::Blocks,
            format!("no pixel graphics; truecolor via {src}"),
        );
    }
    if c256 {
        return (Mode::Blocks256, "no pixel graphics; 256 colours".into());
    }
    (Mode::Off, "no graphics and fewer than 256 colours".into())
}

/// Windows Terminal (and conhost) answer DA1 with this list (4 = sixel, 52 = clipboard
/// are optional) and don't answer XTVERSION.
pub fn windows_terminal(c: &Caps) -> bool {
    c.name.is_empty()
        && c.da1.first() == Some(&61)
        && [6, 7, 14, 21, 22, 23, 24, 28, 32, 42]
            .iter()
            .all(|v| c.da1.contains(v))
}

/// First number in a version string ("kitty(0.40.1)" → 0.40, "WezTerm 20250208-…" →
/// 20250208), as (major, minor).
fn version(name: &str) -> (u32, u32) {
    let s = name.trim_start_matches(|ch: char| !ch.is_ascii_digit());
    let mut it = s
        .split(|ch: char| !ch.is_ascii_digit())
        .map(|p| p.parse::<u32>().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0))
}

/// Block glyphs for pictures drawn with characters: the finest set this terminal
/// draws itself (pixel-exact, whatever the font), else one its fonts are known to have.
pub fn choose_glyphs(c: &Caps, e: &Env, req: Option<Glyphs>) -> (Glyphs, String) {
    if let Some(g) = req {
        return (g, "requested explicitly".into());
    }
    let n = c.name.to_ascii_lowercase();
    let v = version(&n);
    let at_least = |a: (u32, u32)| v >= a;
    let (g, why) = if n.starts_with("kitty") {
        if at_least((0, 40)) {
            (Glyphs::Octant, "kitty ≥ 0.40 draws octants")
        } else {
            (Glyphs::Sextant, "kitty draws sextants")
        }
    } else if n.starts_with("foot") {
        if at_least((1, 20)) {
            (Glyphs::Octant, "foot ≥ 1.20 draws octants")
        } else {
            (Glyphs::Sextant, "foot draws sextants")
        }
    } else if n.starts_with("ghostty") {
        (Glyphs::Octant, "Ghostty draws octants")
    } else if n.starts_with("wezterm") {
        if at_least((20250208, 0)) {
            (Glyphs::Octant, "WezTerm (2025 builds) draws octants")
        } else {
            (Glyphs::Sextant, "WezTerm draws sextants")
        }
    } else if n.starts_with("vte") {
        if at_least((7800, 0)) {
            (Glyphs::Octant, "VTE ≥ 0.78 draws octants")
        } else {
            (Glyphs::Sextant, "VTE draws sextants")
        }
    } else if n.starts_with("konsole") {
        (Glyphs::Sextant, "Konsole draws sextants")
    } else if windows_terminal(c) {
        // 1.25+ draws U+1FB00..1FB94 itself; older versions take them from the bundled
        // Cascadia font. Octants would come from the font only.
        (Glyphs::Sextant, "Windows Terminal draws sextants")
    } else if c.answered || e.colorterm == "truecolor" || e.colorterm == "24bit" {
        (
            Glyphs::Quadrant,
            "quadrants: in every common monospace font",
        )
    } else {
        (Glyphs::Half, "half blocks: unknown terminal")
    };
    (g, why.into())
}

/// Cell size in pixels: CSI 16 t, CSI 14 t / grid, TIOCGWINSZ pixels / grid,
/// else 10×20 when sixel is available (VT340 cell, Windows Terminal's sixel cell).
pub fn cell_size(
    c: &Caps,
    cols: u16,
    rows: u16,
    xp: u16,
    yp: u16,
) -> Option<((u32, u32), &'static str)> {
    if let Some(p) = c.cell_px {
        return Some((p, "CSI 16 t"));
    }
    if let (Some((w, h)), true) = (c.window_px, cols > 0 && rows > 0) {
        if w / cols as u32 > 0 && h / rows as u32 > 0 {
            return Some(((w / cols as u32, h / rows as u32), "CSI 14 t / grid"));
        }
    }
    if xp > 0 && yp > 0 && cols > 0 && rows > 0 && xp / cols > 0 && yp / rows > 0 {
        return Some((
            ((xp / cols) as u32, (yp / rows) as u32),
            "TIOCGWINSZ pixels / grid",
        ));
    }
    if c.sixel {
        return Some((
            (10, 20),
            "sixel default (VT340 / Windows Terminal virtual cell)",
        ));
    }
    None
}

/// Query the terminal on `fd` (already in raw mode). Writes all queries at once, reads
/// until the DA1 reply or the timeout. Returns caps; the raw bytes (replies plus any keys
/// typed meanwhile) are in `caps.raw` for the input parser, which ignores the replies.
pub fn probe(fd_in: c_int, fd_out: c_int, tmux: bool) -> Caps {
    let base_ms: i64 = std::env::var("CPUVIEW_PROBE_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let t0 = crate::term::mono();
    crate::term::write_all(fd_out, queries(tmux).as_bytes());
    let mut end = t0 + base_ms * 1_000_000;
    let mut buf = Vec::with_capacity(512);
    let mut tmp = [0u8; 1024];
    loop {
        let now = crate::term::mono();
        if now >= end {
            break;
        }
        let mut p = libc::pollfd {
            fd: fd_in,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = ((end - now) / 1_000_000).max(1) as c_int;
        let r = unsafe { libc::poll(&mut p, 1, ms) };
        if r <= 0 {
            if r < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
        let n = crate::term::read(fd_in, &mut tmp);
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n as usize]);
        if csi_params(&buf, b"\x1b[?", b'c').is_some() {
            break;
        }
        // replies have started (slow link): allow up to 1 s for the DA1 sentinel
        end = end.max(t0 + base_ms.max(1000) * 1_000_000);
    }
    let mut c = parse_replies(&buf);
    c.ms = ((crate::term::mono() - t0) / 1_000_000) as u32;
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLES: [(&str, &[u8], Mode); 13] = [
        (
            "kitty",
            b"\x1bP>|kitty(0.36.4)\x1b\\\x1b_Gi=31;OK\x1b\\\x1b[6;21;10t\x1b[4;840;1700t\x1bP1$r0;48:2:1:2:3m\x1b\\\x1b[?62;c",
            Mode::Kitty,
        ),
        ("windows-terminal-1.22", b"\x1bP1$r0;48;2;1;2;3m\x1b\\\x1b[?61;4;6;7;14;21;22;23;24;28;32;42;52c", Mode::Sixel),
        ("conhost (DA1 only)", b"\x1b[?1;0c", Mode::Blocks256),
        ("windows-terminal-1.21", b"\x1bP1$r0;48;2;1;2;3m\x1b\\\x1b[?61;6;7;14;21;22;23;24;28;32;42c", Mode::Blocks),
        ("foot", b"\x1bP>|foot(1.18.1)\x1b\\\x1b[6;22;11t\x1bP1$r0;48:2::1:2:3m\x1b\\\x1b[?62;4;22c", Mode::Sixel),
        (
            "wezterm",
            b"\x1bP>|WezTerm 20240203-110809-5046fc22\x1b\\\x1b_Gi=31;OK\x1b\\\x1b[6;20;10t\x1b[?65;4;6;18;22c",
            Mode::Sixel,
        ),
        ("konsole", b"\x1bP>|Konsole 24.02.1\x1b\\\x1b_Gi=31;OK\x1b\\\x1b[?62;4;22c", Mode::Sixel),
        ("iterm2", b"\x1bP>|iTerm2 3.5.0\x1b\\\x1b[?62;22;52c", Mode::Iterm),
        ("tmux-no-sixel", b"\x1bP>|tmux 3.4\x1b\\\x1b[?1;2c", Mode::Blocks),
        ("tmux-sixel", b"\x1bP>|tmux 3.4\x1b\\\x1b[?1;2;4c", Mode::Sixel),
        ("xterm-vt340", b"\x1bP>|XTerm(390)\x1b\\\x1b[?63;1;2;4;6;9;15;16;22;28c", Mode::Sixel),
        ("xterm-default", b"\x1bP>|XTerm(390)\x1b\\\x1b[?65;1;9c", Mode::Blocks),
        ("silent", b"", Mode::Blocks256),
    ];

    fn env(tmux: bool) -> Env {
        Env {
            term: "xterm-256color".into(),
            colorterm: String::new(),
            tmux,
        }
    }

    #[test]
    fn selection_matrix() {
        for (name, buf, want) in SAMPLES {
            let c = parse_replies(buf);
            let (got, why) = choose_mode(&c, &env(name.starts_with("tmux")), Mode::Auto);
            assert_eq!(got, want, "{name}: {why}");
        }
    }

    #[test]
    fn reply_details() {
        let c = parse_replies(SAMPLES[0].1);
        assert_eq!(c.cell_px, Some((10, 21)));
        assert_eq!(c.window_px, Some((1700, 840)));
        assert!(c.truecolor_echo && c.kitty && c.answered);
        assert_eq!(c.name, "kitty(0.36.4)");
        let wt = parse_replies(SAMPLES[1].1);
        assert_eq!(cell_size(&wt, 172, 40, 0, 0).unwrap().0, (10, 20));
        // CSI 14t / grid fallback, then ioctl pixels
        let c = Caps {
            window_px: Some((1720, 800)),
            ..Default::default()
        };
        assert_eq!(cell_size(&c, 172, 40, 0, 0).unwrap().0, (10, 20));
        assert_eq!(
            cell_size(&Caps::default(), 100, 50, 900, 1000).unwrap().0,
            (9, 20)
        );
        assert!(cell_size(&Caps::default(), 100, 50, 0, 0).is_none());
    }

    #[test]
    fn edge_cases() {
        let tc = Env {
            term: "xterm-256color".into(),
            colorterm: "truecolor".into(),
            tmux: false,
        };
        assert_eq!(
            choose_mode(&Caps::default(), &tc, Mode::Auto).0,
            Mode::Blocks
        );
        let vt = Env {
            term: "vt100".into(),
            colorterm: String::new(),
            tmux: false,
        };
        assert_eq!(
            choose_mode(
                &Caps {
                    answered: true,
                    ..Default::default()
                },
                &vt,
                Mode::Auto
            )
            .0,
            Mode::Off
        );
        let scr = Env {
            term: "screen".into(),
            colorterm: String::new(),
            tmux: false,
        };
        let c = Caps {
            answered: true,
            sixel: true,
            ..Default::default()
        };
        assert_ne!(choose_mode(&c, &scr, Mode::Sixel).0, Mode::Sixel);
        assert_eq!(
            choose_mode(&c, &env(false), Mode::Terminal).0,
            Mode::Terminal
        );
        assert_eq!(
            tmux_wrap("\x1b_Ga=d\x1b\\"),
            "\x1bPtmux;\x1b\x1b_Ga=d\x1b\x1b\\\x1b\\"
        );
        // kitty answers inside tmux are not trusted for auto
        let k = parse_replies(b"\x1bP>|tmux 3.4\x1b\\\x1b_Gi=31;OK\x1b\\\x1b[?1;2c");
        assert_eq!(choose_mode(&k, &env(true), Mode::Auto).0, Mode::Blocks);
        // DECRPM-like CSI ? … y must not count as DA1
        assert!(!parse_replies(b"\x1b[?2026;2$y").answered);
    }

    #[test]
    fn glyph_sets() {
        let g = |buf: &[u8]| choose_glyphs(&parse_replies(buf), &env(false), None).0;
        assert_eq!(g(b"\x1bP>|kitty(0.40.1)\x1b\\\x1b[?62;c"), Glyphs::Octant);
        assert_eq!(g(b"\x1bP>|kitty(0.36.4)\x1b\\\x1b[?62;c"), Glyphs::Sextant);
        assert_eq!(
            g(b"\x1bP>|WezTerm 20240203-110809-5046fc22\x1b\\\x1b[?65;4;6c"),
            Glyphs::Sextant
        );
        assert_eq!(
            g(b"\x1bP>|foot(1.21.0)\x1b\\\x1b[?62;4;22c"),
            Glyphs::Octant
        );
        // Windows Terminal: no XTVERSION, its DA1 fingerprint
        let wt = b"\x1b[6;20;10t\x1b[4;900;1280t\x1bP1$r0;48:2::1:2:3m\x1b\\\x1b[?61;4;6;7;14;21;22;23;24;28;32;42;52c";
        assert!(windows_terminal(&parse_replies(wt)));
        assert_eq!(g(wt), Glyphs::Sextant);
        assert_eq!(g(b"\x1bP>|XTerm(390)\x1b\\\x1b[?65;1;9c"), Glyphs::Quadrant);
        assert_eq!(g(b""), Glyphs::Half);
        let c = parse_replies(wt);
        assert_eq!(
            choose_glyphs(&c, &env(false), Some(Glyphs::Half)).0,
            Glyphs::Half
        );
        assert_eq!(version("wezterm 20250208-1"), (20250208, 1));
    }
}
