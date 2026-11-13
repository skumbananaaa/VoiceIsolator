use crate::RATE;
use serde::{Deserialize, Serialize};
use std::{
    cell::UnsafeCell,
    f64::consts::PI,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering::*},
    },
};

pub const LIMIT: usize = RATE * 80 / 1000;
const LEFT: usize = 23;
const RIGHT: usize = 24;
const PHASES: usize = 128;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Correction {
    #[default]
    Linear,
    BandLimited,
}

struct Ring {
    buffer: Box<[UnsafeCell<f32>]>,
    read: AtomicUsize,
    write: AtomicUsize,
}

// Only Producer writes, and only Consumer reads. The two release/acquire
// cursors grant access to disjoint slots and publish completed operations.
unsafe impl Sync for Ring {}

pub struct Producer(Arc<Ring>);
pub struct Consumer(Arc<Ring>);

pub fn ring() -> (Producer, Consumer) {
    let ring = Arc::new(Ring {
        buffer: (0..LIMIT.next_power_of_two())
            .map(|_| UnsafeCell::new(0.0))
            .collect(),
        read: AtomicUsize::new(0),
        write: AtomicUsize::new(0),
    });
    (Producer(ring.clone()), Consumer(ring))
}

impl Producer {
    fn available(&self, write: usize) -> usize {
        LIMIT - write.wrapping_sub(self.0.read.load(Acquire))
    }

    pub fn push(&mut self, samples: Option<&[f32]>, count: usize) -> bool {
        let write = self.0.write.load(Relaxed);
        if count > self.available(write) {
            return false;
        }
        self.publish(write, samples, count);
        true
    }

    pub fn push_some(&mut self, samples: &[f32]) -> usize {
        let write = self.0.write.load(Relaxed);
        let count = samples.len().min(self.available(write));
        if count > 0 {
            self.publish(write, Some(samples), count);
        }
        count
    }

    fn publish(&mut self, write: usize, samples: Option<&[f32]>, count: usize) {
        let ring = &self.0;
        let samples = samples.map(|x| &x[..count]);
        for i in 0..count {
            let value = samples.map_or(0.0, |s| s[i]);
            let value = if value.is_finite() {
                value.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            unsafe {
                *ring.buffer[write.wrapping_add(i) & (ring.buffer.len() - 1)].get() = value;
            }
        }
        ring.write.store(write.wrapping_add(count), Release);
    }
}

struct Read<'a> {
    ring: &'a Ring,
    start: usize,
    len: usize,
}

impl Read<'_> {
    fn get(&self, index: usize) -> f32 {
        debug_assert!(index < self.len);
        unsafe {
            *self.ring.buffer[self.start.wrapping_add(index) & (self.ring.buffer.len() - 1)].get()
        }
    }
}

impl Consumer {
    pub fn pending(&self) -> usize {
        self.read().len
    }

    fn read(&self) -> Read<'_> {
        let start = self.0.read.load(Relaxed);
        let len = self.0.write.load(Acquire).wrapping_sub(start);
        Read {
            ring: &self.0,
            start,
            len,
        }
    }

    fn consume(&mut self, count: usize) {
        let start = self.0.read.load(Relaxed);
        self.0.read.store(start.wrapping_add(count), Release);
    }

    pub fn pop(&mut self, output: &mut [f32]) -> bool {
        let read = self.read();
        if read.len < output.len() {
            return false;
        }
        for (i, sample) in output.iter_mut().enumerate() {
            *sample = read.get(i);
        }
        self.consume(output.len());
        true
    }
}

type Kernels = [[f32; LEFT + RIGHT + 1]; PHASES + 1];

fn kernels() -> &'static Kernels {
    static BANK: OnceLock<Kernels> = OnceLock::new();
    BANK.get_or_init(|| {
        std::array::from_fn(|phase| {
            let fraction = phase as f64 / PHASES as f64;
            let mut weights = std::array::from_fn(|tap| {
                let x = tap as f64 - LEFT as f64 - fraction;
                if x.abs() >= RIGHT as f64 {
                    return 0.0;
                }
                let angle = PI * x / RIGHT as f64;
                let window = 0.42 + 0.5 * angle.cos() + 0.08 * (2.0 * angle).cos();
                let phase = PI * 0.94 * x;
                (0.94
                    * window
                    * if phase == 0.0 {
                        1.0
                    } else {
                        phase.sin() / phase
                    }) as f32
            });
            let sum: f32 = weights.iter().sum();
            for weight in &mut weights {
                *weight /= sum;
            }
            weights
        })
    })
}

pub struct Playback {
    prefill: usize,
    render_rate: usize,
    correction: Correction,
    kernels: Option<&'static Kernels>,
    position: f64,
    filtered: f64,
    primed: bool,
}

pub struct Fill {
    pub pending: usize,
    pub produced: usize,
    pub underrun: bool,
    pub primed: bool,
}

impl Playback {
    pub fn new(correction: Correction) -> Self {
        Self {
            prefill: 0,
            render_rate: RATE,
            correction,
            kernels: None,
            position: 0.0,
            filtered: 0.0,
            primed: false,
        }
    }

    fn history(&self) -> usize {
        if self.kernels.is_some() { LEFT } else { 0 }
    }

    fn lookahead(&self) -> usize {
        if self.kernels.is_some() { RIGHT } else { 1 }
    }

    pub fn configure(
        &mut self,
        batch: usize,
        capture_period: usize,
        render_period: usize,
        capacity: usize,
        render_rate: usize,
    ) -> Option<usize> {
        self.render_rate = render_rate;
        // Format conversion must suppress images above the model's bandwidth.
        // Use the same interpolator for conversion and clock correction.
        self.kernels =
            (render_rate != RATE || self.correction == Correction::BandLimited).then(kernels);
        let padding = capacity.min(2 * render_period);
        let margin = if batch < RATE * 32 / 1000 {
            RATE * 4 / 1000
        } else {
            0
        };
        self.prefill = (34 * RATE / 1000)
            .max(batch + capture_period + margin)
            .max(self.prime_frames(padding));
        (padding > 0 && self.prefill <= LIMIT).then_some(padding)
    }

    fn prime_frames(&self, frames: usize) -> usize {
        // Include the maximum drift correction and the interpolation lookahead.
        (frames * RATE * 1001).div_ceil(self.render_rate * 1000) + self.lookahead()
    }

    fn interpolate(&self, samples: &Read<'_>, i: usize, fraction: f64) -> f32 {
        if let Some(bank) = self.kernels {
            let kernel = &bank[(fraction * PHASES as f64).round() as usize];
            kernel
                .iter()
                .enumerate()
                .map(|(tap, weight)| samples.get((i + tap).saturating_sub(LEFT)) * weight)
                .sum::<f32>()
                .clamp(-1.0, 1.0)
        } else {
            let a = samples.get(i);
            a + (samples.get(i + 1) - a) * fraction as f32
        }
    }

    pub fn fill(&mut self, queue: &mut Consumer, output: &mut [f32]) -> Fill {
        let samples = queue.read();
        let pending = samples.len.saturating_sub(self.position as usize);
        if !self.primed {
            if pending < self.prefill.max(self.prime_frames(output.len())) {
                return Fill {
                    pending,
                    produced: 0,
                    underrun: false,
                    primed: false,
                };
            }
            self.primed = true;
            self.filtered = pending as f64;
        }
        let weight = if self.render_rate == RATE {
            0.02
        } else {
            (2.0 * output.len() as f64 / self.render_rate as f64).min(1.0)
        };
        self.filtered += weight * (pending as f64 - self.filtered);
        let drift = ((self.filtered - self.prefill as f64) * 0.000004).clamp(-0.001, 0.001);
        let rate = (1.0 + drift) * RATE as f64 / self.render_rate as f64;
        let mut position = self.position;
        let mut produced = 0;
        for sample in output.iter_mut() {
            let i = position as usize;
            if i + self.lookahead() >= samples.len {
                break;
            }
            *sample = self.interpolate(&samples, i, position - i as f64);
            produced += 1;
            position += rate;
        }
        let used = samples.len.min(position as usize);
        let underrun = produced != output.len();
        if underrun {
            output[produced..].fill(0.0);
            self.primed = false;
            self.position = 0.0;
            queue.consume(used);
        } else {
            let release = used.saturating_sub(self.history());
            queue.consume(release);
            self.position = position - release as f64;
        }
        Fill {
            pending,
            produced,
            underrun,
            primed: self.primed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_ring_wraps_without_overwriting() {
        let (mut tx, mut rx) = ring();
        let samples: Vec<_> = (0..LIMIT).map(|i| i as f32 / LIMIT as f32).collect();
        assert!(tx.push(Some(&samples), LIMIT));
        assert!(!tx.push(None, 1));
        let mut first = [0.0; 31];
        assert!(rx.pop(&mut first));
        assert_eq!(&first, &samples[..31]);
        assert!(tx.push(Some(&first), 31));
        let mut output = vec![0.0; LIMIT];
        assert!(rx.pop(&mut output));
        assert_eq!(&output[..LIMIT - 31], &samples[31..]);
        assert_eq!(&output[LIMIT - 31..], &first);
    }

    #[test]
    fn playback_primes_and_preserves_constant_level() {
        for correction in [Correction::Linear, Correction::BandLimited] {
            let mut player = Playback::new(correction);
            let (mut tx, mut rx) = ring();
            assert_eq!(player.configure(256, 160, 160, 640, RATE), Some(320));
            let mut output = [0.0; 160];
            assert!(!player.fill(&mut rx, &mut output).primed);
            assert!(tx.push(Some(&[0.25; 600]), 600));
            let filled = player.fill(&mut rx, &mut output);
            assert!(filled.primed && !filled.underrun);
            assert!(output.iter().all(|s| (s - 0.25).abs() < 1e-6));
        }
    }

    #[test]
    fn playback_stays_bounded_with_fast_and_slow_capture_clocks() {
        for correction in [Correction::Linear, Correction::BandLimited] {
            for ppm in [-500.0, 500.0] {
                let mut player = Playback::new(correction);
                let (mut tx, mut rx) = ring();
                assert!(player.configure(256, 160, 160, 640, RATE).is_some());
                let mut output = [0.0; 160];
                let mut generated = 0;
                let mut primed = false;
                for render in 1..=6_000 {
                    let due = (render as f64 * 160.0 * (1.0 + ppm / 1_000_000.0)) as usize;
                    while generated + 256 <= due {
                        assert!(tx.push(Some(&[0.25; 256]), 256));
                        generated += 256;
                    }
                    let fill = player.fill(&mut rx, &mut output);
                    assert!(
                        !fill.underrun && (!primed || fill.primed),
                        "{correction:?} at {ppm} ppm"
                    );
                    assert!(fill.pending < LIMIT);
                    primed |= fill.primed;
                }
                assert!(primed);
            }
        }
    }

    #[test]
    fn short_period_playback_converts_to_the_cable_rate_without_draining_early() {
        for rate in [44_100, 48_000] {
            let mut player = Playback::new(Correction::Linear);
            let (mut tx, mut rx) = ring();
            let period = rate / 200;
            let mut output = vec![0.0; period];
            assert_eq!(
                player.configure(256, 160, period, 2 * period, rate),
                Some(2 * period)
            );
            assert_eq!(player.prefill, 544);
            let mut generated = 0;
            let mut played = 0;
            for tick in 1..=400 {
                let due = tick * period * RATE / rate;
                while generated + 256 <= due {
                    assert!(tx.push(Some(&[0.25; 256]), 256));
                    generated += 256;
                }
                let fill = player.fill(&mut rx, &mut output);
                assert!(!fill.underrun);
                if fill.primed {
                    assert_eq!(fill.produced, period);
                    assert!(output.iter().all(|s| (s - 0.25).abs() < 1e-6));
                }
                played += fill.produced;
            }
            assert!(played > rate);
            assert!(generated.abs_diff(played * RATE / rate + rx.pending()) < 64);
        }
    }

    #[test]
    fn partial_burst_lets_playback_reach_prefill_without_growing_ring() {
        for correction in [Correction::Linear, Correction::BandLimited] {
            let mut player = Playback::new(correction);
            let (mut tx, mut rx) = ring();
            assert_eq!(player.configure(640, 320, 160, 640, RATE), Some(320));
            assert_eq!(player.prefill, 960);
            let mut output = [0.0; 160];
            assert!(tx.push(Some(&[0.25; 256]), 256));
            assert!(tx.push(Some(&[0.25; 640]), 640));
            assert!(!player.fill(&mut rx, &mut output).primed);
            let next = [0.25; 640];
            assert!(!tx.push(Some(&next), next.len()));
            let mut written = tx.push_some(&next);
            assert_eq!(written, 384);
            assert_eq!(rx.pending(), LIMIT);
            while written < next.len() {
                let fill = player.fill(&mut rx, &mut output);
                assert!(fill.primed && !fill.underrun);
                assert!(output.iter().all(|sample| (sample - 0.25).abs() < 1e-6));
                let count = tx.push_some(&next[written..]);
                assert!(count > 0);
                written += count;
                assert!(rx.pending() <= LIMIT);
            }
        }
    }
}
