//! ~/.config/cpuview/config: a tiny `key = value` file, plus image import (copy / curl).

use crate::probe::Mode;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct ImgOpt {
    pub file: String,
    pub dim: f32,
    pub blur: f32,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub theme: String,
    pub image_mode: Mode,
    pub sort: usize,
    pub view: usize,
    pub speed: usize,
    /// index into ui::PAGES (0 CPU, 1 memory & storage, 2 network & system, 3 processes)
    pub page: usize,
    pub imgs: Vec<ImgOpt>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: "default".into(),
            image_mode: Mode::Auto,
            sort: 0,
            view: 0,
            speed: 0,
            page: 0,
            imgs: Vec::new(),
        }
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn config_dir() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p).join("cpuview"),
        _ => home().join(".config/cpuview"),
    }
}

pub fn images_dir() -> PathBuf {
    config_dir().join("images")
}

/// Directories holding the bundled wallpapers, first existing wins.
pub fn wallpaper_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(p) = std::env::var_os("CPUVIEW_WALLPAPERS") {
        v.push(PathBuf::from(p));
    }
    let data = match std::env::var_os("XDG_DATA_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => home().join(".local/share"),
    };
    v.push(data.join("cpuview/wallpapers"));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            v.push(d.join("../share/cpuview/wallpapers"));
            v.push(d.join("../../wallpapers")); // cargo run from the repo
        }
    }
    v.push(PathBuf::from("/usr/share/cpuview/wallpapers"));
    v.push(PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/wallpapers"
    )));
    v.into_iter().filter(|p| p.is_dir()).collect()
}

pub fn is_image_name(n: &str) -> bool {
    let l = n.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg"].iter().any(|e| l.ends_with(e))
}

/// (display name, path): bundled wallpapers then the user's images folder.
pub fn list_images() -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let mut dirs = Vec::new();
    if let Some(d) = wallpaper_dirs().into_iter().next() {
        dirs.push(d);
    }
    dirs.push(images_dir());
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut v: Vec<(String, PathBuf)> = rd
            .filter_map(|e| e.ok())
            .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
            .filter(|(n, p)| is_image_name(n) && p.is_file() && !n.starts_with('.'))
            .collect();
        crate::sys::isort(&mut v, &|a, b| a.0 < b.0);
        for (n, p) in v {
            if !out.iter().any(|(m, _)| *m == n) {
                out.push((n, p));
            }
        }
    }
    out
}

/// Resolve `image:<name or path>` to a file.
pub fn resolve_image(spec: &str) -> Option<PathBuf> {
    let p = Path::new(spec);
    if p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let mut dirs = vec![images_dir()];
    dirs.extend(wallpaper_dirs());
    dirs.into_iter().map(|d| d.join(spec)).find(|p| p.is_file())
}

fn parse_f(v: &str, d: f32) -> f32 {
    crate::sys::pf(v).unwrap_or(d)
}

impl Config {
    pub fn path() -> PathBuf {
        config_dir().join("config")
    }

    pub fn load() -> Config {
        let mut c = Config::default();
        let Ok(text) = std::fs::read_to_string(Self::path()) else {
            return c;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim().trim_matches('"'));
            match k {
                "theme" => c.theme = v.to_string(),
                "image_mode" => c.image_mode = Mode::parse(v).unwrap_or(Mode::Auto),
                "sort" => c.sort = crate::ui::SORTS.iter().position(|s| *s == v).unwrap_or(0),
                "view" => c.view = crate::ui::VIEWS.iter().position(|s| *s == v).unwrap_or(0),
                "speed" => c.speed = crate::ui::speed_index(v),
                "page" => c.page = crate::ui::PAGES.iter().position(|p| *p == v).unwrap_or(0),
                _ => {
                    // image.<file>.dim / image.<file>.blur
                    if let Some(rest) = k.strip_prefix("image.") {
                        if let Some((file, opt)) = rest.rsplit_once('.') {
                            let o = c.img_mut(file);
                            match opt {
                                "dim" => o.dim = parse_f(v, 0.9).clamp(0.2, 1.0),
                                "blur" => o.blur = parse_f(v, 0.0).clamp(0.0, 8.0),
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        c
    }

    pub fn img(&self, file: &str) -> ImgOpt {
        self.imgs
            .iter()
            .find(|o| o.file == file)
            .cloned()
            .unwrap_or(ImgOpt {
                file: file.to_string(),
                dim: 0.9,
                blur: 0.0,
            })
    }

    pub fn img_mut(&mut self, file: &str) -> &mut ImgOpt {
        if let Some(i) = self.imgs.iter().position(|o| o.file == file) {
            return &mut self.imgs[i];
        }
        self.imgs.push(ImgOpt {
            file: file.to_string(),
            dim: 0.9,
            blur: 0.0,
        });
        self.imgs.last_mut().unwrap()
    }

    pub fn save(&self) {
        let mut s = String::from("# cpuview settings (written by cpuview)\n");
        s += &format!("theme = {}\n", self.theme);
        s += &format!("image_mode = {}\n", self.image_mode.name());
        s += &format!("sort = {}\n", crate::ui::SORTS[self.sort]);
        s += &format!("view = {}\n", crate::ui::VIEWS[self.view]);
        s += &format!("speed = {}\n", crate::ui::SPEEDS[self.speed].0);
        s += &format!("page = {}\n", crate::ui::PAGES[self.page.min(3)]);
        for o in &self.imgs {
            s += &format!(
                "image.{}.dim = {}.{}\nimage.{}.blur = {}\n",
                o.file,
                (o.dim * 10.0).round() as u32 / 10,
                (o.dim * 10.0).round() as u32 % 10,
                o.file,
                o.blur as u32
            );
        }
        let _ = std::fs::create_dir_all(config_dir());
        let tmp = Self::path().with_extension("tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(tmp, Self::path());
        }
    }
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() {
        "image".into()
    } else {
        s
    }
}

const MAX_BYTES: u64 = 25 * 1024 * 1024;

/// Run a program (found on PATH) with stdin / stdout / stderr on /dev/null and the
/// signal mask cleared; its exit status (-1: killed). posix_spawn instead of
/// std::process::Command, which costs ~16 KB of code.
fn run_quiet(argv: &[&str]) -> Result<i32, String> {
    use std::ffi::CString;
    let c: Vec<CString> = argv
        .iter()
        .map(|a| CString::new(*a).map_err(|_| "bad argument".to_string()))
        .collect::<Result<_, _>>()?;
    let mut p: Vec<*mut libc::c_char> = c.iter().map(|s| s.as_ptr() as *mut _).collect();
    p.push(std::ptr::null_mut());
    extern "C" {
        static environ: *const *mut libc::c_char;
    }
    unsafe {
        let mut fa: libc::posix_spawn_file_actions_t = std::mem::zeroed();
        let mut at: libc::posix_spawnattr_t = std::mem::zeroed();
        libc::posix_spawn_file_actions_init(&mut fa);
        for fd in 0..3 {
            libc::posix_spawn_file_actions_addopen(
                &mut fa,
                fd,
                c"/dev/null".as_ptr(),
                libc::O_RDWR,
                0,
            );
        }
        libc::posix_spawnattr_init(&mut at);
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        libc::posix_spawnattr_setsigmask(&mut at, &none);
        libc::posix_spawnattr_setflags(&mut at, libc::POSIX_SPAWN_SETSIGMASK as _);
        let mut pid = 0;
        let r = libc::posix_spawnp(&mut pid, p[0], &fa, &at, p.as_ptr(), environ);
        libc::posix_spawn_file_actions_destroy(&mut fa);
        libc::posix_spawnattr_destroy(&mut at);
        if r != 0 {
            return Err(std::io::Error::from_raw_os_error(r).to_string());
        }
        let mut st = 0;
        while libc::waitpid(pid, &mut st, 0) < 0 {
            if *libc::__errno_location() != libc::EINTR {
                return Err("wait failed".into());
            }
        }
        Ok(if libc::WIFEXITED(st) {
            libc::WEXITSTATUS(st)
        } else {
            -1
        })
    }
}

/// Copy (path) or download (http/https URL, via curl) a picture into the images
/// folder after checking that it decodes. Returns the stored file name.
pub fn import_image(src: &str) -> Result<String, String> {
    let dir = images_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let is_url = src.starts_with("http://") || src.starts_with("https://");
    let tmp = dir.join(format!(".download-{}", std::process::id()));
    let base = if is_url {
        let path = src.split(['?', '#']).next().unwrap_or("");
        let last = path.rsplit('/').next().unwrap_or("");
        let max = MAX_BYTES.to_string();
        let t = tmp.to_string_lossy();
        let st = run_quiet(&[
            "curl",
            "-fsSL",
            "--proto",
            "=http,https",
            "--proto-redir",
            "=http,https",
            "--max-time",
            "15",
            "--max-filesize",
            &max,
            "-o",
            &t,
            src,
        ])
        .map_err(|e| format!("curl: {e} (is curl installed?)"))?;
        if st != 0 {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("download failed (curl exit {st})"));
        }
        last.to_string()
    } else {
        let p = if let Some(r) = src.strip_prefix("~/") {
            home().join(r)
        } else {
            PathBuf::from(src)
        };
        let md = std::fs::metadata(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        if md.len() > MAX_BYTES {
            return Err("file is larger than 25 MB".into());
        }
        std::fs::copy(&p, &tmp).map_err(|e| format!("{}: {e}", p.display()))?;
        p.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    };
    let check = (|| {
        let md = std::fs::metadata(&tmp).map_err(|e| e.to_string())?;
        if md.len() > MAX_BYTES {
            return Err("file is larger than 25 MB".to_string());
        }
        let data = std::fs::read(&tmp).map_err(|e| e.to_string())?;
        let img = crate::image::decode_bytes(&data)?;
        if img.w < 2 || img.h < 2 {
            return Err("image too small".to_string());
        }
        Ok(if data.starts_with(b"\x89PNG") {
            "png"
        } else {
            "jpg"
        })
    })();
    let ext = match check {
        Ok(e) => e,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    let stem = sanitize(base.rsplit_once('.').map(|(s, _)| s).unwrap_or(&base));
    let mut name = format!("{stem}.{ext}");
    // same content already present? reuse it
    let mut n = 1;
    while dir.join(&name).exists() {
        if std::fs::read(dir.join(&name)).ok() == std::fs::read(&tmp).ok() {
            let _ = std::fs::remove_file(&tmp);
            return Ok(name);
        }
        name = format!("{stem}-{n}.{ext}");
        n += 1;
    }
    std::fs::rename(&tmp, dir.join(&name)).map_err(|e| e.to_string())?;
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sanitize_names() {
        assert_eq!(sanitize("my pic (1)"), "mypic1");
        assert_eq!(sanitize("../../etc/passwd"), "etcpasswd");
        assert_eq!(sanitize("..."), "image");
    }
}
