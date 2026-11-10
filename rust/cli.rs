use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use std::{
    collections::HashMap,
    ffi::OsString,
    path::PathBuf,
    time::{Duration, Instant},
};
use voiceisolator::{
    RATE,
    isolation::{Backend, Precision, VoxLock},
    wav,
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_OBJECT_0},
        System::{
            ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
            Threading::{
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, GetCurrentProcess,
                GetProcessTimes, SetWaitableTimer, WaitForSingleObject,
            },
        },
    },
    core::PCWSTR,
};

struct Pacer(HANDLE);

impl Pacer {
    fn new() -> Result<Self> {
        // TIMER_MODIFY_STATE | SYNCHRONIZE
        Ok(Self(unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                0x100002,
            )?
        }))
    }

    fn wait(&self, due: Instant) -> Result<()> {
        loop {
            let remaining = due.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            let delay = -(remaining.as_nanos().div_ceil(100) as i64);
            unsafe {
                SetWaitableTimer(self.0, &delay, 0, None, None, false)?;
                ensure!(
                    WaitForSingleObject(self.0, u32::MAX) == WAIT_OBJECT_0,
                    "Benchmark timer failed"
                );
            }
        }
    }
}

impl Drop for Pacer {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn cpu_ticks() -> Result<u64> {
    let [mut creation, mut exit, mut kernel, mut user] = [FILETIME::default(); 4];
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )?;
    }
    let ticks = |t: FILETIME| u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime);
    Ok(ticks(kernel) + ticks(user))
}

fn peak_working_set() -> Result<usize> {
    let mut stats = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    unsafe {
        K32GetProcessMemoryInfo(GetCurrentProcess(), &mut stats, stats.cb).ok()?;
    }
    Ok(stats.PeakWorkingSetSize)
}

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next().unwrap_or_default();
    if mode.is_empty() || mode == "--help" {
        println!(
            "voiceisolator isolate|bench --models DIR --enrollment WAV --input WAV [--chunk-ms N] [--threads N] [--backend cpu|cuda] [--precision int8|int8-full|fp32|fp16] [--ort-config FILE]"
        );
        println!("isolate requires --output WAV. bench accepts --seconds N and --paced.");
        return Ok(());
    }
    let bench = mode == "bench";
    ensure!(bench || mode == "isolate", "Expected isolate or bench");
    let mut paced = false;
    let mut options = HashMap::<String, OsString>::new();
    while let Some(arg) = args.next() {
        let key = arg
            .into_string()
            .map_err(|_| anyhow::anyhow!("Invalid option name"))?;
        if key == "--paced" && bench {
            ensure!(!paced, "Duplicate --paced");
            paced = true;
            continue;
        }
        ensure!(
            matches!(
                key.as_str(),
                "--models"
                    | "--enrollment"
                    | "--input"
                    | "--chunk-ms"
                    | "--threads"
                    | "--backend"
                    | "--precision"
                    | "--ort-config"
            ) || bench && key == "--seconds"
                || !bench && key == "--output",
            "Unknown option {key}"
        );
        let value = args
            .next()
            .with_context(|| format!("Missing value for {key}"))?;
        ensure!(options.insert(key, value).is_none(), "Duplicate option");
    }
    let path = |key| {
        options
            .get(key)
            .map(PathBuf::from)
            .with_context(|| format!("Missing {key}"))
    };
    let number = |key, default, max| -> Result<usize> {
        let Some(value) = options.get(key) else {
            return Ok(default);
        };
        let number = value
            .to_str()
            .context("Invalid numeric argument")?
            .parse()?;
        ensure!((1..=max).contains(&number), "{key} is out of range");
        Ok(number)
    };
    let backend = match options.get("--backend").map(|value| value.to_str()) {
        None | Some(Some("cpu")) => Backend::Cpu,
        Some(Some("cuda")) => Backend::Cuda,
        _ => bail!("Unknown backend. Expected cpu or cuda"),
    };
    let precision_name = options
        .get("--precision")
        .and_then(|x| x.to_str())
        .unwrap_or(backend.live_precision().name());
    let precision = match precision_name {
        "int8" => Precision::Int8,
        "int8-full" => Precision::Int8Full,
        "fp32" => Precision::Fp32,
        "fp16" => Precision::Fp16,
        _ => bail!("Unknown precision"),
    };
    let output_path = (!bench).then(|| path("--output")).transpose()?;
    let input = wav::read(&path("--input")?)?;
    let profile = wav::read(&path("--enrollment")?)?;
    let chunk_ms = number("--chunk-ms", 16, 80)?;
    let threads = number("--threads", 1, 4)?;
    let seconds = number("--seconds", 0, 3600)?;
    let total = if seconds == 0 {
        input.len()
    } else {
        seconds * RATE
    };
    let chunk_size = chunk_ms * RATE / 1000;
    let create = Instant::now();
    let ort_config = options.get("--ort-config").map(PathBuf::from);
    let mut model = VoxLock::open_with_ort_config(
        &path("--models")?,
        precision,
        &profile,
        threads as i32,
        backend,
        ort_config.as_deref(),
    )?;
    let setup_ms = create.elapsed().as_secs_f64() * 1000.0;
    let (warmup_ms, warmup_reset_ms) = if backend == Backend::Cuda {
        let start = Instant::now();
        let reset = model.warm_up(chunk_size)?;
        (
            Some(start.elapsed().as_secs_f64() * 1000.0),
            Some(reset.as_secs_f64() * 1000.0),
        )
    } else {
        (None, None)
    };
    let mut output = Vec::with_capacity(if bench { 0 } else { total });
    let mut block = Vec::with_capacity(chunk_size);
    let mut filtered = Vec::with_capacity(chunk_size + 512);
    let mut service = Vec::with_capacity(total.div_ceil(chunk_size));
    let mut backlog = Vec::new();
    let mut samples = 0;
    let mut clipped = 0;
    let mut first = None;
    let mut misses = 0;
    let mut worst_lateness = 0.0_f64;
    let pacer = paced.then(Pacer::new).transpose()?;
    let cpu_start = cpu_ticks()?;
    let start = Instant::now();
    for offset in (0..total).step_by(chunk_size) {
        let end = (offset + chunk_size).min(total);
        block.clear();
        block.extend((offset..end).map(|i| input[i % input.len()]));
        let due = start + Duration::from_secs_f64(end as f64 / RATE as f64);
        if let Some(pacer) = &pacer {
            pacer.wait(due)?;
            backlog.push(due.elapsed().as_secs_f64() * 1000.0);
        }
        let begin = Instant::now();
        model.process(&block, &mut filtered)?;
        let ms = begin.elapsed().as_secs_f64() * 1000.0;
        service.push(ms);
        let interval = (end - offset) as f64 * 1000.0 / RATE as f64;
        let lateness = if paced {
            due.elapsed().as_secs_f64() * 1000.0 - interval
        } else {
            ms - interval
        };
        misses += usize::from(lateness > 0.0);
        if paced {
            worst_lateness = worst_lateness.max(due.elapsed().as_secs_f64() * 1000.0);
        }
        if !filtered.is_empty() {
            first.get_or_insert(end as f64 * 1000.0 / RATE as f64);
        }
        samples += filtered.len();
        clipped += filtered.iter().filter(|s| s.abs() >= 1.0).count();
        if !bench {
            output.extend_from_slice(&filtered);
        }
    }
    let wall_ms = start.elapsed().as_secs_f64() * 1000.0;
    let cpu_ms = (cpu_ticks()? - cpu_start) as f64 / 10_000.0;
    let finish = Instant::now();
    model.finish(&mut filtered)?;
    let finish_ms = finish.elapsed().as_secs_f64() * 1000.0;
    samples += filtered.len();
    clipped += filtered.iter().filter(|s| s.abs() >= 1.0).count();
    ensure!(samples == total, "Output sample count mismatch");
    if let Some(path) = output_path {
        output.extend_from_slice(&filtered);
        wav::write_new(&path, &output)?;
    }
    let service_total: f64 = service.iter().sum();
    service.sort_by(f64::total_cmp);
    let backlog_trend = if paced {
        let width = (backlog.len() / 4).max(1);
        Some(
            (backlog[backlog.len() - width..].iter().sum::<f64>()
                - backlog[..width].iter().sum::<f64>())
                / width as f64,
        )
    } else {
        None
    };
    backlog.sort_by(f64::total_cmp);
    let [(_, enroll_hash), (_, chunk_hash)] = precision.files();
    let stats = json!({
        "status": "ok", "sherpa_commit": env!("SHERPA_PCM_COMMIT"),
        "voxlock_model_revision": env!("VOXLOCK_V2_MODEL_REVISION"),
        "enroll_sha256": enroll_hash, "chunk_sha256": chunk_hash,
        "backend": backend, "ort_config": ort_config, "precision": precision_name, "chunk_ms": chunk_ms,
        "threads": threads, "paced": paced, "input_samples": total,
        "output_samples": samples, "clipped_samples": clipped,
        "first_output_after_input_ms": first,
        "setup_ms_including_hash_and_enrollment": setup_ms,
        "create_ms": model.create_ms, "separate_pcm_enrollment_ms": model.enrollment_ms,
        "warmup_ms": warmup_ms, "warmup_reset_ms": warmup_reset_ms, "finish_ms": finish_ms,
        "service_p50_ms": percentile(&service, 0.50), "service_p95_ms": percentile(&service, 0.95),
        "service_p99_ms": percentile(&service, 0.99), "service_max_ms": service.last(),
        "deadline_misses": misses,
        "service_real_time_factor": service_total * RATE as f64 / (1000.0 * total as f64),
        "pacing_timer": paced.then_some("windows_high_resolution_waitable"),
        "paced_backlog_p99_ms": percentile(&backlog, 0.99),
        "paced_backlog_last_vs_first_quarter_ms": backlog_trend,
        "worst_paced_completion_from_batch_arrival_ms": paced.then_some(worst_lateness),
        "cpu_core_equivalents": cpu_ms / wall_ms.max(0.001),
        "peak_working_set_bytes": peak_working_set()?,
    });
    println!("{}", serde_json::to_string_pretty(&stats)?);
    Ok(())
}

fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    (!sorted.is_empty()).then(|| sorted[(p * sorted.len() as f64).ceil() as usize - 1])
}
