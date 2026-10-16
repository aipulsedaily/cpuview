//! Theme definitions (embedded from themes/*.toml by build.rs) and the runtime Theme.

use crate::color::{hex, Ramp, Rgb};

/// A TOML value embedded at build time.
#[derive(Debug)]
pub enum P {
    N(f64),
    S(&'static str),
    B(bool),
    A(&'static [P]),
    T(&'static [(&'static str, P)]),
}

include!(concat!(env!("OUT_DIR"), "/themes_gen.rs"));

impl P {
    pub fn get(&self, k: &str) -> Option<&'static P> {
        match self {
            P::T(kv) => kv.iter().find(|(n, _)| *n == k).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn n(&self, k: &str, d: f64) -> f64 {
        match self.get(k) {
            Some(P::N(v)) => *v,
            Some(P::B(b)) => *b as u8 as f64,
            _ => d,
        }
    }
    pub fn s(&self, k: &str) -> Option<&'static str> {
        match self.get(k) {
            Some(P::S(s)) => Some(s),
            _ => None,
        }
    }
    pub fn b(&self, k: &str) -> bool {
        matches!(self.get(k), Some(P::B(true)))
    }
    pub fn arr(&self, k: &str) -> &'static [P] {
        match self.get(k) {
            Some(P::A(a)) => a,
            _ => &[],
        }
    }
    pub fn as_n(&self) -> f64 {
        match self {
            P::N(v) => *v,
            _ => 0.0,
        }
    }
    pub fn as_s(&self) -> &'static str {
        match self {
            P::S(s) => s,
            _ => "",
        }
    }
    /// [u, v] pair with default.
    pub fn pair(&self, k: &str, d: (f64, f64)) -> (f64, f64) {
        match self.arr(k) {
            [a, b] => (a.as_n(), b.as_n()),
            _ => d,
        }
    }
}

/// Gradient stops: a list of colours (evenly spaced) or of [pos, colour] pairs.
pub fn stops(a: &[P]) -> Vec<(f32, Rgb)> {
    let n = a.len().max(2) - 1;
    a.iter()
        .enumerate()
        .map(|(i, p)| match p {
            P::S(s) => (i as f32 / n as f32, hex(s)),
            P::A([pos, c]) => (pos.as_n() as f32, hex(c.as_s())),
            _ => (0.0, 0),
        })
        .collect()
}

pub const S_SCREEN: u8 = 1;
pub const S_BAR: u8 = 2;
pub const S_PANEL: u8 = 3;
pub const S_SELECT: u8 = 4;
pub const S_CHIP: u8 = 5;
pub const S_DIALOG: u8 = 6;
pub const S_TRACK: u8 = 7;
pub const SURF_NAMES: [&str; 8] = [
    "", "screen", "bar", "panel", "select", "chip", "dialog", "track",
];

#[derive(Clone, Copy, Debug)]
pub struct Surf {
    pub tint: Rgb,
    pub alpha: f32,
    pub text_alpha: f32,
}

#[derive(Clone)]
pub struct Theme {
    pub id: String,
    pub label: String,
    pub desc: String,
    pub transparent: bool,
    pub bg: Rgb,
    pub panel: Rgb,
    pub bar: Rgb,
    pub select: Rgb,
    pub chip: Rgb,
    pub track: Rgb,
    pub border: Rgb,
    pub border_hi: Rgb,
    pub title: Rgb,
    pub accent: Rgb,
    pub accent2: Rgb,
    pub fg: Rgb,
    pub text: Rgb,
    pub mid: Rgb,
    pub dim: Rgb,
    pub good: Rgb,
    pub warn: Rgb,
    pub crit: Rgb,
    pub freq: Ramp,
    pub load: Ramp,
    pub temp: Ramp,
    pub freq_idle: Ramp,
    /// load ramp faded toward the panel: the part of a core's bar only one thread reaches
    pub load_one: Ramp,
    pub brightness: f32,
    pub min_contrast: f32,
    pub surf: [Surf; 8],
    /// Procedural background layers (built-in themes).
    pub layers: &'static [P],
    /// Image theme: path of the picture.
    pub image: Option<String>,
    /// Image themes: blur radius (cells) used under panels.
    pub soft_blur: f32,
}

impl Theme {
    pub fn solid(&self, s: u8) -> Rgb {
        match s {
            S_SCREEN => self.bg,
            S_BAR => self.bar,
            S_PANEL => self.panel,
            S_SELECT => self.select,
            S_CHIP => self.chip,
            S_DIALOG => self.panel,
            S_TRACK => self.track,
            _ => self.bg,
        }
    }
    pub fn swatch(&self) -> [Rgb; 8] {
        let mut s = [0; 8];
        for (i, v) in s.iter_mut().enumerate() {
            *v = if i < 4 {
                self.freq.at(i as f32 / 3.0)
            } else {
                self.load.at((i - 4) as f32 / 3.0)
            };
        }
        s
    }
}

pub fn builtin_ids() -> Vec<&'static str> {
    // default first, then by label, transparent last (menu order)
    let mut v: Vec<&P> = THEMES.iter().collect();
    let key = |t: &&P| {
        let n = t.s("name").unwrap_or("");
        (
            n != "default",
            n == "transparent",
            t.s("label").unwrap_or(n),
        )
    };
    crate::sys::isort(&mut v, &|a, b| key(a) < key(b));
    v.iter().map(|t| t.s("name").unwrap_or("")).collect()
}

pub fn find(name: &str) -> Option<&'static P> {
    THEMES.iter().find(|t| t.s("name") == Some(name))
}

fn ramp_of(pal: &P, k: &str, fallback: &[&str]) -> Ramp {
    let a = pal.arr(k);
    if a.is_empty() {
        Ramp::even(&fallback.iter().map(|s| hex(s)).collect::<Vec<_>>())
    } else {
        Ramp::new(&stops(a))
    }
}

pub fn load(name: &str) -> Option<Theme> {
    let t = find(name)?;
    let pal = t.get("palette").unwrap_or(&P::T(&[]));
    let c = |k: &str, d: &str| hex(pal.s(k).unwrap_or(d));
    let blend = t.get("blend").unwrap_or(&P::T(&[]));
    let panel = c("panel", "#0e121b");
    let mut surf = [Surf {
        tint: panel,
        alpha: 0.6,
        text_alpha: 0.8,
    }; 8];
    let def = blend.get("panel");
    for (i, nm) in SURF_NAMES.iter().enumerate().skip(1) {
        let d = blend.get(nm).or(def);
        if let Some(d) = d {
            let a = d.n("alpha", 0.6) as f32;
            surf[i] = Surf {
                tint: hex(d.s("tint").unwrap_or("#000000")),
                alpha: a,
                text_alpha: d.n("text_alpha", (a + 0.2).min(1.0) as f64) as f32,
            };
        }
    }
    let freq = ramp_of(pal, "freq", &["#3d59a1", "#7aa2f7", "#bb9af7", "#ff7ac6"]);
    Some(Theme {
        id: name.to_string(),
        label: t.s("label").unwrap_or(name).to_string(),
        desc: t.s("description").unwrap_or("").to_string(),
        transparent: t.b("transparent"),
        bg: c("bg", "#0a0d14"),
        panel,
        bar: c("bar", "#0c0f17"),
        select: c("select", "#18213a"),
        chip: c("chip", "#1b2440"),
        track: c("track", "#1a2030"),
        border: c("border", "#28304a"),
        border_hi: c("border_hi", "#4a5b8c"),
        title: c("title", "#dfe5ff"),
        accent: c("accent", "#7aa2f7"),
        accent2: c("accent2", "#bb9af7"),
        fg: c("fg", "#d6dcf5"),
        text: c("text", "#aeb6da"),
        mid: c("mid", "#7d86ad"),
        dim: c("dim", "#525a7c"),
        good: c("good", "#9ece6a"),
        warn: c("warn", "#e0af68"),
        crit: c("crit", "#f7768e"),
        freq_idle: freq.shade(panel, 0.5),
        freq,
        load_one: ramp_of(pal, "load", &["#9ece6a", "#e0c068", "#ff9e64", "#f7768e"])
            .shade(panel, 0.45),
        load: ramp_of(pal, "load", &["#9ece6a", "#e0c068", "#ff9e64", "#f7768e"]),
        temp: ramp_of(pal, "temp", &["#7dcfff", "#9ece6a", "#e0af68", "#f7768e"]),
        brightness: blend.n("brightness", 1.0) as f32,
        min_contrast: blend.n("min_contrast", 4.5) as f32,
        surf,
        layers: t.arr("background"),
        image: None,
        soft_blur: 0.0,
    })
}

/// Recompute derived colours after palette edits (image themes).
pub fn refresh_derived(t: &mut Theme) {
    t.freq_idle = t.freq.shade(t.panel, 0.5);
    t.load_one = t.load.shade(t.panel, 0.45);
}

/// Credit line for a bundled wallpaper file name.
pub fn credit(file: &str) -> Option<(&'static str, &'static str)> {
    let e = CREDITS.get(file)?;
    Some((e.s("title").unwrap_or(""), e.s("credit").unwrap_or("")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_builtins_load() {
        let ids = builtin_ids();
        assert_eq!(ids.len(), 10);
        assert_eq!(ids[0], "default");
        assert_eq!(*ids.last().unwrap(), "transparent");
        for id in ids {
            let t = load(id).unwrap();
            assert!(!t.label.is_empty());
            assert!(t.transparent == (id == "transparent"));
        }
        let d = load("default").unwrap();
        assert_eq!(d.bg, 0x0a0d14);
        assert!((d.surf[S_PANEL as usize].alpha - 0.35).abs() < 1e-6);
        assert_eq!(
            credit("nasa-iss-aurora.jpg").unwrap().1,
            "NASA / ISS Expedition 23 crew · public domain"
        );
    }
}
