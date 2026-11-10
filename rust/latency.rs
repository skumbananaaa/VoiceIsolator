use crate::{
    RATE,
    wasapi::{self, Com, Event, Stream},
};
use anyhow::{Context, Result, ensure};
use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};
use std::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    time::{Duration, Instant},
};
use windows::Win32::{
    Media::Audio::eCapture,
    System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
};

const SECONDS: usize = 6;
const ANALYSIS_RATE: usize = 2000;
const BIN_TICKS: u64 = 10_000_000 / ANALYSIS_RATE as u64;
const FRAME_TICKS: u64 = 10_000_000 / RATE as u64;
const WINDOW: usize = 480;
const MAX_LAG: usize = ANALYSIS_RATE / 2;
const ENVELOPE_STRIDE: usize = 10;
const ABSENT: f32 = f32::NAN;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Latency {
    pub median_ms: f64,
    pub p95_ms: f64,
    pub windows: usize,
    pub eligible_windows: usize,
    pub approximate: bool,
    pub gaps_skipped: bool,
}

const PACKET_SIZE: usize = 256;

struct TapPacket {
    audio: [f32; PACKET_SIZE],
    frames: usize,
    tick: u64,
}

pub(crate) struct Tap {
    sender: Sender<TapPacket>,
    receiver: Receiver<TapPacket>,
    enabled: AtomicBool,
    dropped: AtomicUsize,
}

impl Tap {
    pub fn new() -> Self {
        let (sender, receiver) = crossbeam_channel::bounded(64);
        Self {
            sender,
            receiver,
            enabled: AtomicBool::new(false),
            dropped: AtomicUsize::new(0),
        }
    }

    pub fn push(&self, packet: &wasapi::Packet<'_>) {
        if !self.enabled.load(Relaxed) {
            return;
        }
        let Some(tick) = packet.tick else { return };
        for offset in (0..packet.frames).step_by(PACKET_SIZE) {
            let frames = (packet.frames - offset).min(PACKET_SIZE);
            let mut audio = [0.0; PACKET_SIZE];
            if let Some(data) = packet.samples {
                audio[..frames].copy_from_slice(&data[offset..offset + frames]);
            }
            if self
                .sender
                .try_send(TapPacket {
                    audio,
                    frames,
                    tick: tick + offset as u64 * FRAME_TICKS,
                })
                .is_err()
            {
                self.dropped.fetch_add(1, Relaxed);
                break;
            }
        }
    }
}

#[derive(Clone, Default)]
struct Bin {
    sum: f32,
    power: f32,
    count: u32,
    excluded: bool,
}

struct Signal {
    origin: u64,
    bins: Vec<Bin>,
}

impl Signal {
    fn new(origin: u64) -> Self {
        Self {
            origin,
            bins: vec![Bin::default(); ANALYSIS_RATE * SECONDS],
        }
    }

    fn add(&mut self, samples: Option<&[f32]>, frames: usize, tick: u64) {
        for i in 0..frames {
            let Some(relative) = (tick + i as u64 * FRAME_TICKS).checked_sub(self.origin) else {
                continue;
            };
            let Some(bin) = self.bins.get_mut((relative / BIN_TICKS) as usize) else {
                break;
            };
            let value = samples.map_or(0.0, |data| data[i]);
            if value.is_finite() && value.abs() <= 16.0 {
                bin.sum += value;
                bin.power += value * value;
                bin.count += 1;
            }
        }
    }

    fn exclude(&mut self, start: u64, end: u64) {
        let first = ((start.saturating_sub(self.origin) / BIN_TICKS) as usize).min(self.bins.len());
        let end =
            ((end.saturating_sub(self.origin).div_ceil(BIN_TICKS)) as usize).min(self.bins.len());
        for bin in &mut self.bins[first..end.max(first)] {
            bin.excluded = true;
        }
    }

    fn samples(&self) -> Vec<f32> {
        self.bins
            .iter()
            .map(|bin| {
                if bin.count >= 4 && !bin.excluded {
                    bin.sum / bin.count as f32
                } else {
                    ABSENT
                }
            })
            .collect()
    }

    fn envelope(&self) -> Vec<f32> {
        let mut envelope = vec![ABSENT; self.bins.len() / ENVELOPE_STRIDE];
        let count = envelope.len();
        for (i, value) in envelope.iter_mut().enumerate().take(count - 1).skip(1) {
            let start = (i - 1) * ENVELOPE_STRIDE;
            let bins = self.bins[start..start + 3 * ENVELOPE_STRIDE]
                .iter()
                .filter(|b| b.count >= 4 && !b.excluded);
            let (covered, count, power) = bins.fold((0, 0, 0.0), |(n, c, p), b| {
                (n + 1, c + b.count, p + b.power)
            });
            if covered >= 24 {
                *value = (1000.0 * (power / count as f32).sqrt()).ln_1p();
            }
        }
        envelope
    }
}

fn summarize(mut delays: Vec<f64>, eligible: usize, approximate: bool) -> Option<Latency> {
    if delays.len() < 4 {
        return None;
    }
    delays.sort_by(f64::total_cmp);
    let percentile = |p: f64| delays[(p * delays.len() as f64).ceil() as usize - 1];
    let p95 = percentile(0.95);
    (p95 - percentile(0.05) <= 60.0).then(|| Latency {
        median_ms: percentile(0.50),
        p95_ms: p95,
        windows: delays.len(),
        eligible_windows: eligible,
        approximate,
        gaps_skipped: false,
    })
}

fn best(scores: &[f64]) -> usize {
    scores
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map_or(0, |(i, _)| i)
}

fn distinct(scores: &[f64], best: usize, radius: usize) -> f64 {
    let alternate = scores
        .iter()
        .enumerate()
        .filter(|(i, _)| i.abs_diff(best) > radius)
        .map(|(_, &x)| x)
        .fold(0.0_f64, f64::max);
    scores[best] - alternate
}

fn waveform(mic: &[f32], cable: &[f32], canceled: &impl Fn() -> bool) -> Option<Latency> {
    let diff = |x: &[f32]| x.windows(2).map(|p| p[1] - p[0]).collect::<Vec<_>>();
    let mic = diff(mic);
    let cable = diff(cable);
    let (mut energy, mut missing) = (vec![0.0; cable.len() + 1], vec![0; cable.len() + 1]);
    for (i, &x) in cable.iter().enumerate() {
        energy[i + 1] = energy[i] + if x.is_finite() { (x * x) as f64 } else { 0.0 };
        missing[i + 1] = missing[i] + usize::from(!x.is_finite());
    }
    let end = mic.len().min(cable.len().saturating_sub(MAX_LAG));
    let mut eligible = 0;
    let mut delays = Vec::new();
    for start in (0..=end.saturating_sub(WINDOW))
        .step_by(400)
        .filter(|_| end >= WINDOW)
    {
        if canceled() {
            return None;
        }
        let source = &mic[start..start + WINDOW];
        let source_energy = source.iter().map(|x| (x * x) as f64).sum::<f64>();
        if !source_energy.is_finite() || source_energy < WINDOW as f64 * 1e-6 {
            continue;
        }
        eligible += 1;
        let mut scores = [0.0; MAX_LAG + 1];
        for (lag, score) in scores.iter_mut().enumerate() {
            let begin = start + lag;
            let end = begin + WINDOW;
            let target_energy = energy[end] - energy[begin];
            if missing[end] != missing[begin] || target_energy < WINDOW as f64 * 2.5e-7 {
                continue;
            }
            let dot = source
                .iter()
                .zip(&cable[begin..end])
                .map(|(x, y)| (x * y) as f64)
                .sum::<f64>();
            *score = dot.abs() / (source_energy * target_energy).sqrt();
        }
        let lag = best(&scores);
        if scores[lag] >= 0.48 && distinct(&scores, lag, 24) >= 0.06 {
            delays.push(lag as f64 * 0.5);
        }
    }
    summarize(delays, eligible, false)
}

fn envelope_correlation(a: &[f32], b: &[f32]) -> f64 {
    let (mut x, mut y, mut xx, mut yy, mut xy, mut count) = (0.0, 0.0, 0.0, 0.0, 0.0, 0);
    for (&u, &v) in a.iter().zip(b) {
        if !u.is_finite() || !v.is_finite() {
            continue;
        }
        let (u, v) = (u as f64, v as f64);
        x += u;
        y += v;
        xx += u * u;
        yy += v * v;
        xy += u * v;
        count += 1;
    }
    if count == 0 || count * 5 < a.len() * 4 {
        return 0.0;
    }
    let n = count as f64;
    let (xx, yy) = (xx - x * x / n, yy - y * y / n);
    if xx.min(yy) < 0.04 * n {
        return 0.0;
    }
    (xy - x * y / n) / (xx * yy).sqrt()
}

fn envelope(mic: &[f32], cable: &[f32], canceled: &impl Fn() -> bool) -> Option<Latency> {
    const LAGS: usize = MAX_LAG / ENVELOPE_STRIDE + 1;
    let end = mic.len().min(cable.len().checked_sub(LAGS - 1)?);
    if end < 120 || canceled() {
        return None;
    }
    let score = |start: usize, count: usize| -> [f64; LAGS] {
        std::array::from_fn(|lag| {
            envelope_correlation(
                &mic[start..start + count],
                &cable[start + lag..start + lag + count],
            )
        })
    };
    let whole = score(0, end);
    let anchor = best(&whole);
    if whole[anchor] < 0.6 || distinct(&whole, anchor, 8) < 0.10 {
        return None;
    }
    let mut eligible = 0;
    let mut delays = Vec::new();
    for start in (0..=end - 120).step_by(80) {
        if canceled() {
            return None;
        }
        let segment = &mic[start..start + 120];
        if envelope_correlation(segment, segment) < 0.99 {
            continue;
        }
        eligible += 1;
        let scores = score(start, 120);
        let lag = best(&scores);
        if lag.abs_diff(anchor) <= 5 && scores[lag] >= 0.6 {
            delays.push(lag as f64 * 5.0);
        }
    }
    if delays.len() * 2 < eligible {
        return None;
    }
    summarize(delays, eligible, true)
}

fn ticks() -> Result<u64> {
    let (mut qpc, mut frequency) = (0, 0);
    unsafe {
        QueryPerformanceCounter(&mut qpc)?;
        QueryPerformanceFrequency(&mut frequency)?;
    }
    Ok((qpc as u128 * 10_000_000 / frequency as u128) as u64)
}

pub(crate) fn measure(
    cable: &str,
    stop: &Event,
    tap: &Tap,
    canceled: impl Fn() -> bool,
    playback: impl Fn() -> (bool, usize, usize),
) -> Result<Latency> {
    let _com = Com::init()?;
    let devices = wasapi::enumerator()?;
    let endpoint = wasapi::cable_recording_id(&devices, cable)?;
    let mut stream = Stream::open(&devices, &endpoint, eCapture, true)?;
    while tap.receiver.try_recv().is_ok() {}
    let origin = ticks()?;
    let mut mic = Signal::new(origin);
    let mut output = Signal::new(origin);
    let _priority = wasapi::AudioPriority::register();
    let start = Instant::now();
    let (_, mut underruns, mut recoveries) = playback();
    let initial_drops = tap.dropped.load(Relaxed);
    tap.enabled.store(true, Relaxed);
    struct Disable<'a>(&'a AtomicBool);
    impl Drop for Disable<'_> {
        fn drop(&mut self) {
            self.0.store(false, Relaxed);
        }
    }
    let enable_guard = Disable(&tap.enabled);
    stream.start()?;
    let mut previous_end = origin;
    let mut last_observed = origin;
    while start.elapsed() < Duration::from_secs(SECONDS as u64) {
        ensure!(!canceled(), "Latency measurement canceled.");
        ensure!(
            wasapi::wait([stop, &stream.event], 100)? != Some(0),
            "Latency measurement canceled."
        );
        while let Ok(packet) = tap.receiver.try_recv() {
            mic.add(
                Some(&packet.audio[..packet.frames]),
                packet.frames,
                packet.tick,
            );
        }
        stream.capture(|packet| {
            if let Some(tick) = packet.tick {
                let end = tick + packet.frames as u64 * FRAME_TICKS;
                if packet.gap {
                    output.exclude(previous_end.min(tick), end);
                } else {
                    output.add(packet.samples, packet.frames, tick);
                }
                previous_end = end;
            }
            Ok(())
        })?;
        let (live, next_underruns, next_recoveries) = playback();
        let now = ticks()?;
        if !live || next_underruns != underruns || next_recoveries != recoveries {
            output.exclude(last_observed, now + MAX_LAG as u64 * BIN_TICKS);
        }
        (last_observed, underruns, recoveries) = (now, next_underruns, next_recoveries);
    }
    drop(stream);
    drop(enable_guard);
    drop(_priority);
    ensure!(
        tap.dropped.load(Relaxed) == initial_drops,
        "The latency sampler fell behind. Try again."
    );
    let skipped = output.bins.iter().any(|b| b.excluded);
    let mut result = if let Some(result) = waveform(&mic.samples(), &output.samples(), &canceled) {
        result
    } else {
        ensure!(!canceled(), "Latency measurement canceled.");
        let (mic, output) = (mic.envelope(), output.envelope());
        let has_timing = |x: &[f32]| x.iter().filter(|v| v.is_finite()).count() >= x.len() * 4 / 5;
        ensure!(
            has_timing(&mic),
            "Missing mic timestamps. Restart isolation and retry."
        );
        ensure!(
            has_timing(&output),
            "Too little clean cable audio. Check CABLE Output and retry."
        );
        ensure!(
            mic.iter().filter(|&&x| x > 3.0_f32.ln()).count() >= 40,
            "Mic too quiet. Speak at your normal distance."
        );
        ensure!(
            output.iter().filter(|&&x| x > 2.0_f32.ln()).count() >= 40,
            "Cable too quiet. Check the isolated level meter."
        );
        envelope(&mic, &output, &canceled)
            .context("No reliable match. Speak alone in short phrases.")?
    };
    result.gaps_skipped = skipped;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_known_lag_with_gap_and_gain() {
        let mut state = 1_u32;
        let mic: Vec<_> = (0..12000)
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                state as f32 / u32::MAX as f32 * 0.1 - 0.05
            })
            .collect();
        let mut cable = vec![0.0; mic.len()];
        let offset = 137;
        for i in offset..cable.len() {
            cable[i] = mic[i - offset] * 0.3;
        }
        cable[1000..1900].fill(ABSENT);
        let result = waveform(&mic, &cable, &|| false).unwrap();
        assert_eq!(result.median_ms, 68.5);
        assert_eq!(result.p95_ms, 68.5);
        assert!(result.windows < result.eligible_windows);
    }
}
