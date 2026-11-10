# VoiceIsolator

VoiceIsolator is a native Windows x64 Rust application for isolating an enrolled speaker from a live microphone and routing the result to a virtual cable. The egui desktop supervises a separate Rust audio process. Enrollment and inference run locally through the pinned native sherpa-onnx C API and VoxLock weights.

The Windows installer uses the CPU backend and includes the INT8 models. Install [VB-CABLE](https://vb-audio.com/Cable/) separately, then select your microphone and cable in VoiceIsolator and record your voice. Closing the window parks the app in the tray. Choose **Quit** in the tray to close the app or **Stop isolation** to stay in standby on the next launch. The installed app offers **Start with Windows** under Settings.

See [release instructions](docs/release.md) for building and signing the installer.

## Repository documentation

- [Architecture](docs/architecture.md) — live audio flow, speaker enrollment, process supervision, and latency measurement.
- [Development guide](docs/development.md) — source map, native dependencies, and how to choose a test.
- [Phase 1 validation](docs/phase-1-validation.md) — fixture tests, room evaluation, and paced benchmarks.

## Open the desktop app

Open **voiceisolator_live.exe** in `out/windows-x64-pcm/Release`. Keep it beside the matching DLLs. The live target uses the PCM SDK and matching VoxLock 1.2.0 models. It reads the pinned development model folder or a `models` folder beside the executable.

1. Install VB-CABLE separately and reboot as directed by its installer. [VB-CABLE download](https://vb-audio.com/Cable/)
2. Open VoiceIsolator Live. It restores your microphone and virtual cable choices when available. On first use it prefers your Windows communications microphone and the standard VB-CABLE **CABLE Input**. Check the selections and use **Refresh devices** after connecting hardware.
3. Select **Record voice** and speak alone for six seconds at your usual mic distance. That button becomes **Cancel recording** during enrollment and **Forget voice** once a profile is saved. The app saves the selected three seconds encrypted for your Windows account under `%LOCALAPPDATA%\VoiceIsolator`. The profile is available on subsequent launches with the mic you recorded it on.
4. Select **Start isolation**. Wait for the active status and watch the **Isolated level** meter as you speak. The same button becomes **Stop isolation** while there are no pending changes. **Session → Stop isolation** is also available when editing live settings.
5. In Discord, choose **CABLE Output** as your input device. Keep Discord output on your headset. For a comparison, disable Discord's own suppression and automatic gain where available.

Drag the dials up or down, scroll over them, or focus a dial and use the arrow keys. **Home** resets the focused dial. When you edit a setting during isolation, the isolation button becomes **Apply changes**. Gain-only changes apply to the live stream. Changing the virtual cable, **Backend**, **Batch size** (16–40 ms), **CPU threads** (1–4), or **Clock correction** restarts the stream. **Settings → Reset processing settings** restores CPU inference, 0 dB gain, 16 ms batches, one thread, and Linear clock correction. During isolation the reset is staged until you apply it. Stop isolation to change the microphone or voice profile.

**Backend** selects sherpa-onnx's CPU or NVIDIA CUDA execution provider. The live CPU option uses the INT8 model and CUDA uses the FP32 model. The optimized SDK keeps speaker conditioning and recurrent state on the GPU when the CUDA session supports it. CUDA needs the optional runtime below and warms up before opening the audio route, so a cold start can take longer. If CUDA cannot initialize, the app reports the error and leaves the route closed. ONNX Runtime may still run individual operations on the CPU when the CUDA provider is active.

Use **Settings → Dark mode** to switch themes. VoiceIsolator restores your last choice and defaults to dark on a fresh profile.

Use **Clock correction** to choose how VoiceIsolator interpolates between samples as the microphone and cable clocks drift. **Linear** is the default and uses less CPU. **Band-limited (sinc)** preserves more high-frequency detail and needs up to 24 future samples at 16 kHz (1.5 ms), which can fit inside the existing playback buffer.

Once the stream is live, select **Measure latency** and speak alone in short phrases for about six seconds. The app compares timestamped microphone audio with the matching VB-CABLE recording endpoint. It displays the median and 95th percentile of the matched windows, plus the fraction of eligible microphone windows that matched. Brief playback interruptions are excluded from the result. If waveform matching fails but changes in speech level give a consistent result, the panel marks it **Approx.** This includes VoiceIsolator and the cable through **CABLE Output**. Discord, the network and headphone playback are outside this measurement. The probe runs in memory and adds no sound to the route. Keep other audio off the cable and leave the settings steady during the test. If no number appears, check the message in the latency panel and try again.

This prototype uses shared-mode WASAPI, with Windows converting the selected device formats to and from the model's mono 16 kHz stream. The software prefill is at least 34 ms and accounts for the batch size, capture period and resampler lookahead. The cable render target is two shared periods, capped by the endpoint's capacity.

The route starts muted and queues only processed audio. A burst can fill the available space while playback is priming. Render then drains the queue so the worker can publish the rest in order. The worker's wait is bounded by the remaining burst, a render period and a scheduling margin. If it times out, or if capture reports a discontinuity, the app discards the old queues, resets VoxLock and reopens the same endpoints. Repeated failures stop the route. The window supervises a separate live audio process and can stop it if inference hangs. Refresh and restart after reconnecting a lost device.

**Queue underruns** count refills where the app ran short of processed samples. **Restarts** counts stream recovery attempts. **Peak queue** is the largest software output queue observed at refill time, excluding resampler history. End-to-end latency and audible stability still require device testing.

The live diagnostics show p99 batch time and misses against the selected batch interval. Timing starts when the capture callback receives the last samples for a batch and ends after the worker queues any output from that batch. Hover to see input wait p99, inference times, and the last recovery reset time. Queue sizes are shown in milliseconds of audio. Use **Measure latency** for the mic-to-cable measurement.

Install the Rust x64 MSVC toolchain and Visual Studio with **Desktop development with C++** and a Windows SDK. To prepare the pinned sherpa SDK with Visual Studio 2026, use CMake 4.2 or later. Run these commands from the project root.

```powershell
git submodule update --init --recursive
cmake -P scripts/fetch-deps.cmake
.\scripts\build-pcm-sdk.ps1 -Optimized
.\scripts\build-rust.ps1 -OptimizedSdk
```

For Visual Studio 2022, pass `-Generator 'Visual Studio 17 2022'` to the SDK script. The Rust script always installs both executables and the matching DLLs in `out/windows-x64-pcm/Release`. Use `-CloseRunningApp` to close the current VoiceIsolator window before replacing it. Use `-Offline` once Cargo's dependencies are cached.

### Enable CUDA

Use an NVIDIA GPU and a compatible driver. Make the CUDA 12.x and cuDNN 9.x runtime DLLs available beside the executable or on the Windows `PATH` when you launch VoiceIsolator. The CUDA build uses the same sherpa SDK with Microsoft's matching ONNX Runtime 1.28.2 CUDA 12 package. [ONNX Runtime CUDA requirements](https://onnxruntime.ai/docs/execution-providers/CUDA-ExecutionProvider.html)

```powershell
cmake -DVOICEISOLATOR_CUDA=ON -P scripts/fetch-deps.cmake
.\scripts\build-rust.ps1 -OptimizedSdk -Cuda -CloseRunningApp
```

Both backend choices use the same output folder. If you copy models beside the executable for portable use, include `chunk.onnx` for CUDA alongside the INT8 model and enrollment model.

## Developer WAV harness

The WAV harness is a developer diagnostic. The desktop processes live microphone audio directly. Download the public test fixture and run the Rust checks with the same PCM SDK as the live application.

```powershell
cmake -DVOICEISOLATOR_TEST_FIXTURES=ON -P scripts/fetch-deps.cmake
.\scripts\build-rust.ps1 -OptimizedSdk -Test

$cli = '.\out\windows-x64-pcm\Release\voiceisolator.exe'
$models = '.\.deps\models\voxlock-v2'
$fixture = '.\.deps\models\sherpa-onnx-voxlock-int8\test_wavs'
& $cli isolate --models $models --enrollment "$fixture\en-voice-sample-f.wav" --input "$fixture\en-combined.wav" --output '.\out\fixture-isolated.wav'
```

For offline use outside the project, place the pinned VoxLock 1.2.0 weights in a `models` directory next to the executable. Keep the runtime DLLs beside the executable and install the [Microsoft x64 Visual C++ Redistributable](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist) on a test machine without Visual Studio.

Input and enrollment must be **mono 16 kHz PCM16 WAV**. Use a separate 3–30 second enrollment take in which only the intended speaker talks. The harness refuses to overwrite an existing output file. It emits JSON with the dependency pins, frame counts, startup input requirement and service-time percentiles. For source recordings in another format, an optional developer-side conversion is `ffmpeg -i input.wav -ac 1 -ar 16000 -c:a pcm_s16le converted.wav`.

Use `--backend cpu` or `--backend cuda` for harness runs. The JSON report records the backend and model precision. Pass `--precision fp32` on both backends to compare the same weights.

## Run the room evaluation

See [Phase 1 validation](docs/phase-1-validation.md) for recording, scoring and benchmark procedures, including the experimental in-memory enrollment build. Recordings and reports belong in the ignored `recordings/` and `reports/` directories.

The `third_party/voxlock` Git submodule pins upstream VoxLock tag `v1.3.0`, commit `b523f063b816c4f909e262f8773652d5a80cfa50`. Run `git submodule update --init --recursive` to populate it after cloning. The WAV executable uses the independently pinned native sherpa-onnx SDK and weights from [the dependency lock](cmake/Dependencies.cmake).

## License

Application code is Apache-2.0. See [third-party provenance](THIRD_PARTY_NOTICES.md).
