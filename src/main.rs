//! cpuview — btop-style per-core CPU clock / load / temperature monitor.

mod anim;
mod bg;
mod color;
mod config;
mod gfx;
mod image;
mod input;
mod mem;
mod menu;
mod net;
mod probe;
mod procs;
mod prof;
mod screen;
mod sensors;
mod sys;
mod term;
mod theme;
mod ui;

use color::{mix, Ramp};
use config::Config;
use input::{Ev, Key};
use probe::{Caps, Env, Mode};
use screen::*;
use std::sync::atomic::Ordering;
use theme::*;
use ui::App;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HELP: &str = "cpuview — live per-core CPU clock, load and temperature monitor, with an
overview page (everything at a glance: what needs attention, CPU, RAM, disks,
network, the busiest and biggest programs, sensors), a memory & storage page (RAM, swap, disk I/O, filesystems, biggest processes), a
network & system page (link speeds, ports, connections, containers, services), a
processes page (every process: CPU, memory, disk, tree, groups, signals) and a
sensors & power page (temperatures by part, fans, CPU / GPU / battery watts, voltages,
and the drivers they come from)

usage: cpuview [options]

  --theme NAME          theme: default, aurora, ember, forest, mono, nebula, ocean,
                        space, synthwave, transparent, or image:<file>
  --image PATH|URL      use a picture as the background (copied / downloaded into
                        ~/.config/cpuview/images/ and remembered)
  --image-mode MODE     auto | kitty | sixel | iterm | blocks | blocks256 | terminal | off
  --speed SPEED         eco (default: adaptive 1-2 s, ≤ 0.01 % of one thread),
                        smooth (10 samples/s, everything animated at a steady
                        60 fps, ≤ 0.5 %), or updates per second: 1, 2, 4, 10,
                        20, 30, 60
  --page PAGE           start on: overview (the default: every page's answer on
                        one screen), cpu, memory (RAM, swap, disks, filesystems),
                        network (links, ports, containers, services), processes,
                        or sensors (temperatures, fans, power, voltages)
  --setup-sensors       make fans and CPU power readable: shows what it would change
                        and why, asks, then uses sudo. Loads the board's fan driver
                        (if missing) now and at boot; makes the RAPL energy counters
                        readable with a udev rule. Options: --per-core (per-core
                        power: cap_perfmon on this binary, redo after reinstalling),
                        --undo (reverse it all), --dry-run (only show the plan)
  --no-probe            don't query the terminal (assume what TERM/COLORTERM say)
  --doctor              show what the terminal supports and which image mode is used
  -h, --help            this help
  -V, --version         version

keys: 0 1 2 3 4 5 or tab / shift-tab pages (overview, cpu, memory, network,
      processes, sensors); on the overview click a panel to open its page
      arrows/hjkl/click select · s speed (+ faster, - slower) · m memory & storage
      o sort · c view · g graphs · d/enter detail · r reset peaks · space pause
      t themes · q quit
processes page: arrows/pgup/pgdn/home/end/click select · o sort (cpu, mem,
      disk, pid, name) · r reverse · / filter (esc clears) · T tree · G group by
      program · enter/←→ fold · k or F9 signal · n nice ·
      I CPU % of one CPU / all · K kernel threads
";

const OUT: i32 = 1;

use prof::{prof, prof_count};

struct Gfx {
    req: Mode,
    mode: Mode,
    why: String,
    caps: Caps,
    env: Env,
    truecolor: bool,
    cell: (u32, u32),
    cell_src: &'static str,
    bd: Option<Backdrop>,
    comp: Comp,
    depth: Depth,
    kitty_key: Vec<usize>,
    kitty_live: bool,
}

impl Gfx {
    fn resolve(&mut self) {
        let (m, why) = probe::choose_mode(&self.caps, &self.env, self.req);
        self.mode = m;
        self.why = why;
        self.depth = if self.mode == Mode::Blocks256
            || (!self.truecolor && !self.mode.pixels() && self.mode != Mode::Blocks)
        {
            Depth::C256
        } else {
            Depth::True
        };
    }
    fn tmux(&self) -> bool {
        probe::in_tmux(&self.caps, &self.env)
    }
    fn label(&self) -> String {
        let name = if self.caps.name.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.caps.name)
        };
        if self.req == Mode::Auto {
            format!("auto → {}{}", self.mode.name(), name)
        } else {
            format!("{}{}", self.mode.name(), name)
        }
    }
}

fn image_theme(file: &str, img: &image::Img, opt: &config::ImgOpt) -> Theme {
    let mut t = theme::load("default").unwrap();
    let p = image::palette_from(img);
    let (tint, a1, a2, text) = (p.tint, p.a1, p.a2, p.text);
    t.id = format!("image:{file}");
    t.label = file
        .rsplit_once('.')
        .map(|(s, _)| s.to_string())
        .unwrap_or(file.to_string());
    t.desc = "image · palette from picture".into();
    t.bg = tint;
    t.panel = mix(tint, a1, 0.04);
    t.bar = mix(tint, 0, 0.3);
    t.select = mix(tint, a1, 0.22);
    t.chip = mix(tint, a1, 0.18);
    t.track = mix(tint, a1, 0.12);
    t.border = mix(tint, a1, 0.35);
    t.border_hi = mix(tint, a1, 0.7);
    t.title = text;
    t.accent = a1;
    t.accent2 = a2;
    t.fg = text;
    t.text = mix(text, tint, 0.18);
    t.mid = mix(text, tint, 0.42);
    t.dim = mix(text, tint, 0.62);
    t.freq = Ramp::even(&[image::with_value(a2, 0.45), a2, a1, mix(a1, 0xffffff, 0.6)]);
    t.brightness = opt.dim;
    // calmer than the reference behind dense data: panels use a blurred copy of the
    // picture (see Backdrop::soft) and a slightly denser plate
    let s = |tint, alpha, text_alpha| Surf {
        tint,
        alpha,
        text_alpha,
    };
    // blank and text alpha are close, so text plates don't show as dark boxes over
    // the (pixel-patched) picture; the contrast guard darkens only where needed
    t.surf[S_SCREEN as usize] = s(tint, 0.25, 0.5);
    t.surf[S_BAR as usize] = s(tint, 0.6, 0.7);
    t.surf[S_PANEL as usize] = s(tint, 0.52, 0.6);
    t.surf[S_SELECT as usize] = s(t.select, 0.68, 0.74);
    t.surf[S_CHIP as usize] = s(t.chip, 0.8, 0.85);
    t.surf[S_TRACK as usize] = s(t.track, 0.7, 0.8);
    t.surf[S_DIALOG as usize] = s(tint, 0.84, 0.88);
    t.image = Some(file.to_string());
    t.soft_blur = opt.blur;
    t.layers = &[];
    refresh_derived(&mut t);
    t
}

/// Load a theme by id. Image themes also return the decoded picture so the backdrop
/// can be built without decoding twice.
fn load_theme(id: &str, cfg: &Config) -> Result<(Theme, Option<image::Img>), String> {
    if let Some(file) = id.strip_prefix("image:") {
        let path = config::resolve_image(file).ok_or_else(|| format!("image not found: {file}"))?;
        let img = image::decode_file(&path.to_string_lossy())?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or(file.to_string());
        let t = image_theme(&name, &img, &cfg.img(&name));
        Ok((t, Some(img)))
    } else {
        theme::load(id)
            .map(|t| (t, None))
            .ok_or_else(|| format!("unknown theme: {id}"))
    }
}

fn trim_heap() {
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Half-block canvas (w × 2h RGB) → per-cell [upper, lower] pixel pairs.
fn to_cells(b: &[u8], w: usize, h: usize) -> Vec<screen::Px2> {
    (0..w * h)
        .map(|i| {
            let o = (i / w * 2 * w + i % w) * 3;
            let p = o + 3 * w;
            [b[o], b[o + 1], b[o + 2], b[p], b[p + 1], b[p + 2]]
        })
        .collect()
}

fn build_backdrop(
    t: &Theme,
    img: Option<image::Img>,
    w: usize,
    h: usize,
    g: &Gfx,
) -> Option<Backdrop> {
    if t.transparent || matches!(g.mode, Mode::Off | Mode::Terminal) || w == 0 || h == 0 {
        return None;
    }
    if let Some(file) = &t.image {
        let img = match img {
            Some(i) => i,
            None => image::decode_file(&config::resolve_image(file)?.to_string_lossy()).ok()?,
        };
        let ub = t.soft_blur;
        let soft_r = 2.0 + ub;
        let mut sharp = image::cover(&img, w, 2 * h);
        if ub > 0.0 {
            image::blur(&mut sharp, w, 2 * h, ub, ub);
        }
        let mut soft = sharp.clone();
        image::blur(&mut soft, w, 2 * h, soft_r, soft_r);
        let mut bd = Backdrop::empty(w, h);
        bd.sharp = to_cells(&sharp, w, h);
        bd.soft = Some(to_cells(&soft, w, h));
        drop((sharp, soft));
        if !g.mode.pixels() {
            // drop the decoded original *before* trimming the heap
            drop(img);
        } else {
            let (cw, ch) = (g.cell.0 as usize, g.cell.1 as usize);
            let (pw, ph) = (w * cw, h * ch);
            let mut hr = image::cover(&img, pw, ph);
            drop(img);
            if ub > 0.0 {
                image::blur(&mut hr, pw, ph, ub * cw as f32, ub * cw as f32);
            }
            let mut hs = hr.clone();
            image::blur(&mut hs, pw, ph, soft_r * cw as f32, soft_r * cw as f32);
            let empty = || image::Indexed {
                w: 0,
                h: 0,
                idx: Vec::new(),
                pal: Vec::new(),
            };
            bd.hires = Some(if g.mode == Mode::Kitty {
                // the blurred layer needs no detail: send it at 1/4 resolution, kitty
                // scales placements to their cell rectangles
                let small = image::stretch(
                    &image::Img {
                        w: pw,
                        h: ph,
                        rgb: hs,
                    },
                    pw / 4,
                    ph / 4,
                );
                Hires {
                    cw,
                    ch,
                    sharp: empty(),
                    soft: empty(),
                    kitty: Some((hr, small)),
                }
            } else {
                let qs = image::quantize(&hr, pw, ph, 256);
                drop(hr);
                let qf = image::quantize(&hs, pw, ph, 128);
                Hires {
                    cw,
                    ch,
                    sharp: qs,
                    soft: qf,
                    kitty: None,
                }
            });
        }
        trim_heap();
        Some(bd)
    } else {
        let cv = bg::render(t.layers, t.bg, w, 2 * h, w, h, false);
        let mut bd = Backdrop::empty(w, h);
        bd.sharp = (0..w * h)
            .map(|i| {
                let o = i / w * 2 * w + i % w;
                screen::px2(color::from_f(cv.px[o]), color::from_f(cv.px[o + w]))
            })
            .collect();
        bd.has_glyphs = cv.glyph.iter().any(|g| g.0 != '\0');
        if bd.has_glyphs {
            bd.glyph = cv.glyph;
        }
        trim_heap();
        Some(bd)
    }
}

fn look_for(t: &Theme, g: &Gfx) -> Look {
    if t.transparent || g.mode == Mode::Terminal {
        Look::Transparent
    } else if g.bd.is_none() {
        Look::Solid
    } else if g.mode == Mode::Kitty && t.image.is_some() {
        Look::KittyGlass
    } else {
        Look::Glass
    }
}

struct State {
    full: bool,
    app: App,
    scr: Screen,
    g: Gfx,
    cfg: Config,
}

impl State {
    /// (Re)build background + compositor for the current theme, size and mode.
    fn rebuild(&mut self, img: Option<image::Img>) {
        self.kitty_clear();
        let (w, h) = (self.app.w, self.app.h);
        let preview = self.app.menu.is_some();
        // while the menu is open, previews use half-blocks (no pixel data per keypress)
        let saved = self.g.mode;
        if preview && self.g.mode.pixels() {
            self.g.mode = Mode::Blocks;
        }
        self.g.bd = None;
        trim_heap();
        self.g.bd = build_backdrop(&self.app.t, img, w, h, &self.g);
        self.g.comp = Comp::new(&self.app.t, look_for(&self.app.t, &self.g));
        self.g.mode = saved;
        self.app.mode_line = self.g.label();
        self.scr.invalidate();
        self.full = true;
    }

    fn patch(&self) -> Patch {
        let pix = self.g.bd.as_ref().is_some_and(|b| b.hires.is_some()) && self.app.menu.is_none();
        match self.g.mode {
            Mode::Sixel if pix => Patch::Sixel,
            Mode::Iterm if pix => Patch::Iterm,
            _ => Patch::None,
        }
    }

    fn kitty_clear(&mut self) {
        if self.g.kitty_live {
            let mut o = Vec::new();
            gfx::kitty_delete(None, &mut o, self.g.tmux());
            term::write_all(OUT, &o);
            self.g.kitty_live = false;
            self.g.kitty_key.clear();
        }
    }

    /// kitty: transmit once, then (re)place picture, blurred panel crops and tint
    /// overlays whenever the panel geometry / selection / menu changes.
    fn kitty_layers(&mut self, out: &mut Vec<u8>) {
        if self.g.comp.look != Look::KittyGlass {
            return;
        }
        let tmux = self.g.tmux();
        let Some(bd) = self.g.bd.as_mut() else { return };
        let Some(hr) = bd.hires.as_mut() else { return };
        let (cw, ch) = (hr.cw, hr.ch);
        let (w, h) = (self.app.w, self.app.h);
        if let Some((sharp, soft)) = hr.kitty.take() {
            gfx::kitty_delete(None, out, tmux);
            gfx::kitty_transmit_rgb(&sharp, w * cw, h * ch, 1, out, tmux);
            gfx::kitty_transmit_rgb(&soft, w * cw / 4, h * ch / 4, 2, out, tmux);
            for sid in 1..8u8 {
                let (tint, a) = self
                    .g
                    .comp
                    .surf_tint(sid, sid == S_PANEL || sid == S_DIALOG);
                let [r, g, b] = color::ch(tint);
                gfx::kitty_transmit_pixel([r, g, b, (a * 255.0) as u8], 10 + sid as u32, out, tmux);
            }
            self.g.kitty_live = true;
            self.g.kitty_key.clear();
            trim_heap();
        }
        // panel rectangles
        let l = &self.app.lay;
        let mut rects: Vec<(u8, ui::Rect)> = vec![
            (
                S_BAR,
                ui::Rect {
                    x: 0,
                    y: 0,
                    w,
                    h: 1,
                },
            ),
            (
                S_BAR,
                ui::Rect {
                    x: 0,
                    y: h - 1,
                    w,
                    h: 1,
                },
            ),
        ];
        if self.app.page == ui::PG_CPU {
            if l.top.is_some() {
                rects.extend([
                    (S_PANEL, l.g_load),
                    (S_PANEL, l.g_freq),
                    (S_PANEL, l.sensors),
                ]);
            }
            rects.push((S_PANEL, l.cores));
            if let Some(d) = l.det {
                rects.push((S_PANEL, d));
            }
        }
        for r in self.app.ml.rects().into_iter().chain(self.app.page_rects()) {
            if r.w > 0 && r.h > 0 {
                rects.push((S_PANEL, r));
            }
        }
        if let Some(m) = &self.app.menu {
            rects.push((S_DIALOG, m.rect));
        }
        let mut key: Vec<usize> = rects
            .iter()
            .flat_map(|(s, r)| [*s as usize, r.x, r.y, r.w, r.h])
            .collect();
        key.push(w);
        key.push(h);
        if key == self.g.kitty_key {
            return;
        }
        self.g.kitty_key = key;
        for id in [1u32, 2, 11, 12, 13, 14, 15, 16, 17] {
            let s = format!("\x1b_Ga=d,d=i,i={id},q=2;\x1b\\");
            gfx::wrap(s.as_bytes(), out, tmux);
        }
        gfx::kitty_place(1, 1, 0, 0, w, h, -1_073_741_830, out, tmux);
        for (pid, (sid, r)) in rects.iter().enumerate() {
            let pid = pid as u32 + 2;
            if soft_surface(*sid) {
                let s = format!(
                    "\x1b[{};{}H\x1b_Ga=p,i=2,p={pid},x={},y={},w={},h={},c={},r={},z=-1073741829,C=1,q=2;\x1b\\",
                    r.y + 1,
                    r.x + 1,
                    r.x * cw / 4,
                    r.y * ch / 4,
                    r.w * cw / 4,
                    r.h * ch / 4,
                    r.w,
                    r.h
                );
                gfx::wrap(s.as_bytes(), out, tmux);
            }
            gfx::kitty_place(
                10 + *sid as u32,
                pid,
                r.x,
                r.y,
                r.w,
                r.h,
                -1_073_741_828,
                out,
                tmux,
            );
        }
    }

    fn render(&mut self) {
        self.render_with(false);
    }

    /// Smooth-mode animation frame: everything moving is redrawn (`bars`: only the
    /// core cards); the order is kept unless `resort`.
    fn render_frame(&mut self, bars: bool, resort: bool) {
        self.app.anim = !self.full && !resort;
        if bars && !self.full {
            self.render_with(true);
        } else {
            self.render();
        }
        self.app.anim = false;
    }

    fn render_with(&mut self, bars: bool) {
        prof_count(7);
        let full = std::mem::take(&mut self.full);
        if bars {
            prof(14, || self.app.draw_bars(&mut self.scr));
        } else {
            prof(1, || self.app.draw(&mut self.scr, full));
        }
        // smooth: picture patches only with full repaints, never mid-animation (cells
        // that turn blank in between show the half-block backdrop until then)
        let patch = if self.app.smooth() && !full {
            Patch::None
        } else {
            self.patch()
        };
        let mut pre = Vec::new();
        self.kitty_layers(&mut pre);
        if !pre.is_empty() {
            term::write_all(OUT, &pre);
        }
        let (scr, g) = (&mut self.scr, &self.g);
        let bytes = prof(2, || scr.flush(&g.comp, g.bd.as_ref(), g.depth, patch));
        if !bytes.is_empty() {
            prof(3, || term::write_all(OUT, bytes));
        }
        if self.scr.out.capacity() > 256 * 1024 {
            // a full pixel repaint grew the buffer: give the memory back
            self.scr.out = Vec::with_capacity(64 * 1024);
        }
    }

    fn resize(&mut self) {
        let (w, h, _, _) = term::size(OUT);
        let (w, h) = (w as usize, h as usize);
        self.scr.resize(w, h);
        if self.app.w == 0 {
            self.app.first_size(w, h);
        } else {
            self.app.fit(w, h);
        }
        self.rebuild(None);
    }

    fn set_theme(&mut self, id: &str) -> Result<(), String> {
        let (t, img) = load_theme(id, &self.cfg)?;
        self.app.t = t;
        self.rebuild(img);
        Ok(())
    }

    fn menu_action(&mut self, a: menu::Action) {
        use menu::Action::*;
        match a {
            None => {}
            Preview(id) => {
                if let Err(e) = self.set_theme(&id) {
                    if let Some(m) = &mut self.app.menu {
                        m.msg = e;
                    }
                }
            }
            Apply => {
                let id = self.app.t.id.clone();
                self.app.menu = Option::None;
                self.cfg.theme = id;
                self.cfg.save();
                self.rebuild(Option::None);
            }
            Cancel => {
                let orig = self
                    .app
                    .menu
                    .as_ref()
                    .map(|m| m.orig.clone())
                    .unwrap_or_default();
                self.app.menu = Option::None;
                if self.app.t.id != orig {
                    let _ = self.set_theme(&orig);
                } else {
                    self.rebuild(Option::None);
                }
            }
            Dim(d) => {
                if let Some(file) = self.app.t.image.clone() {
                    let o = self.cfg.img_mut(&file);
                    o.dim = ((o.dim + d) * 10.0).round().clamp(2.0, 10.0) / 10.0;
                    self.cfg.save();
                    let id = self.app.t.id.clone();
                    let _ = self.set_theme(&id);
                }
            }
            Blur => {
                if let Some(file) = self.app.t.image.clone() {
                    let o = self.cfg.img_mut(&file);
                    o.blur = match o.blur as u32 {
                        0 => 1.0,
                        1 => 2.0,
                        2 => 4.0,
                        _ => 0.0,
                    };
                    self.cfg.save();
                    let id = self.app.t.id.clone();
                    let _ = self.set_theme(&id);
                }
            }
            Mode => {
                let i = probe::MODES
                    .iter()
                    .position(|&m| m == self.g.req)
                    .unwrap_or(0);
                self.g.req = probe::MODES[(i + 1) % probe::MODES.len()];
                self.cfg.image_mode = self.g.req;
                self.cfg.save();
                self.g.resolve();
                self.rebuild(Option::None);
            }
            Import(src) => {
                if let Some(m) = &mut self.app.menu {
                    m.msg = if src.starts_with("http") {
                        "downloading…".into()
                    } else {
                        "importing…".into()
                    };
                }
                self.render();
                match config::import_image(&src) {
                    Ok(name) => {
                        let id = format!("image:{name}");
                        let mut m = menu::Menu::new(&id);
                        m.orig = self
                            .app
                            .menu
                            .as_ref()
                            .map(|m| m.orig.clone())
                            .unwrap_or_default();
                        m.msg = format!("added {name} — enter to keep it");
                        self.app.menu = Some(m);
                        if let Err(e) = self.set_theme(&id) {
                            if let Some(m) = &mut self.app.menu {
                                m.msg = e;
                            }
                        }
                    }
                    Err(e) => {
                        if let Some(m) = &mut self.app.menu {
                            m.msg = e;
                        }
                    }
                }
            }
        }
    }

    /// Switch to page `p` (remembered). Always true (keep running).
    fn set_page(&mut self, p: usize) -> bool {
        let a = &mut self.app;
        if a.page == p {
            return true;
        }
        if a.page == ui::PG_PROC {
            a.procs_leave();
        }
        if a.page == ui::PG_OVERVIEW {
            a.ov_leave();
        }
        a.page = p;
        self.cfg.page = p;
        self.cfg.save();
        a.fit(a.w, a.h);
        if a.mem_visible() {
            // fresh numbers for what just appeared (outside the eco budget: input
            // handling isn't charged)
            a.m.refresh_fs();
            if a.procs_visible() && !a.m.proc_ok {
                a.m.scan_procs();
            }
            a.mem_retarget(false);
        }
        if p == ui::PG_PROC {
            a.procs_enter();
        }
        if p == ui::PG_NET {
            a.n.enter(term::mono());
            a.net_retarget(false);
        }
        if p == ui::PG_SENS {
            a.z.enter(term::mono());
            a.fit(a.w, a.h);
            a.sens_retarget(false);
        }
        if p == ui::PG_OVERVIEW {
            a.ov_enter();
            a.fit(a.w, a.h);
            trim_heap();
        }
        true
    }

    /// Returns false to quit.
    fn handle(&mut self, ev: Ev) -> bool {
        if self.app.menu.is_some() {
            let a = match ev {
                Ev::Key(Key::CtrlC) => return false,
                Ev::Key(k) => self.app.menu.as_mut().unwrap().key(k),
                Ev::Mouse { x, y, btn: 0 } => self
                    .app
                    .menu
                    .as_mut()
                    .unwrap()
                    .click(x as usize, y as usize),
                Ev::Mouse { btn: 64, .. } => self.app.menu.as_mut().unwrap().key(Key::Up),
                Ev::Mouse { btn: 65, .. } => self.app.menu.as_mut().unwrap().key(Key::Down),
                _ => menu::Action::None,
            };
            self.menu_action(a);
            return true;
        }
        let key = match ev {
            Ev::Key(k) => k,
            Ev::Mouse { x, y, btn } => {
                let (x, y) = (x as usize, y as usize);
                // processes page: rows, column titles, wheel
                if self.app.key_at(x, y).is_none() && self.app.proc_click(x, y, btn) {
                    return true;
                }
                match btn {
                    0 => {
                        if let Some(k) = self.app.key_at(x, y) {
                            Key::Char(k)
                        } else if let Some(p) = self.app.ov_at(x, y) {
                            return self.set_page(p);
                        } else if let Some(g) = self.app.card_at(x, y) {
                            self.app.selected = g;
                            return true;
                        } else {
                            return true;
                        }
                    }
                    64 => Key::Up,
                    65 => Key::Down,
                    _ => return true,
                }
            }
        };
        // the processes page's own keys (and its dialogs / filter line) come first
        if self.app.proc_key(key) {
            return true;
        }
        // pages: 1-5, tab / shift-tab
        let np = ui::NPAGES;
        let p = self.app.page;
        match key {
            Key::Char(c @ '0'..='5') => return self.set_page(c as usize - '0' as usize),
            Key::Tab => return self.set_page((p + 1) % np),
            Key::BackTab => return self.set_page((p + np - 1) % np),
            Key::Char('m') => return self.set_page(if p == ui::PG_MEM { ui::PG_CPU } else { ui::PG_MEM }),
            _ => {}
        }
        let a = &mut self.app;
        match key {
            Key::Char('q') | Key::CtrlC => return false,
            Key::Char(' ') => a.paused = !a.paused,
            Key::Char('s') | Key::Char('+') | Key::Char('=') | Key::Char('-') | Key::Char('_') => {
                let n = ui::SPEEDS.len();
                a.speed = match key {
                    Key::Char('s') => (a.speed + 1) % n,
                    Key::Char('+') | Key::Char('=') => (a.speed + 1).min(n - 1),
                    _ => a.speed.saturating_sub(1),
                };
                self.cfg.speed = a.speed;
                self.cfg.save();
            }
            Key::Char('o') => {
                a.sort = (a.sort + 1) % 3;
                self.cfg.sort = a.sort;
                self.cfg.save();
                a.fit(a.w, a.h); // CCD divider rows exist only in core order
            }
            Key::Char('p') if a.mem_visible() => {
                a.toggle_split();
            }
            Key::Char('c') => {
                a.view = (a.view + 1) % 3;
                self.cfg.view = a.view;
                self.cfg.save();
                a.fit(a.w, a.h);
            }
            Key::Char('g') => {
                a.show_top = !a.show_top;
                a.fit(a.w, a.h);
            }
            // d: overview (heatmap / history grid) → selected core's graphs → hidden
            Key::Char('d') => {
                (a.show_det, a.det_core) = match (a.show_det, a.det_core) {
                    (true, false) => (true, true),
                    (true, true) => (false, false),
                    _ => (true, false),
                };
                a.fit(a.w, a.h);
            }
            Key::Enter => {
                a.show_det = !a.show_det;
                a.fit(a.w, a.h);
            }
            Key::Char('r') => a.s.reset_peaks(),
            Key::Char('t') => {
                a.menu = Some(menu::Menu::new(&a.t.id));
                if self.g.mode.pixels() && self.app.t.image.is_some() {
                    self.rebuild(None);
                }
            }
            Key::Up | Key::Char('k') => a.move_sel(0, -1),
            Key::Down | Key::Char('j') => a.move_sel(0, 1),
            Key::Left | Key::Char('h') => a.move_sel(-1, 0),
            Key::Right | Key::Char('l') => a.move_sel(1, 0),
            Key::CtrlZ => {
                self.kitty_clear();
                term::suspend(0, OUT);
                self.scr.invalidate();
                self.rebuild(None);
            }
            _ => {}
        }
        true
    }
}

/// Keep the process on one physical core that the scheduler rarely uses (lowest
/// amd-pstate preferred-core rank, else the last core): its private L2 then usually
/// still holds our code and data at the next wakeup, which makes each wakeup ~15 %
/// cheaper. Only when the affinity wasn't restricted already; CPUVIEW_PIN=off or =N.
fn auto_pin(t: &sys::Topo) {
    let env = std::env::var("CPUVIEW_PIN").unwrap_or_default();
    if env == "off" || t.cpus.len() < 4 {
        return;
    }
    unsafe {
        let mut cur: libc::cpu_set_t = std::mem::zeroed();
        let sz = std::mem::size_of::<libc::cpu_set_t>();
        if libc::sched_getaffinity(0, sz, &mut cur) != 0 {
            return;
        }
        // a narrower affinity (taskset, cgroup cpuset) is respected: pin to a core
        // inside it, and leave a single-core choice alone
        let allowed = |g: &Vec<usize>| g.iter().any(|&i| libc::CPU_ISSET(t.cpus[i], &cur));
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if let Ok(n) = env.parse::<usize>() {
            libc::CPU_SET(n, &mut set);
        } else {
            let Some(g) = (0..t.groups.len())
                .filter(|&g| allowed(&t.groups[g]))
                .min_by_key(|&g| {
                    if t.rank[g] >= 0 {
                        t.rank[g]
                    } else {
                        -(g as i64)
                    }
                })
            else {
                return;
            };
            if (0..t.groups.len())
                .filter(|&g| allowed(&t.groups[g]))
                .count()
                < 2
            {
                return;
            }
            for &i in &t.groups[g] {
                if libc::CPU_ISSET(t.cpus[i], &cur) {
                    libc::CPU_SET(t.cpus[i], &mut set);
                }
            }
        }
        libc::sched_setaffinity(0, sz, &set);
    }
}

/// The overview's own sampling (view 0): its sensors (no per-core power, a dear
/// chip rarely: sensors::Sens::tick_ov), processes and containers
/// (App::ov_sample). `push`: history seconds for the sensors, `net_held` the links'.
fn ov_work(app: &mut App, now: i64, eco: bool, smooth: bool, push: u32, net_held: &mut u32) {
    let cpu_t = !app.s.temps.is_empty();
    let quiet = eco && app.z.ov_near(now, true, cpu_t) > 0;
    prof(4, || app.z.tick_ov(now, push, eco, cpu_t, false));
    let push = std::mem::take(net_held);
    prof(14, || app.ov_sample(now, eco, smooth, push, quiet));
}

fn update_clock(app: &mut App) {
    let wall = term::now_ns(libc::CLOCK_REALTIME) / 1_000_000_000;
    app.clock = term::local_hms(wall);
    app.uptime = (term::now_ns(libc::CLOCK_BOOTTIME) / 1_000_000_000) as u64;
}

fn doctor(req: Mode, no_probe: bool) {
    let env = Env::get();
    let fd = unsafe {
        libc::open(
            c"/dev/tty".as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    let mut caps = Caps::default();
    let (cols, rows, xp, yp) = term::size(if fd >= 0 { fd } else { OUT });
    if fd >= 0 && !no_probe {
        term::raw_mode(fd);
        term::write_all(fd, b"\x1b[?1049h");
        caps = probe::probe(fd, fd, env.tmux);
        term::write_all(fd, b"\x1b[?1049l");
        std::thread::sleep(std::time::Duration::from_millis(30));
        unsafe { libc::tcflush(fd, libc::TCIFLUSH) };
        term::restore_mode();
    }
    let (mode, why) = probe::choose_mode(&caps, &env, req);
    let cell = probe::cell_size(&caps, cols, rows, xp, yp);
    let tc = probe::truecolor(&caps, &env);
    println!("cpuview {VERSION} doctor");
    println!(
        "terminal      {}",
        if caps.name.is_empty() {
            "(no XTVERSION reply)"
        } else {
            &caps.name
        }
    );
    println!(
        "TERM          {}   COLORTERM={}   ssh={}",
        env.term,
        if env.colorterm.is_empty() {
            "(unset)"
        } else {
            &env.colorterm
        },
        if std::env::var_os("SSH_CONNECTION").is_some() {
            "yes"
        } else {
            "no"
        }
    );
    if no_probe {
        println!("replies       not probed (--no-probe)");
    } else {
        println!(
            "replies       {} in {} ms   DA1={:?}",
            if caps.answered { "yes" } else { "NO" },
            caps.ms,
            caps.da1
        );
    }
    println!(
        "truecolor     {} (DECRQSS {})",
        if tc { "yes" } else { "no" },
        if caps.truecolor_echo {
            "echoed"
        } else {
            "no echo"
        }
    );
    println!("sixel         {}", if caps.sixel { "yes" } else { "no" });
    println!("kitty         {}", if caps.kitty { "yes" } else { "no" });
    match cell {
        Some(((w, h), src)) => println!("cell size     {w}x{h} px ({src})"),
        None => println!("cell size     unknown"),
    }
    println!(
        "window        {cols}x{rows} cells, ioctl pixels {xp}x{yp}, CSI 14t {:?}",
        caps.window_px
    );
    println!(
        "tmux          {}",
        if probe::in_tmux(&caps, &env) {
            "yes"
        } else {
            "no"
        }
    );
    println!("→ image mode  {}  ({why})", mode.name());
    println!("raw replies   {:?}", String::from_utf8_lossy(&caps.raw));
    // test strip in the chosen mode
    let (w, h) = (320usize, 120usize);
    let mut rgbv = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            let c = color::hsv_to_rgb([
                x as f32 / w as f32,
                0.8,
                0.25 + 0.75 * (1.0 - y as f32 / h as f32),
            ]);
            rgbv.extend(c.iter().map(|v| (v * 255.0) as u8));
        }
    }
    println!(
        "\n{} test image below (a colourful strip means it works):",
        mode.name()
    );
    let mut out = Vec::new();
    let tmux = probe::in_tmux(&caps, &env);
    match mode {
        Mode::Sixel => {
            let q = image::quantize(&rgbv, w, h, 256);
            gfx::Sixel::default().encode(&q.idx, w, h, &q.pal, None, &mut out);
            out.push(b'\n');
        }
        Mode::Kitty => {
            gfx::kitty_transmit_rgb(&rgbv, w, h, 77, &mut out, tmux);
            let s = "\x1b_Ga=p,i=77,c=32,r=6,q=2;\x1b\\";
            gfx::wrap(s.as_bytes(), &mut out, tmux);
            out.extend_from_slice(b"\n\n\n\n\n\n");
        }
        Mode::Iterm => {
            gfx::iterm_inline(&gfx::png_rgb(&rgbv, w, h), 32, 6, &mut out, tmux);
            out.extend_from_slice(b"\n\n\n\n\n\n");
        }
        Mode::Off | Mode::Terminal => out.extend_from_slice(b"(no image in this mode)\n"),
        _ => {
            for row in (0..12).step_by(2) {
                for x in 0..64 {
                    let px = |r: usize| {
                        let o = ((r * h / 12) * w + x * w / 64) * 3;
                        color::rgb(rgbv[o], rgbv[o + 1], rgbv[o + 2])
                    };
                    let (a, b) = (px(row), px(row + 1));
                    if mode == Mode::Blocks256 {
                        let _ = std::io::Write::write_fmt(
                            &mut out,
                            format_args!(
                                "\x1b[38;5;{};48;5;{}m▀",
                                color::to_256(a, x, row),
                                color::to_256(b, x, row + 1)
                            ),
                        );
                    } else {
                        let ([r1, g1, b1], [r2, g2, b2]) = (color::ch(a), color::ch(b));
                        let _ = std::io::Write::write_fmt(
                            &mut out,
                            format_args!("\x1b[38;2;{r1};{g1};{b1};48;2;{r2};{g2};{b2}m▀"),
                        );
                    }
                }
                out.extend_from_slice(b"\x1b[0m\n");
            }
        }
    }
    term::write_all(OUT, &out);
}

fn die(msg: &str) -> ! {
    eprintln!("cpuview: {msg}");
    std::process::exit(2)
}

fn main() {
    // Large buffers (screen, backdrop canvases, decoded pictures) go to their own
    // mappings and back to the kernel when freed: without a fixed threshold glibc
    // raises it after the first big free and later big buffers pin the heap. Free
    // memory at the top of the heap goes back too, without padding.
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 64 * 1024);
        libc::mallopt(libc::M_TRIM_THRESHOLD, 64 * 1024);
        libc::mallopt(libc::M_TOP_PAD, 0);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cfg = Config::load();
    let (mut want_doctor, mut no_probe) = (false, false);
    let mut theme_arg = None;
    let mut image_arg = None;
    let mut mode_arg = None;
    let mut speed_arg = None;
    let mut page_arg = None;
    let (mut setup, mut undo, mut per_core, mut dry) = (false, false, false, false);
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let (k, inline) = match a.split_once('=') {
            Some((k, v)) if k.starts_with("--") => (k, Some(v.to_string())),
            _ => (a, None),
        };
        let mut val = || -> String {
            if let Some(v) = &inline {
                return v.clone();
            }
            i += 1;
            args.get(i)
                .cloned()
                .unwrap_or_else(|| die(&format!("{k} needs a value")))
        };
        match k {
            "-h" | "--help" => {
                print!("{HELP}");
                return;
            }
            "-V" | "--version" => {
                println!("cpuview {VERSION}");
                return;
            }
            "--doctor" => want_doctor = true,
            "--setup-sensors" => setup = true,
            "--undo" => undo = true,
            "--per-core" => per_core = true,
            "--dry-run" => dry = true,
            "--no-probe" => no_probe = true,
            "--theme" => theme_arg = Some(val()),
            "--image" => image_arg = Some(val()),
            "--speed" => {
                let v = val();
                let n = ui::speed_index(&v);
                if n == 0 && !matches!(v.as_str(), "eco" | "auto") {
                    let all: Vec<_> = ui::SPEEDS.iter().map(|s| s.0).collect();
                    die(&format!("unknown speed: {v} (one of {})", all.join(", ")));
                }
                speed_arg = Some(n);
            }
            "--page" => {
                let v = val();
                page_arg = Some(ui::page_index(&v).unwrap_or_else(|| {
                    die("unknown page (one of overview, cpu, memory, network, processes, sensors)")
                }));
            }
            "--image-mode" => {
                let v = val();
                mode_arg = Some(
                    Mode::parse(&v).unwrap_or_else(|| die(&format!("unknown image mode: {v}"))),
                );
            }
            _ => die(&format!("unknown option: {a} (see --help)")),
        }
        i += 1;
    }
    if setup || undo || per_core || dry {
        if !setup {
            die("--undo, --per-core and --dry-run go with --setup-sensors");
        }
        std::process::exit(sensors::setup(undo, per_core, dry));
    }
    if let Some(n) = speed_arg {
        cfg.speed = n;
        cfg.save();
    }
    if let Some(p) = page_arg {
        cfg.page = p;
        cfg.save();
    }
    if let Some(m) = mode_arg {
        cfg.image_mode = m;
        cfg.save();
    }
    if want_doctor {
        doctor(cfg.image_mode, no_probe);
        return;
    }
    if let Some(src) = image_arg {
        match config::import_image(&src) {
            Ok(name) => {
                cfg.theme = format!("image:{name}");
                cfg.save();
            }
            Err(e) => die(&e),
        }
    }
    if let Some(t) = theme_arg {
        if load_theme(&t, &cfg).is_err() {
            die(&format!(
                "unknown theme: {t} (built-in: {})",
                builtin_ids().join(", ")
            ));
        }
        cfg.theme = t;
        cfg.save();
    }
    if !term::is_tty(0) || !term::is_tty(OUT) {
        die("needs a terminal (stdin and stdout must be a tty)");
    }

    // terminal setup
    let mask = term::setup_signals();
    unsafe {
        libc::prctl(libc::PR_SET_TIMERSLACK, 2_000_000u64, 0, 0, 0); // let the kernel batch our wakeups
    }
    term::raw_mode(0);
    std::panic::set_hook(Box::new(|info| {
        term::write_all(OUT, term::LEAVE.as_bytes());
        term::restore_mode();
        eprintln!("cpuview: {info}");
    }));
    term::write_all(OUT, term::ENTER.as_bytes());
    let env = Env::get();
    let caps = if no_probe {
        Caps::default()
    } else {
        probe::probe(0, OUT, env.tmux)
    };
    let (cols, rows, xp, yp) = term::size(OUT);
    let (cell, cell_src) =
        probe::cell_size(&caps, cols, rows, xp, yp).unwrap_or(((10, 20), "assumed 10x20"));
    let truecolor = probe::truecolor(&caps, &env);
    let theme0 = theme::load("default").unwrap();
    prof::init();
    let mut g = Gfx {
        req: cfg.image_mode,
        mode: Mode::Off,
        why: String::new(),
        comp: Comp::new(&theme0, Look::Solid),
        caps,
        env,
        truecolor,
        cell,
        cell_src,
        bd: None,
        depth: Depth::True,
        kitty_key: Vec::new(),
        kitty_live: false,
    };
    g.resolve();
    let _ = (&g.why, g.cell_src);
    let mut parser = input::Parser::default();
    let mut evs = Vec::new();
    parser.feed(&g.caps.raw, &mut evs); // keys typed during the probe (replies are ignored)

    let sampler = sys::Sampler::new();
    auto_pin(&sampler.t);
    let (t, img) = load_theme(&cfg.theme, &cfg).unwrap_or((theme0, None));
    let mut app = App::new(sampler, t);
    app.m = mem::Mem::new();
    app.n = net::Net::new();
    app.page = cfg.page;
    app.sort = cfg.sort;
    app.view = cfg.view;
    app.speed = cfg.speed;
    app.depth_name = if g.depth == Depth::True {
        "truecolor"
    } else {
        "256 colours"
    };
    let mut st = State {
        full: true,
        app,
        scr: Screen::new(cols as usize, rows as usize),
        g,
        cfg,
    };
    st.scr.tmux = st.g.tmux();
    st.app.first_size(cols as usize, rows as usize);
    if st.app.procs_visible() {
        st.app.m.scan_procs();
    }
    if st.app.page == ui::PG_PROC {
        st.app.procs_enter();
    }
    if st.app.page == ui::PG_NET {
        st.app.n.enter(term::mono());
    }
    if st.app.page == ui::PG_SENS {
        st.app.z.enter(term::mono());
        st.app.fit(cols as usize, rows as usize);
    }
    if st.app.page == ui::PG_OVERVIEW {
        st.app.ov_enter();
        st.app.fit(cols as usize, rows as usize);
        trim_heap();
    }
    st.rebuild(img);
    update_clock(&mut st.app);
    st.app.s.sample(0, true);
    st.app.s.publish();
    st.app.bars_snap();

    // ── main loop ───────────────────────────────────────────────────────────
    // The process sleeps in ppoll() until the next sample, the next frame (smooth),
    // the next minute (title clock) or input: nothing else ever wakes it.
    //
    // Speed (s / + / -): a fixed rate samples on a wall-clock grid of 1/hz s, history
    // graphs still get one point per second. Eco (the default) is adaptive:
    //
    // Sampling is paced by a CPU-time token bucket. Tokens accrue at BUDGET ns per
    // second (capped at CAP); every wakeup spends all of the process's CPU time
    // since the last one (CLOCK_PROCESS_CPUTIME_ID: every thread, nothing left
    // out), debt included. Samples are taken every second while loads are changing
    // and every IDLE_GAP seconds when they are stable, but only once the bucket
    // holds the expected cost of one (`est`, a slowly decaying peak of the recent
    // sample wakeups, so the dear ones every few samples are paid in advance). So
    // the long-run average stays under BUDGET and any 60 s window under BUDGET +
    // CAP / 60 (85 µs/s, plus a misjudged wakeup) whatever the machine (cold caches,
    // many CPUs, a busy system), the page or the terminal size. When a sample costs
    // more than CAP, the bucket holds one but fills slower, which keeps that bound.
    // User-initiated work (keys, resize, resume) is charged too, but may only use
    // up saved tokens: it never puts sampling in debt.
    //
    // Smooth samples 10 times a second (sys::dual) and draws on a fixed frame grid
    // (anim::FPS, aligned with the samples): every frame advances the springs by
    // exactly one grid step, digits follow the bars (with hysteresis), graphs scroll
    // continuously (anim::Curve). Frames run while anything moves or scrolls; one
    // write each, nothing when nothing changed. Should frames cost more than SM_BUDGET
    // on average over ~5 s (huge screens), the frame rate is halved, and stays constant.
    const SEC: i64 = 1_000_000_000;
    const BUDGET: i64 = 65_000;
    const CAP: i64 = 1_200_000;
    const IDLE_GAP: i64 = 2;
    const SM_BUDGET: i64 = 5_000_000;
    let fixed: Option<i64> = std::env::var("CPUVIEW_PERIOD_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(|ms: i64| ms.max(100) * 1_000_000);
    let mut offset = term::now_ns(libc::CLOCK_REALTIME) - term::mono();
    let mut tokens = CAP;
    let mut refill_t = term::mono();
    let mut est: i64 = CAP / 2;
    let mut nsamp = 0u32;
    let mut cpu_mark = term::cpu_ns();
    let mut user = false;
    let mut active_left: u32 = 5;
    let wall_sec = |mono: i64, offset: i64| (mono + offset) / SEC;
    let last_sample_sec = wall_sec(term::mono(), offset);
    let mut speed = st.app.speed;
    let mut period = SEC;
    let mut last_tick = i64::MIN; // set from speed below
    let mut last_push_sec = last_sample_sec;
    let mut last_temp_sec = last_sample_sec;
    let mut last_slow_sec = last_sample_sec;
    let mut last_pol_sec = last_sample_sec;
    let mut last_min = last_sample_sec / 60;
    let (mut net_held, mut net_last) = (0u32, 0i64);
    // the overview's sensors and processes: due (smooth: done after the frame), and
    // the history seconds they hold
    let (mut ov_due, mut ov_push) = (false, 0u32);
    let mut winch_at: Option<i64> = None;
    let mut esc_at: Option<i64> = None;
    // smooth: next frame on the grid, the time the animation state is at, frames per
    // second, CPU accounting for the governor
    let mut next_frame: Option<i64> = None;
    let mut anim_t = term::mono();
    // CPUVIEW_FPS: smooth-mode frame rate (default anim::FPS)
    let fps0: i64 = std::env::var("CPUVIEW_FPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(anim::FPS, |f: i64| f.clamp(10, 120));
    let mut fps = fps0;
    let mut fcount = 0u64;
    let mut resort = false;
    let (mut cpu_held, mut cpu_skip) = (0u32, false);
    let mut settled = false; // smooth, paused and at rest: no frames
    let (mut sm_cpu, mut sm_avg) = (term::cpu_ns(), 0i64);
    let mut buf = [0u8; 4096];
    let mut dirty = true;
    let mut running = true;
    for ev in std::mem::take(&mut evs) {
        running &= st.handle(ev);
    }
    while running {
        if dirty {
            st.render();
            dirty = false;
        }
        if term::QUIT.load(Ordering::Relaxed) {
            break;
        }
        // speed changed: re-grid; the budget only applies to eco, so start it half full
        // (start-up costs more than a sample: first scans, every job once)
        if st.app.speed != speed || last_tick == i64::MIN {
            speed = st.app.speed;
            let hz = ui::SPEEDS[speed].1;
            period = fixed.unwrap_or(if hz > 0 { SEC / hz } else { SEC });
            last_tick = (term::mono() + offset) / period;
            tokens = CAP / 2;
            cpu_mark = term::cpu_ns();
            let smooth = st.app.smooth();
            st.app.s.dual = smooth;
            // smooth: clocks shown as the clock while running
            st.app.s.clock_hold = if smooth { 1_500_000_000 } else { 0 };
            anim_t = term::mono();
            fps = fps0;
            sm_avg = 0;
            if !smooth {
                st.app.bars_snap();
                next_frame = None;
            }
        }
        let smooth = st.app.smooth();
        if smooth && next_frame.is_none() && !(st.app.paused && settled) {
            next_frame = Some(anim::next_frame(term::mono() + offset, fps) - offset);
        }
        let eco = fixed.is_none() && ui::SPEEDS[speed].1 == 0;
        // account the CPU time of the wakeup that just finished (eco only: reading
        // the process clock is a syscall, which faster speeds needn't pay per frame)
        let c = if eco { term::cpu_ns() } else { cpu_mark };
        let now = term::mono();
        // sensors page: the sensor reads have their own share of the budget (`zp`,
        // planned from their measured costs and periods, see sensors::Sens::period),
        // the rest of the wakeup is charged here. A board's sensor chip can cost
        // 2.5 ms a read, so any minute holding one keeps the rest under ~25 µs/s: the
        // bucket holds 0.4 ms there (bursts of the rest stay small)
        // (the overview: its sensors' share, and while a dear read is within a minute
        // most of the budget for that read, see sensors::Sens::ov_planned)
        let zs = st.app.z.take_spent() + std::mem::take(&mut st.app.ov.spent);
        let ovp = st.app.page == ui::PG_OVERVIEW;
        let cpu_t = !st.app.s.temps.is_empty();
        let near = eco && ovp && st.app.z.ov_near(now, true, cpu_t) > 0;
        let zp = if eco && st.app.page == ui::PG_SENS {
            st.app.z.planned(true).min(48_000)
        } else if eco && ovp {
            (st.app.z.ov_planned(now, true, cpu_t) + st.app.ov_planned()).min(55_000)
        } else {
            0
        };
        let cap = if zp > 0 && (!ovp || near) { CAP / 3 } else { CAP };
        // (a wakeup that handled keys, a resize or a resume is the user's: it may use
        // up saved tokens, never borrow)
        tokens = (tokens - (c - cpu_mark - zs).max(0)).max(if user { tokens.min(0) } else { i64::MIN });
        if !eco {
            tokens = CAP;
        }
        cpu_mark = c;
        user = false;
        // wakeups dearer than CAP: the bucket holds one, and fills slower by the excess
        // over a minute (so any minute still stays under BUDGET + CAP / 60)
        let over = (est - cap).max(0);
        let budget = BUDGET - zp;
        tokens = (tokens + (budget - over / 60) * (now - refill_t) / SEC).min(cap + over);
        refill_t = now;
        // next sample: eco = whole wall-clock seconds, gap by activity, paid by
        // tokens; fixed speed = the next tick of the 1/hz grid
        let t_sample = if eco {
            let gap = if active_left > 0 { 1 } else { IDLE_GAP };
            let mut t = (last_tick + gap) * SEC - offset;
            if tokens < est {
                t = t.max(now + (est - tokens) * SEC / (budget - over / 60).max(1_000));
                t = (wall_sec(t, offset) + 1) * SEC - offset;
            }
            t
        } else {
            (last_tick + 1) * period - offset
        };
        let t_minute = ((wall_sec(now, offset) / 60) + 1) * 60 * SEC - offset;
        let mut deadline = if st.app.paused {
            t_minute
        } else {
            t_sample.min(t_minute)
        };
        if let Some(t) = winch_at {
            deadline = deadline.min(t);
        }
        if let Some(t) = esc_at {
            deadline = deadline.min(t);
        }
        if let Some(t) = next_frame {
            deadline = deadline.min(t);
        }
        let readable = term::wait(0, deadline - now, &mask);
        if term::WINCH.swap(false, Ordering::Relaxed) {
            winch_at = Some(term::mono() + 80_000_000);
        }
        if term::CONT.swap(false, Ordering::Relaxed) {
            term::raw_mode(0);
            term::write_all(OUT, term::ENTER.as_bytes());
            st.rebuild(None);
            dirty = true;
            user = true;
        }
        if readable {
            user = true;
            let n = term::read(0, &mut buf);
            if n <= 0 {
                break; // terminal gone
            }
            parser.feed(&buf[..n as usize], &mut evs);
            esc_at = if parser.pending() {
                Some(term::mono() + 40_000_000)
            } else {
                None
            };
            settled = false;
        }
        let now = term::mono();
        if esc_at.is_some_and(|t| now >= t) {
            user = true;
            esc_at = None;
            parser.flush_timeout(&mut evs);
        }
        if winch_at.is_some_and(|t| now >= t) {
            user = true;
            winch_at = None;
            st.resize();
            dirty = true;
            fps = fps0;
            sm_avg = 0;
        }
        // keys (a lone Esc after its timeout too)
        for ev in std::mem::take(&mut evs) {
            running &= st.handle(ev);
            dirty = true;
            st.full = true;
        }
        if user && dirty && running {
            st.render();
            dirty = false;
        }
        // wakeups land a little after the deadline: round to the intended tick
        let slack = (period / 5).min(if smooth { 2_000_000 } else { 20_000_000 });
        let sec = wall_sec(now + slack, offset);
        let tick = (now + slack + offset) / period;
        if sec / 60 != last_min {
            last_min = sec / 60;
            update_clock(&mut st.app);
            offset = term::now_ns(libc::CLOCK_REALTIME) - term::mono();
            dirty = true;
        }
        if !st.app.paused && now + slack >= t_sample && tick > last_tick {
            last_tick = tick;
            prof_count(6);
            let push = (sec - last_push_sec).clamp(0, 30) as u32;
            last_push_sec = sec;
            // slow sensors: one temperature every 3 s (round robin; smooth: all of
            // them every second), load average every 5 s, governor / EPP every 10 s
            let zpage = st.app.page == ui::PG_SENS;
            let ov = st.app.page == ui::PG_OVERVIEW;
            if zpage {
                // (the sensors page reads its own)
            } else if smooth && push > 0 {
                last_temp_sec = sec;
                prof(4, || st.app.s.read_temps(true));
            } else if sec - last_temp_sec >= 3 {
                last_temp_sec = sec;
                prof(4, || st.app.s.read_temps(false));
            }
            // (load average, governor: not on the sensors page)
            if sec - last_slow_sec >= 5 && !zpage {
                last_slow_sec = sec;
                prof(5, || st.app.s.read_slow());
            }
            if sec - last_pol_sec >= 10 && !zpage {
                last_pol_sec = sec;
                st.app.s.read_policy();
            }
            // smooth: clocks 5 times a second, numbers / order 2.5 times
            // (the overview in eco: every other sample, its numbers are averages)
            let clocks = if eco && st.app.page == ui::PG_OVERVIEW { nsamp % 2 == 0 } else { !smooth || tick % 2 == 0 };
            // the network page shows no CPU numbers: the CPU sampler runs every other
            // sample there (eco; other speeds twice a second), its histories get the
            // seconds in between from the next one
            cpu_held += push;
            // (the sensors page: one in four in eco)
            cpu_skip = (st.app.page == ui::PG_NET || zpage)
                && (if eco {
                    if zpage {
                        nsamp % 6 != 0
                    } else {
                        !cpu_skip
                    }
                } else {
                    tick % 5 != 0
                });
            if !cpu_skip {
                let push = std::mem::take(&mut cpu_held);
                prof(0, || st.app.s.sample(push, clocks));
            }
            // memory & storage: memory and disks every sample (cheap, their histories
            // keep going); swap rates and pressure, filesystems (every 3 s, eco 10 s),
            // processes (below) and the DIMM / drive temperatures (helper thread, every
            // 5 s, eco 60 s) only while shown
            // (the overview shows RAM, swap, disks and the fullest filesystem)
            let vis = st.app.mem_visible();
            let mvis = vis || ov;
            let pvis = st.app.procs_visible();
            let m = &mut st.app.m;
            let now = term::mono();
            // swap rates: smooth 2.5 times a second, eco every 4 s at most (the
            // overview: 12 s)
            let vm = if eco {
                now - m.last_vm >= if ov { 12 } else { 4 } * SEC - SEC / 5
            } else {
                !smooth || tick % 4 == 0
            };
            prof(16, || m.sample(push, now, mvis, vm));
            // (the overview: the hottest part may be a drive or a RAM stick)
            mem::slow_period(if !vis && !ov {
                0
            } else if eco {
                60_000
            } else if ov {
                10_000
            } else {
                5_000
            });
            if mvis {
                let gap = if eco && ov { 30 } else if eco { 10 } else { 3 } * SEC;
                if now - m.last_fs >= gap {
                    m.last_fs = now;
                    m.refresh_fs();
                }
            }
            if vis {
                // processes: a full scan (to find newcomers) costs ~1.3 µs a process,
                // so it runs every 4 s (eco: 2 minutes); the biggest ones are followed
                // live through their open statm (~1 µs each): 2.5 times a second
                // (eco: every 10 s)
                let gap = if eco { 120 } else { 4 } * SEC;
                let live = if eco { 10 * SEC } else { 2 * SEC / 5 };
                if pvis && now - m.last_proc >= gap - SEC / 5 {
                    m.scan_procs();
                } else if pvis && now - m.last_top >= live - SEC / 20 {
                    m.last_top = now;
                    m.refresh_procs();
                    // eco: a full scan early when a big newcomer appeared (at most
                    // once a minute: on a busy machine there always is one, and a
                    // scan costs ~1 ms)
                    if eco && now - m.last_proc >= 60 * SEC && m.need_scan() {
                        m.scan_procs();
                    }
                }
            }
            // network & system: links every sample while shown (every 10 s hidden, to
            // keep their history), the rest on its own spread-out schedule (net::tick)
            // (the sensors page in eco: the hidden links' history every 30 s, its
            // seconds held until then)
            net_held += push;
            if ov {
                // (the overview: links every 4 s in eco, containers now and then: in
                // App::ov_sample)
            } else if !(zpage && eco && now - net_last < 30 * SEC) {
                net_last = now;
                let push = std::mem::take(&mut net_held);
                st.app.n.tick(now, push, st.app.page == ui::PG_NET, eco);
            }
            // sensors & power (view 5): only while shown, each source on its own
            // cadence (sensors::Sens::tick)
            if zpage {
                prof(4, || st.app.z.tick(now, push, eco));
            }
            // the overview (view 0): its sensors (no per-core power, a dear chip rarely
            // in eco) and processes (sensors::Sens::tick_ov, App::ov_sample)
            // (smooth: after this wakeup's frame, so a dear read doesn't delay it)
            if ov {
                ov_push += push;
                if smooth {
                    ov_due = true;
                } else {
                    ov_work(&mut st.app, now, eco, smooth, std::mem::take(&mut ov_push), &mut net_held);
                }
            }
            // processes page (view 4): only while shown, see App::procs_sample
            if st.app.page == ui::PG_PROC {
                prof(16, || st.app.procs_sample(now, eco, smooth, tick, push));
            }
            if smooth {
                if tick % 4 == 0 {
                    st.app.s.publish();
                    resort = true; // a new order may show with this frame
                }
                st.app.retarget();
                if push > 0 {
                    // frame-rate governor: CPU per second (averaged over ~5 s) against
                    // SM_BUDGET
                    let c = term::cpu_ns();
                    sm_avg = (sm_avg * 4 + (c - sm_cpu) / push as i64) / 5;
                    sm_cpu = c;
                    if sm_avg > SM_BUDGET && fps > 15 {
                        fps /= 2;
                        sm_avg = 0;
                    }
                }
            } else {
                st.app.s.publish();
                st.app.bars_snap();
            }
            active_left = if st.app.s.activity > 0.15 {
                5
            } else {
                active_left.saturating_sub(1)
            };
            update_clock(&mut st.app);
            if !smooth {
                st.render();
                dirty = false;
            }
            nsamp += 1;
            if eco && nsamp > 2 {
                // a slowly decaying peak: the dearer wakeups (scans, refreshes) every
                // few samples keep it up (not the first samples: start-up, first scans)
                let spent = term::cpu_ns() - cpu_mark - st.app.z.spent() - st.app.ov.spent;
                est = if spent > est {
                    spent
                } else {
                    est - (est - spent) / 16
                };
                est = est.clamp(20_000, 4 * CAP);
            }
        } else if st.app.paused && sec > last_push_sec {
            last_push_sec = sec;
        }
        // smooth: a frame on the grid (in the same wakeup as a sample that's due)
        if smooth && next_frame.is_some_and(|t| now + 1_000_000 >= t) {
            let t = next_frame.unwrap();
            let dt = ((t - anim_t) as f32 / 1e9).clamp(0.0, 0.1);
            anim_t = t;
            let a = &mut st.app;
            let moving = prof(14, || a.anim_step(dt) | a.anim_numbers(dt));
            if !a.paused {
                a.phase = ((t - a.s.push_t) as f32 / 1e9).clamp(0.0, 1.0);
            }
            if !a.paused {
                a.frame += 1;
            }
            // at 60 frames/s every other frame moves only the bars (cheap); numbers,
            // panels and graphs change on the others
            fcount += 1;
            let bars = fps > 30 && fcount % 2 == 1 && !resort;
            if st.app.menu.is_none() || fcount % 6 == 0 {
                // (the theme menu repaints whole screens: 10 a second is plenty)
                st.render_frame(bars, resort);
                resort = false;
            }
            dirty = false;
            // paused and settled: no more frames until something changes
            settled = st.app.paused && !moving;
            next_frame = if settled {
                None
            } else {
                Some(anim::next_frame(now + offset, fps) - offset)
            };
        }
        if std::mem::take(&mut ov_due) && st.app.page == ui::PG_OVERVIEW {
            ov_work(&mut st.app, now, eco, smooth, std::mem::take(&mut ov_push), &mut net_held);
        }
    }
    st.kitty_clear();
    term::write_all(OUT, term::LEAVE.as_bytes());
    term::restore_mode();
    prof::dump(0);
}
