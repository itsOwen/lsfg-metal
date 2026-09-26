// pacer: interval estimator and display slot fitter for adaptive pacing

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub interval: f64,
    pub trusted: bool,
}

// frame interval from the gap between presents, distrusted when the caller blocked for much of it
#[derive(Default)]
pub struct Estimator {
    previous: Option<f64>,
}

impl Estimator {
    pub fn sample(&mut self, now: f64, blocked: f64) -> Sample {
        let s = match self.previous {
            None => Sample {
                interval: 0.0,
                trusted: false,
            },
            Some(prev) => {
                let interval = now - prev;
                Sample {
                    interval,
                    trusted: blocked < f64::max(0.001, interval * 0.1),
                }
            }
        };
        self.previous = Some(now);
        s
    }
}

const SMOOTHING: f64 = 0.2;
const SNAP: f64 = 0.1;
// how close a ratio must be to a simple fraction for its slot pattern to lock to it
const FRACTION: f64 = 0.02;
const CAP_AFTER: u32 = 10;
const PROBE_AFTER: u32 = 60;
// frames a probe lasts; the first carries the game's catch-up after the held frames and is dropped
const PROBE_SAMPLES: usize = 4;
// tolerance on slot <= ratio so accumulated phase rounding cannot produce a near-zero slot
const EPS: f64 = 1e-9;

// fits generated frames into display slots from the smoothed frame interval
pub struct Pacer {
    refresh: f64,
    cap: usize,
    estimate: f64,
    cadence: f64,
    untrusted: u32,
    probing: u32,
    probes: Vec<f64>,
    phase: f64,
    // slot counts of the last frames, which may still be queued ahead of this one
    shown: [usize; 4],
    histogram: [u32; 9],
}

impl Pacer {
    pub fn new(refresh: f64, cap: u32) -> Self {
        let refresh = if refresh.is_finite() && refresh > 0.0 {
            refresh
        } else {
            1.0 / 60.0
        };
        Self {
            refresh,
            cap: cap.max(1) as usize,
            estimate: 0.0,
            cadence: 0.0,
            untrusted: 0,
            probing: 0,
            probes: Vec::new(),
            phase: 1.0,
            shown: [0; 4],
            histogram: [0; 9],
        }
    }

    pub fn interval(&self) -> f64 {
        self.estimate
    }

    // timestamps in (0, 1]; a last value of 1 means "show the source frame in that slot"
    pub fn slots(&mut self, sample: Sample) -> Vec<f64> {
        let interval = sample.interval;
        if !interval.is_finite()
            || interval <= 0.0
            || interval > f64::max(0.2, self.refresh * self.cap as f64 * 4.0)
        {
            return self.lock(1);
        }
        // a frame slower than any queued frame's slots left the display idle, so it was not ahead
        let queued = self.shown.iter().max().copied().unwrap_or(0);
        let behind = queued > 0 && interval > (queued as f64 + 0.5) * self.refresh;
        if sample.trusted || behind {
            self.untrusted = 0;
            self.cadence = 0.0;
            if self.probing > 0 {
                self.probes.push(interval);
                if self.probes.len() < PROBE_SAMPLES {
                    return self.lock(1);
                }
                let rest = &mut self.probes[1..];
                rest.sort_by(f64::total_cmp);
                self.estimate = rest[rest.len() / 2];
                self.probes.clear();
                self.probing = 0;
            } else if self.estimate == 0.0 {
                self.estimate = interval;
            } else {
                self.estimate = self.estimate * (1.0 - SMOOTHING) + interval * SMOOTHING;
            }
        } else {
            self.cadence = if self.cadence == 0.0 {
                interval
            } else {
                self.cadence * (1.0 - SMOOTHING) + interval * SMOOTHING
            };
            self.untrusted += 1;
            if self.estimate == 0.0 || (self.untrusted >= CAP_AFTER && self.cadence < self.estimate)
            {
                self.estimate = self.cadence;
            }
            // a present thread blocked through the whole probe never yields a trusted sample
            if self.probing as usize >= PROBE_SAMPLES {
                self.probing = 0;
                self.probes.clear();
                self.untrusted = 0;
            } else if self.untrusted >= PROBE_AFTER || self.probing > 0 {
                self.probing += 1;
                return self.lock(1);
            }
        }
        let mut ratio = self.estimate / self.refresh;
        let rounded = ratio.round();
        if (ratio - rounded).abs() < SNAP || rounded >= self.cap as f64 {
            return self.lock((rounded as usize).clamp(1, self.cap));
        }
        // a ratio near a/b keeps the phase on multiples of 1/b, so one frame in b ends on its original
        if let Some((a, b)) = (2..=5).map(f64::from).find_map(|b| {
            let a = (ratio * b).round();
            ((ratio - a / b).abs() < FRACTION).then_some((a, b))
        }) {
            ratio = a / b;
            let j = (self.phase * b).round();
            self.phase = if j < 1.0 { 1.0 } else { j / b };
        }
        let mut out = Vec::new();
        let mut slot = self.phase;
        while slot <= ratio + EPS && out.len() < self.cap {
            out.push(f64::min(slot / ratio, 1.0));
            slot += 1.0;
        }
        self.phase = if slot - ratio > EPS {
            slot - ratio
        } else {
            1.0
        };
        if out.is_empty() {
            return self.lock(1);
        }
        if let Some(last) = out.last_mut().filter(|l| **l > 1.0 - SNAP) {
            *last = 1.0;
        }
        self.count(out)
    }

    fn lock(&mut self, n: usize) -> Vec<f64> {
        self.phase = 1.0;
        self.count((1..=n).map(|i| i as f64 / n as f64).collect())
    }

    fn count(&mut self, out: Vec<f64>) -> Vec<f64> {
        self.shown.rotate_right(1);
        self.shown[0] = out.len();
        self.histogram[out.len().min(8)] += 1;
        out
    }

    // "<n>:<count>" pairs for non-zero counts, e.g. "1:40 2:20"; clears the counts
    pub fn histogram(&mut self) -> String {
        let s = (1..=8)
            .filter(|&n| self.histogram[n] > 0)
            .map(|n| format!("{n}:{}", self.histogram[n]))
            .collect::<Vec<_>>()
            .join(" ");
        self.histogram = [0; 9];
        s
    }
}

// a refresh interval in seconds as the nanoseconds vulkan reports; never zero, games divide by it
pub fn refresh_nanos(secs: f64) -> u64 {
    if secs.is_finite() && secs > 0.0 {
        ((secs * 1e9).round() as u64).max(1)
    } else {
        16_666_667
    }
}

// seconds per display frame: 1/LSFGM_TARGET_FPS, else the main screen's maximum rate, else 60
pub fn display_refresh() -> f64 {
    let target = std::env::var("LSFGM_TARGET_FPS")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        // beyond real displays the pacing math turns seconds into durations that overflow
        .filter(|f| (1.0..=1000.0).contains(f));
    1.0 / target.unwrap_or_else(|| {
        let fps = main_screen_fps();
        if fps <= 0 {
            60.0
        } else {
            fps as f64
        }
    })
}

// resolved at runtime; the class is only there once appkit is loaded
fn main_screen_fps() -> isize {
    use objc2::runtime::{AnyClass, AnyObject};
    let Some(cls) = AnyClass::get(c"NSScreen") else {
        return 0;
    };
    unsafe {
        let screen: *mut AnyObject = objc2::msg_send![cls, mainScreen];
        // maximumFramesPerSecond is macos 12 or newer; on 11 the 0 below makes the caller use 60
        let known: bool = !screen.is_null()
            && objc2::msg_send![screen, respondsToSelector: objc2::sel!(maximumFramesPerSecond)];
        if !known {
            0
        } else {
            objc2::msg_send![screen, maximumFramesPerSecond]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_nanos_converts_seconds_and_never_returns_zero() {
        assert_eq!(refresh_nanos(1.0 / 60.0), 16_666_667);
        assert_eq!(refresh_nanos(1.0 / 120.0), 8_333_333);
        // a bad reading must not hand the game a zero to divide by
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(refresh_nanos(bad), 16_666_667, "bad input {bad}");
        }
        assert!(refresh_nanos(1e-12) >= 1);
    }

    const REFRESH: f64 = 1.0 / 60.0;

    fn trusted(interval: f64) -> Sample {
        Sample {
            interval,
            trusted: true,
        }
    }

    fn untrusted(interval: f64) -> Sample {
        Sample {
            interval,
            trusted: false,
        }
    }

    fn check(p: &mut Pacer, s: Sample) -> Vec<f64> {
        let out = p.slots(s);
        assert!(!out.is_empty() && out.len() <= p.cap, "{out:?}");
        assert!(out.windows(2).all(|w| w[0] < w[1]), "{out:?}");
        assert!(out.iter().all(|&t| t > 0.0 && t <= 1.0), "{out:?}");
        out
    }

    fn total(p: &mut Pacer, s: Sample, frames: usize) -> usize {
        (0..frames).map(|_| check(p, s).len()).sum()
    }

    #[test]
    fn slow_game_on_fast_display_is_not_a_hitch() {
        // 15 fps at 144 Hz: 67 ms is under the 200 ms floor, so it locks to two slots
        let mut p = Pacer::new(1.0 / 144.0, 2);
        assert!(total(&mut p, trusted(1.0 / 15.0), 60) > 60);
    }

    #[test]
    fn estimator_trust_rule() {
        let mut e = Estimator::default();
        assert_eq!(
            e.sample(10.0, 0.0),
            Sample {
                interval: 0.0,
                trusted: false
            }
        );
        let s = e.sample(10.0333, 0.0005);
        assert!(s.trusted && (s.interval - 0.0333).abs() < 1e-9);
        assert!(!e.sample(10.0666, 0.008).trusted);
        // below 1 ms is always trusted, even for a short interval
        assert!(e.sample(10.0686, 0.0009).trusted);
    }

    #[test]
    fn thirty_fps_on_sixty_hz_locks_to_two_slots() {
        let mut p = Pacer::new(REFRESH, 4);
        for _ in 0..60 {
            assert_eq!(check(&mut p, trusted(1.0 / 30.0)), [0.5, 1.0]);
        }
        assert_eq!(format!("slots={}", p.histogram()), "slots=2:60");
        assert_eq!(p.histogram(), "");
        assert_eq!(total(&mut p, trusted(1.0 / 30.0), 100), 200);
        assert_eq!(
            check(&mut Pacer::new(REFRESH, 4), trusted(1.0 / 20.0)),
            [1.0 / 3.0, 2.0 / 3.0, 1.0]
        );
    }

    #[test]
    fn fractional_ratios() {
        // 45 fps: ratio 1.333, pattern averages 4/3
        let mut p = Pacer::new(REFRESH, 4);
        for _ in 0..60 {
            check(&mut p, trusted(1.0 / 45.0));
        }
        assert_eq!(format!("slots={}", p.histogram()), "slots=1:40 2:20");
        assert_eq!(total(&mut p, trusted(1.0 / 45.0), 300), 400);
        // 1.2x: 118..122 slots per 100 frames
        let mut p = Pacer::new(REFRESH, 4);
        let n = total(&mut p, trusted(1.2 * REFRESH), 100);
        assert!((118..=122).contains(&n), "{n}");
        let first = check(&mut Pacer::new(REFRESH, 4), trusted(1.2 * REFRESH));
        assert!(
            first.len() == 1 && (first[0] - 1.0 / 1.2).abs() < 1e-9,
            "{first:?}"
        );
    }

    #[test]
    fn simple_fractions_end_on_the_original() {
        // frames of the 200 that end on their original, from a phase where none would
        let originals = |interval: f64, jitter: f64, phase: f64| {
            let mut p = Pacer::new(REFRESH, 4);
            p.estimate = interval;
            p.phase = phase;
            (0..200)
                .filter(|i| {
                    let d = if i % 2 == 0 { jitter } else { -jitter };
                    check(&mut p, trusted(interval + d)).last() == Some(&1.0)
                })
                .count()
        };
        // 40 fps on 60 hz shows every second original, 45 fps every third
        assert_eq!(originals(1.5 * REFRESH, 0.0, 0.75), 100);
        assert_eq!(originals(1.5 * REFRESH, 0.0002, 0.75), 100);
        assert!((66..=67).contains(&originals(4.0 / 3.0 * REFRESH, 0.0, 0.5)));
        // a ratio away from any simple fraction keeps its average
        let mut p = Pacer::new(REFRESH, 4);
        let n = total(&mut p, trusted(1.7 * REFRESH), 100);
        assert!((168..=172).contains(&n), "{n}");
    }

    #[test]
    fn cap_and_fast_sources() {
        let mut p = Pacer::new(REFRESH, 4);
        assert_eq!(
            check(&mut p, trusted(6.0 * REFRESH)),
            [0.25, 0.5, 0.75, 1.0]
        );
        assert_eq!(total(&mut p, trusted(6.0 * REFRESH), 100), 400);
        let mut p = Pacer::new(REFRESH, 4);
        assert_eq!(check(&mut p, trusted(0.5 * REFRESH)), [1.0]);
        assert_eq!(check(&mut p, trusted(1.0 * REFRESH)), [1.0]);
        assert_eq!(check(&mut p, trusted(1.04 * REFRESH)), [1.0]);
        // constructor defaults
        let mut p = Pacer::new(f64::NAN, 0);
        assert_eq!(p.refresh, REFRESH);
        assert_eq!(check(&mut p, trusted(1.0 / 30.0)), [1.0]);
    }

    #[test]
    fn untrusted_runs_and_probing() {
        // fresh pacer on untrusted 2x: cadence is adopted, then probing after 60 frames
        let mut p = Pacer::new(REFRESH, 4);
        for _ in 0..59 {
            assert_eq!(check(&mut p, untrusted(1.0 / 30.0)).len(), 2);
        }
        assert_eq!(check(&mut p, untrusted(1.0 / 30.0)), [1.0]);
        // a probe on a game slower than its one slot sees the idle display and returns to two
        let first: Vec<usize> = (0..10)
            .map(|_| check(&mut p, untrusted(1.0 / 30.0)).len())
            .collect();
        assert_eq!(first, [1, 1, 1, 1, 1, 1, 2, 2, 2, 2]);
        // 60 trusted, 59 untrusted keep 2, the 60th probes, a trusted sample restores 2
        let mut p = Pacer::new(REFRESH, 4);
        assert_eq!(total(&mut p, trusted(1.0 / 30.0), 60), 120);
        assert_eq!(total(&mut p, untrusted(1.0 / 30.0), 59), 118);
        assert_eq!(check(&mut p, untrusted(1.0 / 30.0)), [1.0]);
        assert_eq!(total(&mut p, trusted(1.0 / 30.0), 3), 3);
        assert_eq!(check(&mut p, trusted(1.0 / 30.0)), [0.5, 1.0]);
        // after another 60 untrusted, the probe's median replaces the estimate unsmoothed
        assert_eq!(total(&mut p, untrusted(1.0 / 30.0), 60), 118 + 1);
        assert_eq!(total(&mut p, trusted(1.2 / 60.0), 3), 3);
        assert_eq!(check(&mut p, trusted(1.2 / 60.0)).len(), 1);
        assert_eq!(p.interval(), 1.2 / 60.0);
        // a capped game catching up after held frames, then a hitch: measured in slay the spire
        total(&mut p, untrusted(1.2 / 60.0), 59);
        assert_eq!(check(&mut p, untrusted(1.2 / 60.0)), [1.0]);
        for ms in [10.3, 29.8, 32.2] {
            assert_eq!(check(&mut p, trusted(ms / 1000.0)), [1.0]);
        }
        assert_eq!(check(&mut p, trusted(0.169)).len(), 2);
        assert_eq!(p.interval(), 0.0322);
        // a slower untrusted cadence caps the estimate after 10 frames
        let mut p = Pacer::new(REFRESH, 4);
        assert_eq!(total(&mut p, trusted(1.0 / 20.0), 20), 60);
        for i in 0..20 {
            let n = check(&mut p, untrusted(1.0 / 30.0)).len();
            assert_eq!(n, if i < 9 { 3 } else { 2 }, "frame {i}");
        }
    }

    #[test]
    fn present_thread_blocked_for_the_whole_interval() {
        // an emulator presenting from its own thread waits in the hooks for nearly every frame
        // while its other threads render at 23 fps: measured in bloodborne on shadps4
        let mut p = Pacer::new(REFRESH, 3);
        total(&mut p, trusted(REFRESH), 10);
        total(&mut p, untrusted(0.043), 120);
        let n = total(&mut p, untrusted(0.043), 60);
        // the cap of three, less one short probe
        assert!(n >= 150, "{n}");
        assert_eq!(p.interval(), 0.043);
        // at 45 fps on 60 hz the probe's one slot never falls behind, so it must end on its own
        let mut p = Pacer::new(REFRESH, 4);
        total(&mut p, untrusted(1.0 / 45.0), 120);
        let n = total(&mut p, untrusted(1.0 / 45.0), 60);
        assert!(n >= 75, "{n}");
    }

    #[test]
    fn invalid_intervals() {
        let mut p = Pacer::new(REFRESH, 4);
        for bad in [f64::NAN, f64::INFINITY, -1.0, 0.0, 17.0 * REFRESH] {
            assert_eq!(check(&mut p, trusted(bad)), [1.0]);
            assert_eq!(p.interval(), 0.0);
        }
        assert_eq!(check(&mut p, untrusted(f64::NAN)), [1.0]);
        assert_eq!(p.histogram(), "1:6");
    }

    // game loop with three drawables, one refresh per slot
    fn closed_loop(work: f64, cap: Option<f64>, seed: f64) -> (f64, f64) {
        let mut p = Pacer::new(REFRESH, 4);
        p.slots(untrusted(seed));
        let mut est = Estimator::default();
        let (mut now, mut display_free, mut last_present) = (0.0, 0.0, f64::NEG_INFINITY);
        let mut inflight = std::collections::VecDeque::new();
        let (mut frames, mut slots, mut t0) = (0usize, 0usize, 0.0);
        for i in 0..900 {
            now += work;
            if let Some(c) = cap {
                now = f64::max(now, last_present + c);
            }
            let mut blocked = 0.0;
            if inflight.len() == 3 {
                let free: f64 = inflight.pop_front().unwrap();
                blocked = f64::max(0.0, free - now);
                now = f64::max(now, free);
            }
            last_present = now;
            let out = check(&mut p, est.sample(now, blocked));
            let shown = f64::max(now, display_free);
            display_free = shown + out.len() as f64 * REFRESH;
            inflight.push_back(display_free);
            if i == 300 {
                t0 = now;
            }
            if i > 300 {
                frames += 1;
                slots += out.len();
            }
        }
        (slots as f64 / frames as f64, frames as f64 / (now - t0))
    }

    #[test]
    fn closed_loop_converges() {
        for seed in [0.0333, 0.1] {
            let (k, fps) = closed_loop(1.0 / 45.0, None, seed);
            assert!(
                k > 1.25 && k < 1.45 && fps > 42.0,
                "45 fps: k={k} fps={fps}"
            );
            let (k, fps) = closed_loop(0.005, Some(1.0 / 30.0), seed);
            assert!(
                k > 1.95 && k <= 2.0 && fps > 29.0,
                "30 fps capped: k={k} fps={fps}"
            );
            let (k, fps) = closed_loop(0.005, None, seed);
            assert!(k == 1.0 && fps > 55.0, "display bound: k={k} fps={fps}");
            let (k, fps) = closed_loop(1.0 / 20.0, None, seed);
            assert!(k == 3.0 && fps > 19.0, "20 fps: k={k} fps={fps}");
        }
    }
}
