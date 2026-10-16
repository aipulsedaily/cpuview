//! Theme menu (modal): live preview, apply / cancel, image mode, dim, blur, add image.

use crate::f;
use crate::input::Key;
use crate::screen::*;
use crate::theme::*;
use crate::ui::{App, Rect};
use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Builtin(&'static str),
    Image(String),
    Add,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub kind: Kind,
    pub label: String,
    pub desc: String,
    pub swatch: Option<[u32; 8]>,
}

impl Item {
    pub fn id(&self) -> Option<String> {
        match &self.kind {
            Kind::Builtin(n) => Some(n.to_string()),
            Kind::Image(f) => Some(format!("image:{f}")),
            Kind::Add => None,
        }
    }
}

pub struct Menu {
    pub items: Vec<Item>,
    pub cur: usize,
    pub top: usize,
    pub orig: String,
    pub input: Option<String>,
    pub msg: String,
    pub rect: Rect,
    pub list_y: usize,
    pub rows: usize,
}

pub enum Action {
    None,
    Preview(String),
    Apply,
    Cancel,
    Dim(f32),
    Blur,
    Mode,
    Import(String),
}

pub fn items() -> Vec<Item> {
    let mut v = Vec::new();
    for id in builtin_ids() {
        if let Some(t) = load(id) {
            v.push(Item {
                kind: Kind::Builtin(id),
                label: t.label.clone(),
                desc: t.desc.clone(),
                swatch: Some(t.swatch()),
            });
        }
    }
    for (name, _) in crate::config::list_images() {
        let stem = name
            .rsplit_once('.')
            .map(|(s, _)| s.to_string())
            .unwrap_or(name.clone());
        // bundled pictures are named by their title ("Night city"), the
        // parenthetical / the rest of the credit goes to the description
        let (label, desc) = match credit(&name) {
            Some((title, cr)) => {
                let (t, extra) = match title.split_once(" (") {
                    Some((t, e)) => (t, e.trim_end_matches(')')),
                    None => (title, ""),
                };
                let who = cr.split(" · ").last().unwrap_or("");
                let d = if extra.is_empty() {
                    format!("picture · {who}")
                } else {
                    format!("picture · {extra} · {who}")
                };
                (t.to_string(), d)
            }
            None => (stem, "your picture · palette from the image".to_string()),
        };
        v.push(Item {
            kind: Kind::Image(name),
            label: format!("◆ {label}"),
            desc,
            swatch: None,
        });
    }
    v.push(Item {
        kind: Kind::Add,
        label: "+ Add image… (path or URL)".into(),
        desc: String::new(),
        swatch: None,
    });
    v
}

impl Menu {
    pub fn new(cur_id: &str) -> Menu {
        let items = items();
        let cur = items
            .iter()
            .position(|i| i.id().as_deref() == Some(cur_id))
            .unwrap_or(0);
        Menu {
            items,
            cur,
            top: 0,
            orig: cur_id.to_string(),
            input: None,
            msg: String::new(),
            rect: Rect::default(),
            list_y: 0,
            rows: 1,
        }
    }

    fn preview(&self) -> Action {
        match self.items[self.cur].id() {
            Some(id) => Action::Preview(id),
            None => Action::None,
        }
    }

    fn go(&mut self, d: isize) -> Action {
        let n = self.items.len() as isize;
        self.cur = ((self.cur as isize + d).rem_euclid(n)) as usize;
        self.msg.clear();
        self.preview()
    }

    pub fn key(&mut self, k: Key) -> Action {
        if let Some(inp) = &mut self.input {
            match k {
                Key::Esc => self.input = None,
                Key::Enter => {
                    let s = inp.trim().to_string();
                    self.input = None;
                    if !s.is_empty() {
                        return Action::Import(s);
                    }
                }
                Key::Backspace => {
                    inp.pop();
                }
                Key::CtrlU => inp.clear(),
                Key::Char(c) => inp.push(c),
                _ => {}
            }
            return Action::None;
        }
        match k {
            Key::Up | Key::Char('k') => self.go(-1),
            Key::Down | Key::Char('j') | Key::Tab => self.go(1),
            Key::BackTab => self.go(-1),
            Key::PgUp => self.go(-(self.rows as isize)),
            Key::PgDn => self.go(self.rows as isize),
            Key::Home => self.go(-(self.cur as isize)),
            Key::End => self.go((self.items.len() - 1 - self.cur) as isize),
            Key::Enter => {
                if self.items[self.cur].kind == Kind::Add {
                    self.input = Some(String::new());
                    Action::None
                } else {
                    Action::Apply
                }
            }
            Key::Esc | Key::Char('q') | Key::Char('t') => Action::Cancel,
            Key::Left | Key::Char('h') => Action::Dim(-0.1),
            Key::Right | Key::Char('l') => Action::Dim(0.1),
            Key::Char('b') => Action::Blur,
            Key::Char('m') => Action::Mode,
            Key::Char('a') => {
                self.input = Some(String::new());
                Action::None
            }
            _ => Action::None,
        }
    }

    pub fn click(&mut self, x: usize, y: usize) -> Action {
        if !self.rect.contains(x, y) {
            return Action::Cancel;
        }
        if y >= self.list_y && y < self.list_y + self.rows {
            let i = self.top + y - self.list_y;
            if i < self.items.len() {
                if i == self.cur {
                    return self.key(Key::Enter);
                }
                self.cur = i;
                return self.preview();
            }
        }
        Action::None
    }
}

impl App {
    pub fn draw_menu(&mut self, scr: &mut Screen) {
        let Some(m) = self.menu.as_mut() else { return };
        let th = &self.t;
        let (w, h) = (self.w, self.h);
        let n = m.items.len();
        let mw = w.saturating_sub(4).min(92);
        let mh = (n + 10).min(h.saturating_sub(2));
        if mw < 30 || mh < 10 {
            return;
        }
        let r = Rect {
            x: (w - mw) / 2,
            y: (h - mh) / 2,
            w: mw,
            h: mh,
        };
        m.rect = r;
        let bg = surf(S_DIALOG);
        scr.fill(r.x, r.y, r.w, r.h, Cell { ch: ' ', fg: 0, bg });
        let b = th.border_hi;
        let (x1, y1) = (r.x + r.w - 1, r.y + r.h - 1);
        for x in r.x + 1..x1 {
            scr.put(x, r.y, '─', b, bg);
            scr.put(x, y1, '─', b, bg);
        }
        for y in r.y + 1..y1 {
            scr.put(r.x, y, '│', b, bg);
            scr.put(x1, y, '│', b, bg);
        }
        scr.put(r.x, r.y, '╭', b, bg);
        scr.put(x1, r.y, '╮', b, bg);
        scr.put(r.x, y1, '╰', b, bg);
        scr.put(x1, y1, '╯', b, bg);
        scr.text(r.x + 1, r.y, " themes ", th.title | BOLD, bg, x1);
        let info = f!(
            " {} · {} ",
            self.mode_line.split(" (").next().unwrap_or(""),
            self.depth_name
        );
        let il = info.s().chars().count();
        if il + 12 < r.w {
            scr.text(x1 - 1 - il, r.y, info.s(), th.mid, bg, x1);
        }
        let ix = r.x + 2;
        let iw = r.w - 4;
        let maxx = ix + iw;
        // list
        let footer = 6;
        let rows = mh.saturating_sub(footer + 3).max(1);
        m.rows = rows;
        if m.cur < m.top {
            m.top = m.cur;
        } else if m.cur >= m.top + rows {
            m.top = m.cur + 1 - rows;
        }
        m.list_y = r.y + 2;
        let longest = m
            .items
            .iter()
            .filter(|i| i.kind != Kind::Add)
            .map(|i| i.label.chars().count())
            .max()
            .unwrap_or(20);
        let lw = (longest + 1).min(iw * 2 / 5);
        for k in 0..rows {
            let i = m.top + k;
            if i >= n {
                break;
            }
            let it = &m.items[i];
            let y = m.list_y + k;
            let sel = i == m.cur;
            let rb = if sel { surf(S_SELECT) } else { bg };
            if sel {
                scr.set_bg(r.x + 1, y, r.w - 2, 1, rb);
                scr.put(r.x + 1, y, '▌', th.accent, rb);
            }
            let active = it.id().as_deref() == Some(m.orig.as_str());
            let fg = if sel {
                th.fg | BOLD
            } else if active {
                th.accent
            } else {
                th.text
            };
            let lim = if it.kind == Kind::Add { iw } else { lw };
            if it.label.chars().count() > lim {
                let cut: String = it.label.chars().take(lim - 1).collect();
                let e = scr.text(ix, y, &cut, fg, rb, ix + lim);
                scr.text(e, y, "…", fg, rb, ix + lim);
            } else {
                scr.text(ix, y, &it.label, fg, rb, ix + lim);
            }
            let mut x = ix + lw + 1;
            if let Some(sw) = it.swatch {
                for (j, c) in sw.iter().take(4).enumerate() {
                    scr.put(x + 2 * j, y, '▀', *c, sw[4 + j] & 0xFF_FFFF);
                    scr.put(x + 2 * j + 1, y, '▀', *c, sw[4 + j] & 0xFF_FFFF);
                }
                x += 10;
            }
            if x < maxx {
                let d = &it.desc;
                let room = maxx - x;
                if d.chars().count() > room && room > 1 {
                    let cut: String = d.chars().take(room - 1).collect();
                    let e = scr.text(x, y, &cut, th.mid, rb, maxx);
                    scr.text(e, y, "…", th.mid, rb, maxx);
                } else {
                    scr.text(x, y, d, th.mid, rb, maxx);
                }
            }
        }
        // footer: separator, mode, background, credit, help
        let fy = y1 - footer;
        for x in r.x + 1..x1 {
            scr.put(x, fy, '─', th.border, bg);
        }
        let mut y = fy + 1;
        let ml = f!("{}", self.mode_line);
        App::spans(
            scr,
            ix,
            y,
            maxx,
            bg,
            &[("image mode ", th.dim), (ml.s(), th.fg)],
        );
        y += 1;
        let cur = &m.items[m.cur];
        match &cur.kind {
            Kind::Image(file) => {
                let dim = self.t.brightness;
                let bw = 12;
                let filled = ((dim - 0.2) / 0.8 * bw as f32).round() as usize;
                let mut x = scr.text(ix, y, "background ", th.dim, bg, maxx);
                for i in 0..bw {
                    scr.put(
                        x + i,
                        y,
                        '▄',
                        if i < filled { th.accent } else { TRACK },
                        bg,
                    );
                }
                x += bw;
                let p = crate::ui::num(" ", dim * 100.0, 0, "%");
                x = scr.text(x, y, p.s(), th.fg, bg, maxx);
                let bl = if self.t.soft_blur > 0.0 {
                    crate::ui::num("   blur ", self.t.soft_blur, 0, "")
                } else {
                    f!("   blur off")
                };
                scr.text(x, y, bl.s(), th.fg, bg, maxx);
                y += 1;
                let cr = credit(file)
                    .map(|c| c.1.to_string())
                    .unwrap_or_else(|| "your picture".into());
                App::spans(
                    scr,
                    ix,
                    y,
                    maxx,
                    bg,
                    &[("credit     ", th.dim), (&cr, th.text)],
                );
            }
            Kind::Builtin(_) => {
                App::spans(
                    scr,
                    ix,
                    y,
                    maxx,
                    bg,
                    &[("background ", th.dim), ("procedural", th.fg)],
                );
                y += 1;
                App::spans(
                    scr,
                    ix,
                    y,
                    maxx,
                    bg,
                    &[
                        ("credit     ", th.dim),
                        ("built-in theme (cpuview)", th.text),
                    ],
                );
            }
            Kind::Add => {
                App::spans(
                    scr,
                    ix,
                    y,
                    maxx,
                    bg,
                    &[
                        ("add        ", th.dim),
                        ("~/.config/cpuview/images/ is listed here too", th.text),
                    ],
                );
                y += 1;
                App::spans(
                    scr,
                    ix,
                    y,
                    maxx,
                    bg,
                    &[
                        ("from PC    ", th.dim),
                        ("scp .\\pic.jpg you@host:~/.config/cpuview/images/", th.text),
                    ],
                );
            }
        }
        y = fy + 4;
        if !m.msg.is_empty() {
            let msg = m.msg.clone();
            scr.text(ix, y, &msg, th.warn, bg, maxx);
        }
        y += 1;
        if let Some(inp) = &m.input {
            let mut x = scr.text(ix, y, "path or URL: ", th.accent | BOLD, bg, maxx);
            let room = maxx.saturating_sub(x + 1);
            let skip = inp.chars().count().saturating_sub(room);
            let shown: String = inp.chars().skip(skip).collect();
            x = scr.text(x, y, &shown, th.fg, bg, maxx);
            scr.put(x, y, '▏', th.accent, bg);
        } else {
            App::spans(
                scr,
                ix,
                y,
                maxx,
                bg,
                &[
                    ("↑↓", th.accent | BOLD),
                    (" preview  ", th.dim),
                    ("enter", th.accent | BOLD),
                    (" apply  ", th.dim),
                    ("esc", th.accent | BOLD),
                    (" cancel  ", th.dim),
                    ("←→", th.accent | BOLD),
                    (" dim  ", th.dim),
                    ("b", th.accent | BOLD),
                    (" blur  ", th.dim),
                    ("m", th.accent | BOLD),
                    (" mode  ", th.dim),
                    ("a", th.accent | BOLD),
                    (" add", th.dim),
                ],
            );
        }
    }
}
