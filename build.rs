//! Embeds the built-in theme definitions (`themes/*.toml`) and the wallpaper credits
//! (`wallpapers/credits.toml`) as static Rust data, so the binary needs no TOML parser.
//! The parser below handles the subset of TOML those files use: tables, arrays of
//! tables, quoted keys, strings, numbers, booleans, (nested / multi-line) arrays and
//! inline tables.

use std::fmt::Write as _;
use std::path::Path;

#[derive(Debug, Clone)]
enum V {
    N(f64),
    S(String),
    B(bool),
    A(Vec<V>),
    T(Vec<(String, V)>),
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    file: String,
}

impl<'a> Parser<'a> {
    fn err(&self, m: &str) -> ! {
        let line = self.s[..self.i].iter().filter(|&&c| c == b'\n').count() + 1;
        panic!("{}:{}: {}", self.file, line, m)
    }
    fn peek(&self) -> u8 {
        *self.s.get(self.i).unwrap_or(&0)
    }
    fn ws(&mut self, newlines: bool) {
        loop {
            match self.peek() {
                b' ' | b'\t' | b'\r' => self.i += 1,
                b'\n' if newlines => self.i += 1,
                b'#' => {
                    while self.peek() != b'\n' && self.peek() != 0 {
                        self.i += 1
                    }
                }
                _ => break,
            }
        }
    }
    fn key(&mut self) -> String {
        self.ws(false);
        if self.peek() == b'"' {
            return self.string();
        }
        let st = self.i;
        while matches!(self.peek(), b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'.') {
            self.i += 1
        }
        if st == self.i {
            self.err("expected key")
        }
        String::from_utf8(self.s[st..self.i].to_vec()).unwrap()
    }
    fn string(&mut self) -> String {
        self.i += 1; // opening quote
        let mut out = Vec::new();
        loop {
            let c = self.peek();
            self.i += 1;
            match c {
                0 | b'\n' => self.err("unterminated string"),
                b'"' => break,
                b'\\' => {
                    let e = self.peek();
                    self.i += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'u' => {
                            let h = std::str::from_utf8(&self.s[self.i..self.i + 4]).unwrap();
                            self.i += 4;
                            let ch = char::from_u32(u32::from_str_radix(h, 16).unwrap()).unwrap();
                            let mut b = [0; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
                        }
                        _ => self.err("bad escape"),
                    }
                }
                c => out.push(c),
            }
        }
        String::from_utf8(out).unwrap()
    }
    fn value(&mut self) -> V {
        self.ws(false);
        match self.peek() {
            b'"' => V::S(self.string()),
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                loop {
                    self.ws(true);
                    if self.peek() == b']' {
                        self.i += 1;
                        break;
                    }
                    items.push(self.value());
                    self.ws(true);
                    match self.peek() {
                        b',' => self.i += 1,
                        b']' => {}
                        _ => self.err("expected , or ]"),
                    }
                }
                V::A(items)
            }
            b'{' => {
                self.i += 1;
                let mut kv = Vec::new();
                loop {
                    self.ws(false);
                    if self.peek() == b'}' {
                        self.i += 1;
                        break;
                    }
                    let k = self.key();
                    self.ws(false);
                    if self.peek() != b'=' {
                        self.err("expected =")
                    }
                    self.i += 1;
                    let v = self.value();
                    kv.push((k, v));
                    self.ws(false);
                    match self.peek() {
                        b',' => self.i += 1,
                        b'}' => {}
                        _ => self.err("expected , or }"),
                    }
                }
                V::T(kv)
            }
            b't' if self.s[self.i..].starts_with(b"true") => {
                self.i += 4;
                V::B(true)
            }
            b'f' if self.s[self.i..].starts_with(b"false") => {
                self.i += 5;
                V::B(false)
            }
            _ => {
                let st = self.i;
                while matches!(
                    self.peek(),
                    b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E' | b'_'
                ) {
                    self.i += 1
                }
                let t: String = std::str::from_utf8(&self.s[st..self.i])
                    .unwrap()
                    .replace('_', "");
                V::N(t.parse().unwrap_or_else(|_| self.err("bad number")))
            }
        }
    }
    fn doc(&mut self) -> Vec<(String, V)> {
        let mut root: Vec<(String, V)> = Vec::new();
        // current target: None = root, Some((key, is_array))
        let mut cur: Option<(String, bool)> = None;
        loop {
            self.ws(true);
            if self.peek() == 0 {
                break;
            }
            if self.peek() == b'[' {
                let arr = self.s.get(self.i + 1) == Some(&b'[');
                self.i += if arr { 2 } else { 1 };
                let k = self.key();
                self.ws(false);
                self.i += if arr { 2 } else { 1 };
                if arr {
                    match root.iter_mut().find(|(n, _)| *n == k) {
                        Some((_, V::A(v))) => v.push(V::T(Vec::new())),
                        _ => root.push((k.clone(), V::A(vec![V::T(Vec::new())]))),
                    }
                } else {
                    root.push((k.clone(), V::T(Vec::new())));
                }
                cur = Some((k, arr));
                continue;
            }
            let k = self.key();
            self.ws(false);
            if self.peek() != b'=' {
                self.err("expected =")
            }
            self.i += 1;
            let v = self.value();
            match &cur {
                None => root.push((k, v)),
                Some((name, arr)) => {
                    let slot = root.iter_mut().rev().find(|(n, _)| n == name).unwrap();
                    let t = match (&mut slot.1, arr) {
                        (V::A(items), true) => items.last_mut().unwrap(),
                        (t, false) => t,
                        _ => unreachable!(),
                    };
                    if let V::T(kv) = t {
                        kv.push((k, v))
                    }
                }
            }
        }
        root
    }
}

fn emit(v: &V, out: &mut String) {
    match v {
        V::N(n) => {
            let _ = write!(out, "P::N({:?})", n);
        }
        V::S(s) => {
            let _ = write!(out, "P::S({:?})", s);
        }
        V::B(b) => {
            let _ = write!(out, "P::B({})", b);
        }
        V::A(items) => {
            out.push_str("P::A(&[");
            for it in items {
                emit(it, out);
                out.push(',');
            }
            out.push_str("])");
        }
        V::T(kv) => {
            out.push_str("P::T(&[");
            for (k, v) in kv {
                let _ = write!(out, "({:?},", k);
                emit(v, out);
                out.push_str("),");
            }
            out.push_str("])");
        }
    }
}

fn parse(path: &Path) -> V {
    let src = std::fs::read(path).unwrap();
    let mut p = Parser {
        s: &src,
        i: 0,
        file: path.display().to_string(),
    };
    V::T(p.doc())
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tdir = root.join("themes");
    println!("cargo:rerun-if-changed=themes");
    println!("cargo:rerun-if-changed=wallpapers/credits.toml");
    let mut names: Vec<_> = std::fs::read_dir(&tdir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    names.sort();
    let mut out = String::from("pub static THEMES: &[P] = &[\n");
    for p in &names {
        println!("cargo:rerun-if-changed={}", p.display());
        emit(&parse(p), &mut out);
        out.push_str(",\n");
    }
    out.push_str("];\n");
    let credits = root.join("wallpapers/credits.toml");
    out.push_str("pub static CREDITS: P = ");
    if credits.exists() {
        emit(&parse(&credits), &mut out);
    } else {
        out.push_str("P::T(&[])");
    }
    out.push_str(";\n");
    let dst = Path::new(&std::env::var("OUT_DIR").unwrap()).join("themes_gen.rs");
    std::fs::write(dst, out).unwrap();
}
