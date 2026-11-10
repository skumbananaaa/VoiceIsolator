# Development dependency provenance

The native dependency lock records the archives, source revisions and model SHA-256 values. [Cargo.lock](Cargo.lock) pins the Rust dependencies for the desktop and harness. Keep the corresponding licenses and notices with any redistributable bundle.

| Component | License and provenance |
|---|---|
| sherpa-onnx | Apache-2.0. The Rust build uses the C API from the pinned PCM SDK. The optimized SDK includes the VoiceIsolator modifications listed below. [License](https://github.com/k2-fsa/sherpa-onnx/blob/111617b114bc12c895a202cdc5ee2db3403d53f2/LICENSE) |
| egui and eframe | MIT or Apache-2.0. The desktop uses the Glow renderer and AccessKit integration. [Project license](https://github.com/emilk/egui#license) |
| tray-icon | MIT or Apache-2.0. Provides the Windows tray icon and menu. [Project license](https://github.com/tauri-apps/tray-icon#license) |
| minisign-verify | MIT. Verifies GitHub release manifests with the embedded public key. [Project license](https://github.com/jedisct1/rust-minisign-verify) |
| ort-sys | MIT or Apache-2.0. Rust bindings for the ONNX Runtime CUDA startup check. [Project license](https://github.com/pykeio/ort) |
| windows-rs | MIT or Apache-2.0. Rust bindings for the Windows APIs used by the audio engine and profile store. [Project license](https://github.com/microsoft/windows-rs#license) |
| VoxLock reference source | Apache-2.0. Source version and weights are pinned separately. [License](https://github.com/arbazsiddiqui/voxlock/blob/v1.3.0/LICENSE) |
| VoxLock ONNX weights | Upstream declares Apache-2.0. Keep the upstream weight-origin notice with the models. The sherpa-compatible releases add metadata to the model files. [Model repository and notice](https://huggingface.co/arbazsiddiqui/voxlock/tree/main) |
| ONNX Runtime | MIT, with its own third-party notices. The CPU binaries come with sherpa-onnx. The CUDA option uses Microsoft's pinned Windows CUDA package and requires NVIDIA runtime libraries. [License](https://github.com/microsoft/onnxruntime/blob/v1.28.2/LICENSE) · [Notices](https://github.com/microsoft/onnxruntime/blob/v1.28.2/ThirdPartyNotices.txt) |
| Upstream audio fixtures | Downloaded for development verification from the sherpa-onnx model release. Keep them out of the app distribution pending their separate provenance review. [Fixture release](https://github.com/k2-fsa/sherpa-onnx/releases/tag/speech-enhancement-models) |

Audit the exact transitive dependencies and obtain their notices before publishing a portable binary release. VB-CABLE is a separately installed proprietary donationware driver. [VB-Audio licensing](https://vb-audio.com/Services/licensing.htm)

## VoiceIsolator modifications to sherpa-onnx

VoiceIsolator applies [one patch](cmake/sherpa-voxlock.patch) to sherpa-onnx commit `111617b114bc12c895a202cdc5ee2db3403d53f2` when building the optimized SDK. It changes these upstream files.

| Upstream file | VoiceIsolator change |
|---|---|
| `sherpa-onnx/c-api/c-api.cc` | Move denoised audio into its C API result to avoid a sample copy. |
| `sherpa-onnx/csrc/offline-speech-denoiser-voxlock-model.cc` | Add optional ONNX Runtime chunk profiling and CUDA I/O binding. |
| `sherpa-onnx/csrc/offline-speech-denoiser-voxlock-model.h` | Define the per-stream storage and entry points for the optional binding. |
| `sherpa-onnx/csrc/online-speech-denoiser-voxlock-impl.h` | Skip the temporary input copy at the model sample rate and reuse the enrolled stream on reset. |
| `sherpa-onnx/csrc/speech-denoiser-voxlock-stream.h` | Reset recurrent and audio state while retaining speaker conditioning. Dispatch to the bound chunk path when supported. |
