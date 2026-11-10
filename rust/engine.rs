use std::{
    io::{self, BufRead, BufReader, BufWriter, Read, Write},
    os::windows::{io::AsRawHandle, process::CommandExt},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{
            AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize,
            Ordering::{Acquire, Relaxed, Release},
        },
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    RATE,
    isolation::{Backend, VoxLock, default_models},
    latency::{self, Latency, Tap},
    playback::{self, Correction, Playback},
    preferences,
    wasapi::{self, AudioPriority, Com, Event, Stream},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        Media::Audio::{eCapture, eRender},
        System::JobObjects::*,
    },
    core::PCWSTR,
};

pub const BATCHES: [usize; 4] = [16, 24, 32, 40];
const CONTROL_LIMIT: u64 = 16_384;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Processing {
    pub batch_ms: usize,
    pub threads: i32,
    pub correction: Correction,
    #[serde(default)]
    pub backend: Backend,
}

impl Default for Processing {
    fn default() -> Self {
        Self {
            batch_ms: 16,
            threads: 1,
            correction: Correction::Linear,
            backend: Backend::Cpu,
        }
    }
}

impl Processing {
    pub fn valid(&self) -> bool {
        BATCHES.contains(&self.batch_ms)
            && (1..=4).contains(&self.threads)
            && (!cfg!(feature = "cpu-only") || self.backend == Backend::Cpu)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gains {
    pub mic_db: i32,
    pub output_db: i32,
}

impl Gains {
    pub fn valid(self) -> bool {
        (-12..=12).contains(&self.mic_db) && (-18..=12).contains(&self.output_db)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub microphone: String,
    pub cable: String,
    pub processing: Processing,
    pub gains: Gains,
    pub models: PathBuf,
}

#[derive(Serialize, Deserialize)]
enum Control {
    Start { protocol: u32, config: Config },
    Stop,
    Gains(Gains),
    Measure,
    CancelMeasurement,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Status {
    pub model_ready: bool,
    pub live: bool,
    pub input: f32,
    pub output: f32,
    pub underruns: usize,
    pub recoveries: usize,
    pub peak_frames: usize,
    pub input_pending_frames: usize,
    pub output_pending_frames: usize,
    pub inference_p99_ms: Option<usize>,
    pub inference_peak_us: u64,
    pub queue_wait_p99_ms: Option<usize>,
    pub batch_p99_ms: Option<usize>,
    pub batch_deadline_misses: usize,
    pub last_reset_us: u64,
    pub heartbeat: u64,
}

#[derive(Serialize, Deserialize)]
enum Update {
    Status(Status),
    Measured(Result<Latency, String>),
    Fault(String),
}

fn send(writer: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let mut message = serde_json::to_vec(value)?;
    ensure!(
        message.len() < CONTROL_LIMIT as usize,
        "Engine message is too large"
    );
    message.push(b'\n');
    writer.write_all(&message)?;
    writer.flush()?;
    Ok(())
}

fn receive<T: DeserializeOwned>(
    reader: &mut impl BufRead,
    buffer: &mut Vec<u8>,
) -> Result<Option<T>> {
    buffer.clear();
    if reader.take(CONTROL_LIMIT).read_until(b'\n', buffer)? == 0 {
        return Ok(None);
    }
    ensure!(buffer.last() == Some(&b'\n'), "Incomplete engine message");
    Ok(Some(serde_json::from_slice(buffer)?))
}

#[derive(Default)]
struct Inbox {
    status: Status,
    measured: Option<Result<Latency, String>>,
    fault: Option<String>,
}

struct Job(HANDLE);

impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

impl Job {
    fn new() -> Result<Self> {
        let job = Self(unsafe { CreateJobObjectW(None, PCWSTR::null())? });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of_val(&limits) as u32,
            )?;
        }
        Ok(job)
    }
}

pub struct Session {
    child: Child,
    _job: Job,
    commands: ChildStdin,
    inbox: Arc<Mutex<Inbox>>,
    last_heartbeat: u64,
    last_progress: Instant,
    stopping: Option<Instant>,
    pub config: Config,
}

impl Session {
    pub fn launch(config: Config) -> Result<Self> {
        let job = Job::new()?;
        let desktop = std::env::current_exe()?;
        let worker = desktop.with_file_name("voiceisolator_engine.exe");
        let mut command = Command::new(&worker);
        if !worker.is_file() && !desktop.with_file_name("installed-release").is_file() {
            // Cargo can build just the desktop target during development.
            command = Command::new(desktop);
            command.arg("--engine");
        }
        let mut child = command
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("Could not launch the VoiceIsolator audio engine")?;
        if let Err(error) =
            unsafe { AssignProcessToJobObject(job.0, HANDLE(child.as_raw_handle())) }
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("Protect the audio engine with a Windows job");
        }
        let commands = child
            .stdin
            .take()
            .context("Engine control pipe is missing")?;
        let updates = child
            .stdout
            .take()
            .context("Engine status pipe is missing")?;
        let inbox = Arc::new(Mutex::new(Inbox::default()));
        let mailbox = inbox.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(updates);
            let mut buffer = Vec::new();
            loop {
                let message = receive::<Update>(&mut reader, &mut buffer);
                let mut inbox = mailbox.lock().unwrap();
                match message {
                    Ok(Some(Update::Status(status))) => inbox.status = status,
                    Ok(Some(Update::Measured(result))) => inbox.measured = Some(result),
                    Ok(Some(Update::Fault(error))) => inbox.fault = Some(error),
                    Ok(None) => break,
                    Err(error) => {
                        inbox.fault = Some(format!("Audio engine status failed: {error:#}"));
                        break;
                    }
                }
            }
        });
        let mut session = Self {
            child,
            _job: job,
            commands,
            inbox,
            last_heartbeat: 0,
            last_progress: Instant::now(),
            stopping: None,
            config,
        };
        send(
            &mut session.commands,
            &Control::Start {
                protocol: 1,
                config: session.config.clone(),
            },
        )?;
        Ok(session)
    }

    pub fn status(&self) -> Status {
        self.inbox.lock().unwrap().status.clone()
    }

    pub fn take_measurement(&self) -> Option<Result<Latency, String>> {
        self.inbox.lock().unwrap().measured.take()
    }

    pub fn gains(&mut self, gains: Gains) -> Result<()> {
        send(&mut self.commands, &Control::Gains(gains))?;
        self.config.gains = gains;
        Ok(())
    }

    pub fn measure(&mut self) -> Result<()> {
        send(&mut self.commands, &Control::Measure)
    }

    pub fn cancel_measurement(&mut self) -> Result<()> {
        send(&mut self.commands, &Control::CancelMeasurement)
    }

    pub fn stop(&mut self) {
        if self.stopping.is_none() {
            self.stopping = Some(Instant::now());
            let _ = send(&mut self.commands, &Control::Stop);
        }
    }

    pub fn stopping(&self) -> bool {
        self.stopping.is_some()
    }

    // Returns true after the child exits. A watchdog error drops the session
    // and closes the protected route before the UI can start another one.
    pub fn poll(&mut self) -> Result<bool> {
        let exited = self.child.try_wait()?;
        if let Some(since) = self.stopping {
            if exited.is_some() {
                return Ok(true);
            }
            if since.elapsed() >= Duration::from_secs(1) {
                self.child.kill()?;
                self.child.wait()?;
                return Ok(true);
            }
            return Ok(false);
        }
        let inbox = self.inbox.lock().unwrap();
        if let Some(error) = &inbox.fault {
            bail!("{error}");
        }
        if let Some(code) = exited {
            bail!("The audio engine exited ({code}).");
        }
        if inbox.status.heartbeat != self.last_heartbeat {
            self.last_progress = Instant::now();
            self.last_heartbeat = inbox.status.heartbeat;
        }
        let timeout = if inbox.status.model_ready {
            2500
        } else if self.config.processing.backend == Backend::Cuda {
            60000
        } else {
            20000
        };
        ensure!(
            self.last_progress.elapsed() < Duration::from_millis(timeout),
            "The audio engine stopped responding."
        );
        Ok(false)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[derive(Default)]
struct Meters {
    model_ready: AtomicBool,
    live: AtomicBool,
    input: AtomicU32,
    output: AtomicU32,
    underruns: AtomicUsize,
    recoveries: AtomicUsize,
    peak_frames: AtomicUsize,
    input_pending_frames: AtomicUsize,
    output_pending_frames: AtomicUsize,
    timing: BatchMetrics,
    last_reset_us: AtomicU64,
    heartbeat: AtomicU64,
}

#[derive(Default)]
struct BatchMetrics {
    inference: Timing,
    queue_wait: Timing,
    total: Timing,
    deadline_misses: AtomicUsize,
}

impl BatchMetrics {
    fn complete(&self, elapsed: Duration, budget: Duration) {
        self.total.record(elapsed);
        if elapsed > budget {
            self.deadline_misses.fetch_add(1, Relaxed);
        }
    }
}

struct Timing {
    milliseconds: [AtomicUsize; 64],
    peak_us: AtomicU64,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            milliseconds: [const { AtomicUsize::new(0) }; 64],
            peak_us: AtomicU64::new(0),
        }
    }
}

impl Timing {
    fn record(&self, elapsed: Duration) {
        let us = elapsed.as_micros() as u64;
        let bucket = us.div_ceil(1000).min((self.milliseconds.len() - 1) as u64) as usize;
        self.milliseconds[bucket].fetch_add(1, Relaxed);
        self.peak_us.fetch_max(us, Relaxed);
    }

    fn p99_ms(&self) -> Option<usize> {
        let counts = self
            .milliseconds
            .each_ref()
            .map(|count| count.load(Relaxed));
        let mut remaining = (counts.iter().sum::<usize>() * 99).div_ceil(100);
        if remaining == 0 {
            return None;
        }
        counts.iter().position(|count| {
            remaining = remaining.saturating_sub(*count);
            remaining == 0
        })
    }
}

impl Meters {
    fn snapshot(&self) -> Status {
        Status {
            model_ready: self.model_ready.load(Acquire),
            live: self.live.load(Relaxed),
            input: f32::from_bits(self.input.swap(0, Relaxed)),
            output: f32::from_bits(self.output.swap(0, Relaxed)),
            underruns: self.underruns.load(Relaxed),
            recoveries: self.recoveries.load(Relaxed),
            peak_frames: self.peak_frames.load(Relaxed),
            input_pending_frames: self.input_pending_frames.load(Relaxed),
            output_pending_frames: self.output_pending_frames.load(Relaxed),
            inference_p99_ms: self.timing.inference.p99_ms(),
            inference_peak_us: self.timing.inference.peak_us.load(Relaxed),
            queue_wait_p99_ms: self.timing.queue_wait.p99_ms(),
            batch_p99_ms: self.timing.total.p99_ms(),
            batch_deadline_misses: self.timing.deadline_misses.load(Relaxed),
            last_reset_us: self.last_reset_us.load(Relaxed),
            heartbeat: self.heartbeat.load(Relaxed),
        }
    }
}

struct Shared {
    stop: Event,
    stopped: AtomicBool,
    measuring: AtomicBool,
    cancel_measurement: AtomicBool,
    gains: [AtomicI32; 2],
    meters: Meters,
}

impl Shared {
    fn stop(&self) {
        self.stopped.store(true, Relaxed);
        self.stop.signal();
    }
}

pub fn run_child() -> Result<()> {
    let mut reader = BufReader::new(io::stdin());
    let mut buffer = Vec::new();
    let Some(Control::Start {
        protocol: 1,
        config,
    }) = receive(&mut reader, &mut buffer)?
    else {
        bail!("Missing compatible audio engine startup message");
    };
    let result = child_session(&config, reader, buffer);
    if let Err(error) = &result {
        let _ = send(
            &mut io::stdout().lock(),
            &Update::Fault(format!("{error:#}")),
        );
    }
    result
}

fn child_session(
    config: &Config,
    mut reader: impl BufRead + Send + 'static,
    mut buffer: Vec<u8>,
) -> Result<()> {
    ensure!(
        config.processing.valid()
            && config.gains.valid()
            && !config.microphone.is_empty()
            && !config.cable.is_empty(),
        "Invalid audio engine configuration."
    );
    let shared = Arc::new(Shared {
        stop: Event::new(true)?,
        stopped: AtomicBool::new(false),
        measuring: AtomicBool::new(false),
        cancel_measurement: AtomicBool::new(false),
        gains: [
            AtomicI32::new(config.gains.mic_db),
            AtomicI32::new(config.gains.output_db),
        ],
        meters: Meters::default(),
    });
    let tap = Tap::new();
    let (measure_tx, measure_rx) = crossbeam_channel::bounded(1);
    let (result_tx, result_rx) = crossbeam_channel::bounded(1);
    let commands = shared.clone();
    let rejected = result_tx.clone();
    thread::spawn(move || {
        while let Ok(Some(command)) = receive::<Control>(&mut reader, &mut buffer) {
            match command {
                Control::Stop | Control::Start { .. } => break,
                Control::Gains(gains) if gains.valid() => {
                    commands.gains[0].store(gains.mic_db, Relaxed);
                    commands.gains[1].store(gains.output_db, Relaxed);
                }
                Control::Gains(_) => break,
                Control::Measure
                    if commands.meters.live.load(Relaxed)
                        && !commands.measuring.swap(true, Relaxed) =>
                {
                    commands.cancel_measurement.store(false, Relaxed);
                    if measure_tx.try_send(()).is_err() {
                        commands.measuring.store(false, Relaxed);
                    }
                }
                Control::Measure if !commands.measuring.load(Relaxed) => {
                    let _ = rejected.try_send(Err(
                        "Wait for the route to go live, then try again.".to_owned(),
                    ));
                }
                Control::Measure => {}
                Control::CancelMeasurement => commands.cancel_measurement.store(true, Relaxed),
            }
        }
        commands.stop();
    });
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut writer = BufWriter::new(io::stdout().lock());
            loop {
                if let Ok(result) = result_rx.try_recv() {
                    if send(&mut writer, &Update::Measured(result)).is_err() {
                        shared.stop();
                        break;
                    }
                }
                if shared.stopped.load(Relaxed) {
                    break;
                }
                if send(&mut writer, &Update::Status(shared.meters.snapshot())).is_err() {
                    shared.stop();
                    break;
                }
                let _ = wasapi::wait([&shared.stop], 40);
            }
        });
        scope.spawn(|| {
            while !shared.stopped.load(Relaxed) {
                match measure_rx.recv_timeout(Duration::from_millis(40)) {
                    Ok(()) => {
                        let result = latency::measure(
                            &config.cable,
                            &shared.stop,
                            &tap,
                            || {
                                shared.stopped.load(Relaxed)
                                    || shared.cancel_measurement.load(Relaxed)
                            },
                            || {
                                (
                                    shared.meters.live.load(Relaxed),
                                    shared.meters.underruns.load(Relaxed),
                                    shared.meters.recoveries.load(Relaxed),
                                )
                            },
                        );
                        let _ = result_tx.try_send(result.map_err(|error| format!("{error:#}")));
                        shared.measuring.store(false, Relaxed);
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        let result = run_audio(config, &shared, &tap);
        shared.stop();
        result
    })
}

fn run_audio(config: &Config, shared: &Shared, tap: &Tap) -> Result<()> {
    let profile = preferences::load_profile(&preferences::profile_path()?)?
        .context("Record your voice before starting isolation.")?;
    ensure!(
        profile.microphone == config.microphone,
        "Record your voice again for the selected microphone."
    );
    let backend = config.processing.backend;
    let mut model = VoxLock::open(
        &config.models,
        backend.live_precision(),
        &profile.samples,
        config.processing.threads,
        backend,
    )
    .with_context(|| format!("Could not start the {} backend", backend.label()))?;
    drop(profile);
    if backend == Backend::Cuda {
        // Initialize GPU kernels before starting the microphone and cable.
        model
            .warm_up(config.processing.batch_ms * RATE / 1000)
            .context("Could not warm up the CUDA backend")?;
    }
    shared.meters.heartbeat.fetch_add(1, Relaxed);
    shared.meters.model_ready.store(true, Release);
    let mut consecutive = 0;
    while !shared.stopped.load(Relaxed) {
        let start = Instant::now();
        let result = generation(config, shared, &mut model, tap);
        shared.meters.live.store(false, Relaxed);
        if shared.stopped.load(Relaxed) {
            return Ok(());
        }
        let error = result.err().context("Audio route stopped unexpectedly")?;
        if start.elapsed() >= Duration::from_secs(5) {
            consecutive = 0;
        }
        consecutive += 1;
        if consecutive > 3 {
            return Err(error).context("The selected devices did not recover.");
        }
        shared.meters.recoveries.fetch_add(1, Relaxed);
        shared.meters.heartbeat.fetch_add(1, Relaxed);
        let _ = wasapi::wait([&shared.stop], 250 * consecutive as u32)?;
        if !shared.stopped.load(Relaxed) {
            let reset = Instant::now();
            model.reset()?;
            shared
                .meters
                .last_reset_us
                .store(reset.elapsed().as_micros() as u64, Relaxed);
        }
    }
    Ok(())
}

fn generation(config: &Config, shared: &Shared, model: &mut VoxLock, tap: &Tap) -> Result<()> {
    let batch = config.processing.batch_ms * RATE / 1000;
    let budget = Duration::from_millis(config.processing.batch_ms as u64);
    let playback = Playback::new(config.processing.correction);
    let (mut input_tx, mut input_rx) = playback::ring();
    // The extra slot allows the worker to pop PCM before consuming its timestamp.
    let (arrival_tx, arrival_rx) = crossbeam_channel::bounded(playback::LIMIT / batch + 1);
    let (mut output_tx, mut output_rx) = playback::ring();
    let render_period_ms = AtomicU32::new(10);
    let ready = Event::new(false)?;
    let writable = Event::new(false)?;
    let fault = Event::new(true)?;
    let terminate = Event::new(true)?;
    let mut block = vec![0.0; batch];
    let mut filtered = Vec::with_capacity(batch + 512);
    let mut gain = [1.0, 1.0];
    thread::scope(|scope| {
        let io = scope.spawn(|| {
            let mut playback = playback;
            let mut partial_batch = 0;
            let result = (|| -> Result<()> {
                let _com = Com::init()?;
                let devices = wasapi::enumerator()?;
                let mut capture = Stream::open(&devices, &config.microphone, eCapture, false)?;
                let mut render = Stream::open(&devices, &config.cable, eRender, true)?;
                let render_period = render.period()?;
                let padding = playback
                    .configure(batch, capture.period()?, render_period, render.capacity)
                    .context("The Windows audio periods are too large for live isolation.")?;
                render_period_ms.store((render_period * 1000).div_ceil(RATE) as u32, Relaxed);
                render.render(padding, |_| false)?;
                let _priority = AudioPriority::register();
                if wasapi::wait([&shared.stop, &terminate], 0)?.is_some() {
                    return Ok(());
                }
                capture.start()?;
                render.start()?;
                let mut last = [Instant::now(); 2];
                loop {
                    match wasapi::wait(
                        [&shared.stop, &terminate, &capture.event, &render.event],
                        1500,
                    )? {
                        Some(0 | 1) => return Ok(()),
                        Some(2) => {
                            if capture.capture(|packet| {
                                ensure!(!packet.gap, "Microphone stream was interrupted.");
                                let frames = partial_batch + packet.frames;
                                if frames >= batch {
                                    let arrived = Instant::now();
                                    // Publish timestamps before the PCM they describe.
                                    for _ in 0..frames / batch {
                                        arrival_tx
                                            .try_send(arrived)
                                            .context("Isolation fell behind the microphone.")?;
                                    }
                                }
                                partial_batch = frames % batch;
                                ensure!(
                                    input_tx.push(packet.samples, packet.frames),
                                    "Isolation fell behind the microphone."
                                );
                                tap.push(&packet);
                                Ok(())
                            })? {
                                last[0] = Instant::now();
                                ready.signal();
                            }
                        }
                        Some(3) => {
                            render.render(padding, |frames| {
                                if !matches!(wasapi::wait([&shared.stop, &terminate], 0), Ok(None))
                                {
                                    return false;
                                }
                                let fill = playback.fill(&mut output_rx, frames);
                                shared
                                    .meters
                                    .output_pending_frames
                                    .store(fill.pending, Relaxed);
                                shared.meters.peak_frames.fetch_max(fill.pending, Relaxed);
                                shared.meters.live.store(fill.primed, Relaxed);
                                if fill.underrun {
                                    shared.meters.underruns.fetch_add(1, Relaxed);
                                }
                                if fill.produced > 0 {
                                    writable.signal();
                                }
                                fill.produced > 0
                            })?;
                            last[1] = Instant::now();
                        }
                        _ => bail!("Windows audio stopped responding."),
                    }
                    ensure!(
                        last.iter()
                            .all(|t| t.elapsed() < Duration::from_millis(1500)),
                        "The microphone or cable stopped responding."
                    );
                }
            })();
            fault.signal();
            result
        });
        let process = (|| -> Result<()> {
            while !shared.stopped.load(Relaxed) {
                match wasapi::wait([&shared.stop, &fault, &ready], 1500)? {
                    Some(0 | 1) => break,
                    Some(2) => {}
                    _ => bail!("The microphone stopped supplying audio."),
                }
                while input_rx.pop(&mut block) {
                    if wasapi::wait([&shared.stop, &fault], 0)?.is_some() {
                        return Ok(());
                    }
                    let arrived = arrival_rx
                        .try_recv()
                        .context("Missing capture batch timestamp")?;
                    shared.meters.timing.queue_wait.record(arrived.elapsed());
                    let targets = shared
                        .gains
                        .each_ref()
                        .map(|value| db_gain(value.load(Relaxed)));
                    apply_gain(&mut block, gain[0], targets[0]);
                    shared
                        .meters
                        .input
                        .fetch_max(wasapi::peak(&block).to_bits(), Relaxed);
                    shared
                        .meters
                        .input_pending_frames
                        .store(input_rx.pending(), Relaxed);
                    let inference = Instant::now();
                    model.process(&block, &mut filtered)?;
                    shared.meters.timing.inference.record(inference.elapsed());
                    apply_gain(&mut filtered, gain[1], targets[1]);
                    gain = targets;
                    shared
                        .meters
                        .output
                        .fetch_max(wasapi::peak(&filtered).to_bits(), Relaxed);
                    shared.meters.heartbeat.fetch_add(1, Relaxed);
                    let mut written = output_tx.push_some(&filtered);
                    if written < filtered.len() {
                        // A partial publish can reach the priming threshold before render drains
                        // the ring. Allow time to drain the tail plus one render period.
                        let tail_ms = ((filtered.len() - written) * 1000).div_ceil(RATE) as u64;
                        let wait_ms = tail_ms + u64::from(render_period_ms.load(Relaxed)) + 10;
                        let deadline = Instant::now() + Duration::from_millis(wait_ms);
                        while written < filtered.len() {
                            let remaining = deadline.saturating_duration_since(Instant::now());
                            if remaining.is_zero() {
                                break;
                            }
                            match wasapi::wait(
                                [&shared.stop, &fault, &writable],
                                remaining.as_nanos().div_ceil(1_000_000) as u32,
                            )? {
                                Some(0 | 1) => return Ok(()),
                                Some(2) => written += output_tx.push_some(&filtered[written..]),
                                _ => break,
                            }
                        }
                    }
                    shared.meters.timing.complete(arrived.elapsed(), budget);
                    ensure!(
                        written == filtered.len(),
                        "The virtual cable fell behind isolation."
                    );
                }
            }
            Ok(())
        })();
        terminate.signal();
        let io = io
            .join()
            .map_err(|_| anyhow::anyhow!("Windows audio thread failed"))?;
        process.and(io)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_includes_queueing_and_output_publication() {
        let metrics = BatchMetrics::default();
        let budget = Duration::from_millis(16);
        metrics.queue_wait.record(Duration::from_millis(7));
        metrics.inference.record(Duration::from_millis(6));
        metrics.complete(Duration::from_millis(16), budget);
        metrics.complete(Duration::from_millis(17), budget);
        assert_eq!(metrics.deadline_misses.load(Relaxed), 1);
        assert_eq!(metrics.queue_wait.p99_ms(), Some(7));
        assert_eq!(metrics.inference.p99_ms(), Some(6));
        assert_eq!(metrics.total.p99_ms(), Some(17));
    }
}

fn db_gain(db: i32) -> f32 {
    10.0_f32.powf(db as f32 / 20.0)
}

fn apply_gain(samples: &mut [f32], from: f32, to: f32) {
    let count = samples.len() as f32;
    for (i, sample) in samples.iter_mut().enumerate() {
        let gain = from + (to - from) * (i + 1) as f32 / count;
        *sample = (*sample * gain).clamp(-1.0, 1.0);
    }
}

pub fn default_config(microphone: String, cable: String) -> Config {
    Config {
        microphone,
        cable,
        processing: Processing::default(),
        gains: Gains::default(),
        models: default_models(),
    }
}
