# Phase 1 validation

## Native smoke tests

The Rust build uses the pinned sherpa-onnx PCM SDK and VoxLock 1.2.0. Its tests cover invalid enrollment and model hashes, streaming counts and reset, short WAV tails, profile persistence, bounded audio queues, clock drift and latency matching. The engine startup test checks that missing enrollment leaves the route closed. The adapter removes the 128-sample enrollment check from its returned audio.

```powershell
cmake -DVOICEISOLATOR_TEST_FIXTURES=ON -DVOICEISOLATOR_MODEL_VARIANTS=ON -P scripts/fetch-deps.cmake
.\scripts\build-pcm-sdk.ps1
.\scripts\build-rust.ps1 -Test
```

Use `out/windows-x64-pcm/Release/voiceisolator.exe` with `.deps/models/voxlock-v2`. For VS 2022 pass `-Generator 'Visual Studio 17 2022'` to the SDK script. Omit `-DVOICEISOLATOR_MODEL_VARIANTS=ON` for an INT8-only run.

## Record in the target room

1. Make a clean enrollment take of 6–10 seconds from the intended mic while the target speaks alone. Capture it separately from the evaluation set.
2. Record the actual mic at normal gain and distance. Include target-only, interferer-only, interrupted and simultaneous speech, the louder interferer, quiet and off-axis target, similar voice levels, and keyboard/fan noise. Include clips where the other person starts. Save raw recordings and label segment boundaries. Retain held-out takes for the final comparison.
3. For controlled overlap measurements, record synchronized isolated stems where possible or create mixtures from separate takes at −5, 0 and +5 dB target-to-interferer ratios. Keep the target stem time-aligned with the raw mix and output. Use the real room recording for listening when stems are unavailable.
4. Process the same input through each model/build. Compare raw, native output, upstream VoxLock reference using explicit enrollment and the corresponding weights, and Krisp Desktop if available. Note any differences in input routing or DSP. Preserve original relative levels for leakage scoring.

The room recordings stay on the test PC. Write them under `recordings/` or any local directory outside Git. Write comparison reports under `reports/`.

## Score and listen

Prepare a local JSON manifest with paths relative to its folder. Use zero-based sample offsets with exclusive ends. A mix and its output must have equal lengths. Align and trim a controlled target stem to that same time base before scoring.

```json
{
  "takes": [
    {
      "raw": "mix.wav",
      "isolated": "mix-isolated.wav",
      "target_stem": "target.wav",
      "segments": [
        {"kind": "interferer", "start": 16000, "end": 48000},
        {"kind": "overlap", "start": 64000, "end": 96000}
      ]
    }
  ]
}
```

Run `python scripts/score-room.py recordings/room.json` with Python 3.10 or later. This optional development scorer reports per-segment attenuation, SI-SDR improvement and samples at the PCM limit. Its quantitative gate requires interferer-only attenuation of at least 20 dB median and 12 dB at the 10th percentile, and overlap SI-SDR improvement of at least 5 dB median.

For listening, randomize at least 20 held-out overlap clips plus target-only and interferer-only controls. Both players should rate at least 90% of overlap clips usable. When Krisp is available, require median naturalness and leakage ratings within one point of Krisp on five-point scales. Repeated intelligible bleed or lost target phrases blocks advancement even if the quantitative gate passes.

## Run the paced matrix

```powershell
.\scripts\run-benchmark.ps1 -Cli out/windows-x64-pcm/Release/voiceisolator.exe -Models .deps/models/voxlock-v2 -Enrollment recordings/enrollment.wav -InputWav recordings/room-mixture.wav -ReportDirectory reports/baseline-idle
```

The default screen runs 10/20/40/80 ms at 1/2/4 threads, with 20 seconds per case. Screen 8/16/24 ms with `-ChunkMs 8,16,24` and compare the finalist cadences with `-Precision int8,int8-full,fp32,fp16`. Run finalists with `-Seconds 600` at idle and during the actual game. Use a new report directory for each run. Record the CPU, OS, game, mic, model, precision and client settings.

The CLI reports p50/p95/p99/max around the complete processing call, including startup calls. It measures setup, backend creation and PCM enrollment separately from steady-state work. Paced reports also show deadline misses, p99 arrival backlog and the change in mean backlog from the first to the last quarter. These include host scheduling. Measure WASAPI and cable latency with the live app and validate Discord separately.

Use `-Backend cpu,cuda -Precision fp32` to compare identical weights. Omit `-Precision` to compare the live defaults, CPU INT8 and CUDA FP32. The runner records the CLI, sherpa and ONNX Runtime DLL hashes. CUDA warm-up and the reset immediately after it are timed separately.

### Native SDK comparison and chunk tracing

The stock SDK and the optimized SDK build in separate worktrees at the same pinned revision. The optimized patch keeps the speaker tensors across a stream reset, skips a redundant copy for audio already at the model sample rate, and moves the native output buffer into the C API result. Select the SDK explicitly when preparing a comparison.

```powershell
.\scripts\build-pcm-sdk.ps1 -Optimized
.\scripts\build-rust.ps1 -OptimizedSdk -Cuda -Test
```

For an isolated trace, pass a sherpa config file with the following contents through `-OrtConfig` in the matrix runner or `--ort-config` in the bench CLI. Create the destination directory first. The optimized SDK profiles the actual chunk session separately from enrollment.

```ini
SessionConfig.VoiceIsolatorVoxLockProfile=reports/chunk
```

Run `scripts/summarize-ort-profile.ps1 -Profile <trace.json>` for executed node placement and the copy nodes visible to ONNX Runtime's graph profiler. The first chunk logs whether it used I/O binding or shows the tensor sizes and device locations for a plain run. Use unprofiled runs for timing comparisons.

The optimized SDK enables I/O binding when CUDA places the conditioning and both recurrent states on device and the PCM output shape supports a reusable buffer. It binds conditioning once per stream generation and reuses two sets of recurrent-state buffers on the device. For comparisons, set `SessionConfig.VoiceIsolatorVoxLockIoBinding=0` to use plain inference or `=1` to require binding and fail if the session cannot use it.

The desktop prototype now supports a live route. Use the room quality gate and the sustained p99 worker service-time target of at most 80% of the batch interval before selecting production settings. If isolation fails in the room, compare the native and upstream model paths and check the enrollment and mic setup.

## Historical comparison before the Rust port

The earlier C++ harness measured both INT8 model versions on a Ryzen 7 9800X3D with Windows 11 25H2. A small controlled mix made from the public test voices gave similar quality scores.

| Model lane | Interferer-only attenuation | Overlap SI-SDR improvement |
|---|---:|---:|
| v2.0.1 / VoxLock 1.1.1 | 25.7 dB | 11.1 dB |
| PCM SDK / VoxLock 1.2.0 | 25.6 dB | 11.1 dB |

The fixture used the first 48,000 samples of the public Mandarin female sample for enrollment and the remainder as a target stem. The male sample played alone for one second before the target entered, with RMS matched during overlap and peak headroom preserved. This checks the harness and scorer on one artificial mixture. The room gate remains open pending recordings, listening and the 10-minute finalist runs under the actual gaming workload.

The two-second, high-resolution INT8 screen covered 10/16/20/40/80 ms at 1/2/4 threads on both builds.

| Batch / threads | Tagged p99 service time | PCM p99 service time |
|---|---:|---:|
| 10 ms / 2 | 17.0 ms | 6.7 ms |
| 16 ms / 1 | 21.8 ms | 8.4 ms |
| 20 ms / 2 | 19.0 ms | 9.0 ms |
| 40 ms / 1 | 27.0 ms | 12.9 ms |

Start the room evaluation with the PCM build at 16 ms and one thread. It had zero deadline misses in this short screen and produced its first isolated frames after 32 ms of supplied input in the fixture test. Keep 10 ms and two threads as a comparison. These are worker measurements on file input. The live latency and final configuration depend on the room and Windows audio tests.
