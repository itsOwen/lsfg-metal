use std::ptr::NonNull;
use std::sync::{Mutex, OnceLock};

use block2::RcBlock;
use objc2::runtime::ProtocolObject;
use objc2_metal::MTLDrawable;
use objc2_quartz_core::{CACurrentMediaTime, CAMetalDrawable};

use crate::log;

#[derive(Clone, Copy, Default)]
pub struct Frame {
    queued: f64,
    started: f64,
}

impl Frame {
    pub fn enqueue(&mut self) {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        if *ENABLED.get_or_init(|| std::env::var_os("LSFGM_LATENCY").is_some()) {
            self.queued = clock();
        }
    }

    pub fn start(&mut self) {
        if self.queued > 0.0 {
            self.started = clock();
        }
    }

    pub fn clock(&self) -> f64 {
        if self.queued > 0.0 {
            clock()
        } else {
            0.0
        }
    }
}

fn clock() -> f64 {
    CACurrentMediaTime()
}

#[derive(Clone, Copy)]
pub enum Kind {
    Generated,
    Original,
    Fallback,
}

#[derive(Default)]
pub struct Probe {
    batches: Mutex<[Batch; 3]>,
}

#[derive(Default)]
struct Batch {
    samples: Vec<[f64; 5]>,
    dropped: usize,
    count: usize,
}

impl Probe {
    pub fn attach(
        &'static self,
        drawable: &ProtocolObject<dyn CAMetalDrawable>,
        frame: Frame,
        acquire_ms: f64,
        kind: Kind,
    ) {
        if frame.queued == 0.0 {
            return;
        }
        let submitted = clock();
        let block = RcBlock::new(move |d: NonNull<ProtocolObject<dyn MTLDrawable>>| {
            let displayed = unsafe { d.as_ref() }.presentedTime();
            let batch = {
                let mut batches = self.batches.lock().unwrap();
                let batch = &mut batches[kind as usize];
                batch.count += 1;
                if displayed.is_finite() && displayed >= submitted {
                    batch.samples.push([
                        (displayed - frame.queued) * 1000.0,
                        (frame.started - frame.queued) * 1000.0,
                        (submitted - frame.started) * 1000.0,
                        acquire_ms,
                        (displayed - submitted) * 1000.0,
                    ]);
                } else {
                    batch.dropped += 1;
                }
                if batch.count < 120 {
                    return;
                }
                std::mem::take(batch)
            };
            let kind = ["generated", "original", "fallback"][kind as usize];
            let mut line = format!(
                "Metal latency kind={kind} samples={} dropped={}",
                batch.samples.len(),
                batch.dropped
            );
            for (i, name) in [
                "enqueue_display",
                "queue",
                "worker",
                "drawable",
                "submit_display",
            ]
            .iter()
            .enumerate()
            {
                let values: Vec<_> = batch.samples.iter().map(|s| s[i]).collect();
                if let Some((p50, p95)) = percentiles(values) {
                    line += &format!(" {name}_ms={p50:.3}/{p95:.3}");
                }
            }
            log::info(&line);
        });
        unsafe { drawable.addPresentedHandler(RcBlock::as_ptr(&block)) };
    }
}

fn percentiles(mut values: Vec<f64>) -> Option<(f64, f64)> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable_by(f64::total_cmp);
    let at = |percent: usize| values[(values.len() * percent).div_ceil(100) - 1];
    Some((at(50), at(95)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn nearest_rank_percentiles() {
        assert_eq!(super::percentiles(vec![]), None);
        assert_eq!(super::percentiles(vec![7.0]), Some((7.0, 7.0)));
        assert_eq!(
            super::percentiles((1..=100).rev().map(f64::from).collect()),
            Some((50.0, 95.0))
        );
    }
}
