//! Smooth-mode animation primitives: critically damped springs for bars, hysteresis
//! for the digits, and continuous-time history curves for scrolling graphs.
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

/// A history series as a continuous function of age (seconds before now): the live
/// value at age 0, the current second's partial mean at age φ/2 (faded in over the
/// first half second, so a fresh second doesn't start from one noisy sample), and
/// point k (k = 0 newest, one per second) at age φ + 0.5 + k, where φ is the time
/// since the newest point. Linear in between. As φ grows every point slides to older
/// ages, so a graph sampled at fixed ages scrolls continuously and its newest part
/// grows in from the live value.
pub struct Curve<'a> {
    pub phase: f32,
    pub head: f32,
    pub part: f32,
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
        let ph = self.phase.clamp(0.0, 1.0);
        // knot list as (age, value), walked in step with increasing sample ages
        let knot = |k: usize| -> (f32, f32) {
            if k == 0 {
                (0.0, self.head)
            } else if k == 1 {
                let a = ph * 0.5;
                (a, self.head + (self.part - self.head) * (ph * 2.0).min(1.0))
            } else {
                let j = k - 2;
                (ph + 0.5 + j as f32, (self.pt)(j))
            }
        };
        let nk = self.len + 2;
        let mut k = 0;
        let (mut a0, mut v0) = knot(0);
        let (mut a1, mut v1) = knot(1);
        for j in 0..n {
            let a = off + step * j as f32;
            while a > a1 && k + 2 < nk {
                k += 1;
                (a0, v0) = (a1, v1);
                (a1, v1) = knot(k + 1);
            }
            if a > a1 {
                break; // older than the history: pad
            }
            let v = if a1 > a0 {
                v0 + (v1 - v0) * ((a - a0) / (a1 - a0)).clamp(0.0, 1.0)
            } else {
                v1
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
    fn curve_scrolls_continuously_and_meets_the_points() {
        let pts = [10.0f32, 20.0, 30.0, 40.0];
        let f = |k: usize| pts[k];
        let mk = |phase: f32| Curve {
            phase,
            head: 5.0,
            part: 5.0,
            len: pts.len(),
            pt: &f,
            pad: 0.0,
        };
        let mut out = Vec::new();
        // at φ = 0.5 point k sits at age 1 + k: dot columns at integer ages hit them
        mk(0.5).sample(&mut out, 6, 0.0, 1.0);
        assert_eq!(out, vec![0.0, 40.0, 30.0, 20.0, 10.0, 5.0]);
        // between: every column moves a little, continuously, towards its older neighbour
        let mut prev = Vec::new();
        mk(0.5).sample(&mut prev, 6, 0.0, 1.0);
        for i in 1..=10 {
            mk(0.5 + i as f32 * 0.04).sample(&mut out, 6, 0.0, 1.0);
            for j in 2..5 {
                assert!(
                    out[j] <= prev[j] + 1e-4 && prev[j] - out[j] < 0.5,
                    "{out:?} {prev:?}"
                );
            }
            prev.clone_from(&out);
        }
        // the newest point grows in from the live value, then the partial mean
        let c = Curve {
            phase: 0.0,
            head: 50.0,
            part: 0.0,
            len: 1,
            pt: &|_| 10.0,
            pad: 0.0,
        };
        c.sample(&mut out, 2, 0.0, 1.0);
        assert_eq!(out[1], 50.0);
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
