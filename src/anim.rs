//! Smooth-mode animation primitives: critically damped springs for bars, hysteresis
//! for the digits, and history curves with a live head for scrolling graphs.
//!
//! Smooth mode renders on a fixed frame grid (`FPS`). Every frame advances the
//! animation by exactly one grid interval, so motion per frame is uniform whatever the
//! wakeup jitter. Samples (10/s) only move the springs' targets; the springs filter
//! the 10 Hz staircase of targets into C1-continuous motion (velocity never jumps), and
//! with ω ≈ 14 rad/s ≪ 2π·10 the residual 10 Hz velocity ripple is under ~10 %.

/// Animation frames per second (a fixed grid; halved only if the CPU budget is
/// exceeded for several seconds, e.g. on huge screens).
pub const FPS: i64 = 60;
/// History bodies (graphs, strips, heatmap) are recomputed every GRAPH_DIV frames
/// (15 times a second: they scroll by one braille dot a second, so that is ample),
/// each kind on a different frame of the cycle so every frame costs about the same.
pub const GRAPH_DIV: u64 = 4;
/// Spring angular frequencies (rad/s): a step settles to 90 % in ~3.9 / ω s.
pub const W_LOAD: f32 = 14.0;
pub const W_CLOCK: f32 = 8.0;
pub const W_TEMP: f32 = 4.0;

/// One critically damped spring step of `dt` s toward `target` (exact for a fixed
/// target; `ex` = exp(-w·dt)). Snaps and returns false once within `eps` and slow.
#[inline]
pub fn spring(x: &mut f32, v: &mut f32, target: f32, dt: f32, w: f32, ex: f32, eps: f32) -> bool {
    let e = *x - target;
    if e.abs() < eps && v.abs() < eps * w {
        *x = target;
        *v = 0.0;
        return false;
    }
    let k = (*v + w * e) * dt;
    *x = target + (e + k) * ex;
    *v = (*v - w * k) * ex;
    true
}

/// An animated quantity (memory view): spring position `x` and velocity `v` towards
/// `to`, and the number shown `n` following `x` like the core digits (`App::anim_numbers`).
#[derive(Clone, Copy, Default, Debug)]
pub struct Val {
    pub x: f32,
    pub v: f32,
    pub to: f32,
    pub n: f32,
    age: f32,
}

impl Val {
    /// New target; other speeds than smooth jump to it.
    pub fn set(&mut self, to: f32, smooth: bool) {
        self.to = to;
        if !smooth {
            *self = Val {
                x: to,
                to,
                n: to,
                ..Default::default()
            };
        }
    }
    /// Spring step (`ex` = exp(-w·dt)); settles within `eps` (+ 0.1 % of the target).
    pub fn step(&mut self, dt: f32, w: f32, ex: f32, eps: f32) -> bool {
        spring(
            &mut self.x,
            &mut self.v,
            self.to,
            dt,
            w,
            ex,
            eps + self.to.abs() * 1e-3,
        )
    }
    /// The shown number: at most 4 changes a second while `x` wanders, every frame
    /// while it moves faster than half of `full` a second, exactly the target at rest.
    /// `unit`: the display resolution at the current value.
    pub fn digits(&mut self, dt: f32, unit: f32, full: f32) -> bool {
        self.age += dt;
        let rest = self.v == 0.0 && self.x == self.to;
        if !(rest || self.v.abs() >= 0.5 * full || self.age >= 0.25) {
            return false;
        }
        let ch = hyst(
            &mut self.n,
            self.x,
            unit.max(1e-9),
            if rest { 0.0 } else { 1.0 },
        );
        if rest && self.n != self.x && (self.n - self.x).abs() < unit {
            // land on the value itself (the formatter rounds it)
            self.n = self.x;
        }
        if ch {
            self.age = 0.0;
        }
        ch
    }
}

impl Val {
    /// The shown number of a noisy rate: changes at most twice a second, and only by
    /// more than `unit` (small wobbles are held, so it can be read); at rest it
    /// lands on the target (also no sooner than half a second after the last change).
    pub fn digits_hold(&mut self, dt: f32, unit: f32) -> bool {
        self.age += dt;
        if self.v == 0.0 && self.x == self.to {
            let ch = self.n != self.x && self.age >= 0.5;
            if ch {
                self.n = self.x;
                self.age = 0.0;
            }
            return ch;
        }
        if self.age < 0.5 || (self.x - self.n).abs() < unit {
            return false;
        }
        self.n = self.x;
        self.age = 0.0;
        true
    }
}

/// Digits that follow a moving value: `shown` (a multiple of `unit`) changes only
/// once `x` is more than (0.5 + h) units away, so a value hovering at a rounding
/// boundary doesn't flicker, and a moving one counts monotonically. Returns true
/// when the shown value changed.
#[inline]
pub fn hyst(shown: &mut f32, x: f32, unit: f32, h: f32) -> bool {
    if (x - *shown).abs() < unit * (0.5 + h) {
        return false;
    }
    let n = (x / unit).round() * unit;
    let changed = n != *shown;
    *shown = n;
    changed
}

/// A history series as a function of age (seconds before now): the live value at age 0
/// and point k (k = 0 newest, one per second) at age 1 + k, linear in between.
///
/// The points sit at whole-second ages and move one step (one braille dot column)
/// when a new point is pushed; only the head moves continuously. (Sliding them by the
/// sub-second phase instead made every column alternate between "a point" and "the
/// mean of two neighbours" once a second: one-point peaks shrank and regrew and
/// one-dot features blinked off and on — amplitude aliasing, not motion.)
pub struct Curve<'a> {
    pub head: f32,
    pub len: usize,
    pub pt: &'a dyn Fn(usize) -> f32,
    /// value before the history starts
    pub pad: f32,
}

impl Curve<'_> {
    /// `n` samples at ages off, off + step, ...; written oldest first into `out`
    /// (`out[n - 1]` is the youngest).
    pub fn sample(&self, out: &mut Vec<f32>, n: usize, off: f32, step: f32) {
        out.clear();
        out.resize(n, self.pad);
        let at = |k: usize| -> f32 {
            if k == 0 {
                self.head
            } else {
                (self.pt)(k - 1)
            }
        };
        for j in 0..n {
            let a = off + step * j as f32;
            let k = a.floor().max(0.0) as usize;
            if k > self.len {
                break; // older than the history: pad
            }
            let f = a - k as f32;
            let v = if f <= 0.0 || k == self.len {
                at(k)
            } else {
                at(k) + (at(k + 1) - at(k)) * f
            };
            out[n - 1 - j] = v;
        }
    }
}

/// The next frame time on the `fps` grid strictly after `t` (ns, any epoch).
#[inline]
pub fn next_frame(t: i64, fps: i64) -> i64 {
    let k = t.div_euclid(1_000_000_000) * fps + t.rem_euclid(1_000_000_000) * fps / 1_000_000_000;
    let at = |k: i64| k.div_euclid(fps) * 1_000_000_000 + k.rem_euclid(fps) * 1_000_000_000 / fps;
    let mut k = k;
    while at(k) <= t {
        k += 1;
    }
    at(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spring_is_c1_and_settles_without_overshoot() {
        let (mut x, mut v) = (0.0f32, 0.0f32);
        let dt = 1.0 / FPS as f32;
        let ex = (-W_LOAD * dt).exp();
        let (mut prev, mut pv, mut n) = (0.0, 0.0f32, 0);
        while spring(&mut x, &mut v, 1.0, dt, W_LOAD, ex, 0.001) {
            assert!(
                x >= prev - 1e-6 && x <= 1.0 + 1e-4,
                "monotone, no overshoot"
            );
            // velocity changes by a bounded amount per frame (no jumps)
            assert!((v - pv).abs() <= W_LOAD * W_LOAD * dt * 1.01 + 1e-3);
            (prev, pv) = (x, v);
            n += 1;
            assert!(n < 120);
        }
        assert_eq!(x, 1.0);
        // 90 % within ~0.28 s
        let (mut x, mut v, mut t) = (0.0f32, 0.0f32, 0.0);
        while x < 0.9 {
            spring(&mut x, &mut v, 1.0, dt, W_LOAD, ex, 1e-6);
            t += dt;
        }
        assert!(t < 0.3, "{t}");
    }

    #[test]
    fn spring_moves_steadily_through_a_staircase() {
        // targets stepping 10 times a second along a ramp: per-frame motion stays
        // within ±15 % of its mean once moving (no stop-and-go)
        let dt = 1.0 / FPS as f32;
        let ex = (-W_LOAD * dt).exp();
        let (mut x, mut v) = (0.0f32, 0.0f32);
        let mut d = Vec::new();
        for f in 0..(4 * FPS) {
            let t = f as f32 * dt;
            let target = ((t * 10.0).floor() / 10.0) * 0.2;
            let p = x;
            spring(&mut x, &mut v, target, dt, W_LOAD, ex, 1e-6);
            if t > 1.0 {
                d.push(x - p);
            }
        }
        let m = d.iter().sum::<f32>() / d.len() as f32;
        assert!(d.iter().all(|&s| (s - m).abs() < 0.15 * m), "{m} {d:?}");
    }

    #[test]
    fn digits_change_monotonically_and_ignore_jitter() {
        let mut s = 30.0;
        // jitter around a rounding boundary: no change
        for k in 0..100 {
            assert!(!hyst(
                &mut s,
                30.5 + if k % 2 == 0 { 0.2 } else { -0.2 },
                1.0,
                0.25
            ));
        }
        // a ramp counts up one by one (or faster), never back
        let mut prev = s;
        for k in 0..200 {
            hyst(&mut s, 30.0 + k as f32 * 0.25, 1.0, 0.25);
            assert!(s >= prev);
            prev = s;
        }
        assert_eq!(s, 80.0);
    }

    #[test]
    fn curve_points_sit_on_whole_seconds_and_never_breathe() {
        let pts = [10.0f32, 20.0, 0.0, 40.0];
        let f = |k: usize| pts[k];
        let c = Curve {
            head: 5.0,
            len: pts.len(),
            pt: &f,
            pad: 0.0,
        };
        let mut out = Vec::new();
        // column j at age j: the live value, then every point exactly (a one-point dip
        // stays a full dip), then padding
        c.sample(&mut out, 6, 0.0, 1.0);
        assert_eq!(out, vec![0.0, 40.0, 0.0, 20.0, 10.0, 5.0]);
        // between whole ages: linear (strip / heatmap buckets)
        c.sample(&mut out, 2, 0.5, 1.0);
        assert_eq!(out, vec![15.0, 7.5]);
    }

    #[test]
    fn held_rate_digits_change_at_most_twice_a_second() {
        let dt = 1.0 / FPS as f32;
        let ex = (-W_LOAD * dt).exp();
        let mut v = Val::default();
        v.set(100.0, false);
        let (mut last, mut changes, mut t) = (0.0f32, Vec::new(), 0.0f32);
        for f in 0..(6 * FPS) {
            // a noisy rate: a new target every sample, sometimes settling
            if f % 6 == 0 {
                v.set(if (f / 6) % 3 == 0 { 100.0 } else { 140.0 }, true);
            }
            v.step(dt, W_LOAD, ex, 0.01);
            if v.digits_hold(dt, (v.x * 0.04).max(1.0)) {
                assert!(t - last >= 0.5 - 1e-3 || changes.is_empty(), "{t} {last}");
                last = t;
                changes.push(t);
            }
            t += dt;
        }
        assert!(!changes.is_empty());
    }

    #[test]
    fn frame_grid() {
        let f = 1_000_000_000 / 60;
        assert_eq!(next_frame(0, 60), f);
        assert_eq!(next_frame(f, 60), 2 * f + 1); // 33_333_333
        assert_eq!(next_frame(999_999_999, 60), 1_000_000_000);
        // samples at 0.1 s fall exactly on frames
        assert_eq!(next_frame(99_999_999, 60), 100_000_000);
        assert_eq!(next_frame(-1, 30), 0);
    }
}
