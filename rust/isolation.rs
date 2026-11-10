use std::{
    ffi::{CStr, CString, c_char, c_void},
    fs::{File, OpenOptions},
    io::Read,
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    ptr::{self, NonNull},
    slice,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use windows::{Win32::Security::Cryptography::*, core::w};

use crate::{HOP, PROFILE_SAMPLES, RATE};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[default]
    Cpu,
    Cuda,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Cuda => "CUDA",
        }
    }

    fn provider(self) -> &'static CStr {
        match self {
            Self::Cpu => c"cpu",
            Self::Cuda => c"cuda",
        }
    }

    pub fn live_precision(self) -> Precision {
        match self {
            Self::Cpu => Precision::Int8,
            Self::Cuda => Precision::Fp32,
        }
    }
}

// Layouts from the pinned sherpa PCM SDK's c-api.h.
#[repr(C)]
struct ModelConfig {
    gtcrn: *const c_char,
    num_threads: i32,
    debug: i32,
    provider: *const c_char,
    dpdfnet: *const c_char,
    attenuation_limit: f32,
    voxlock: VoxLockConfig,
}

#[repr(C)]
struct VoxLockConfig {
    enroll_model: *const c_char,
    chunk_model: *const c_char,
    voice_sample: *const c_char,
}

#[repr(C)]
struct Config {
    model: ModelConfig,
}

#[repr(C)]
struct DenoisedAudio {
    samples: *const f32,
    n: i32,
    sample_rate: i32,
}

unsafe extern "C" {
    fn SherpaOnnxCreateOnlineSpeechDenoiser(config: *const Config) -> *mut c_void;
    fn SherpaOnnxDestroyOnlineSpeechDenoiser(handle: *mut c_void);
    fn SherpaOnnxOnlineSpeechDenoiserGetSampleRate(handle: *mut c_void) -> i32;
    fn SherpaOnnxOnlineSpeechDenoiserGetFrameShiftInSamples(handle: *mut c_void) -> i32;
    fn SherpaOnnxOnlineSpeechDenoiserSetVoiceSample(
        handle: *mut c_void,
        samples: *const f32,
        n: i32,
        rate: i32,
    ) -> i32;
    fn SherpaOnnxOnlineSpeechDenoiserRun(
        handle: *mut c_void,
        samples: *const f32,
        n: i32,
        rate: i32,
    ) -> *const DenoisedAudio;
    fn SherpaOnnxOnlineSpeechDenoiserReset(handle: *mut c_void);
    fn SherpaOnnxOnlineSpeechDenoiserFlush(handle: *mut c_void) -> *const DenoisedAudio;
    fn SherpaOnnxDestroyDenoisedAudio(audio: *const DenoisedAudio);
}

#[derive(Clone, Copy, Debug, Default)]
pub enum Precision {
    #[default]
    Int8,
    Int8Full,
    Fp32,
    Fp16,
}

impl Precision {
    pub fn name(self) -> &'static str {
        match self {
            Self::Int8 => "int8",
            Self::Int8Full => "int8-full",
            Self::Fp32 => "fp32",
            Self::Fp16 => "fp16",
        }
    }

    pub fn files(self) -> [(&'static str, &'static str); 2] {
        let enrollment = ("enroll.onnx", env!("VOXLOCK_V2_ENROLL_SHA256"));
        match self {
            Self::Int8 => [
                enrollment,
                ("chunk_int8.onnx", env!("VOXLOCK_V2_CHUNK_SHA256")),
            ],
            Self::Int8Full => [
                enrollment,
                (
                    "chunk_int8_full.onnx",
                    env!("VOXLOCK_V2_CHUNK_INT8_FULL_SHA256"),
                ),
            ],
            Self::Fp32 => [
                enrollment,
                ("chunk.onnx", env!("VOXLOCK_V2_CHUNK_FP32_SHA256")),
            ],
            Self::Fp16 => [
                ("enroll_fp16.onnx", env!("VOXLOCK_V2_ENROLL_FP16_SHA256")),
                ("chunk_fp16.onnx", env!("VOXLOCK_V2_CHUNK_FP16_SHA256")),
            ],
        }
    }
}

pub fn default_models() -> PathBuf {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("models")));
    sibling
        .filter(|dir| dir.is_dir())
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join(".deps/models/voxlock-v2"))
}

fn verify_model(directory: &Path, name: &str, expected: &str) -> Result<(File, CString)> {
    let path = directory.join(name);
    // Keep the file pinned against modification or replacement until ORT opens it.
    let mut file = OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&path)
        .with_context(|| format!("Open model {}", path.display()))?;
    ensure!(
        sha256(&mut file)? == expected,
        "Model hash mismatch for {}",
        path.display()
    );
    let path = std::path::absolute(path)?;
    Ok((
        file,
        CString::new(path.to_str().context("Model path is not valid Unicode")?)?,
    ))
}

pub fn sha256(mut source: impl Read) -> Result<String> {
    let mut algorithm = BCRYPT_ALG_HANDLE::default();
    unsafe {
        BCryptOpenAlgorithmProvider(
            &mut algorithm,
            w!("SHA256"),
            None,
            BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
        )
        .ok()?
    };
    struct Algorithm(BCRYPT_ALG_HANDLE);
    impl Drop for Algorithm {
        fn drop(&mut self) {
            unsafe {
                let _ = BCryptCloseAlgorithmProvider(self.0, 0);
            }
        }
    }
    let algorithm = Algorithm(algorithm);
    let mut hash = BCRYPT_HASH_HANDLE::default();
    unsafe { BCryptCreateHash(algorithm.0, &mut hash, None, None, 0).ok()? };
    struct Hash(BCRYPT_HASH_HANDLE);
    impl Drop for Hash {
        fn drop(&mut self) {
            unsafe {
                let _ = BCryptDestroyHash(self.0);
            }
        }
    }
    let hash = Hash(hash);
    let mut bytes = [0; 64 * 1024];
    loop {
        let read = source.read(&mut bytes)?;
        if read == 0 {
            break;
        }
        unsafe { BCryptHashData(hash.0, &bytes[..read], 0).ok()? };
    }
    let mut digest = [0; 32];
    unsafe { BCryptFinishHash(hash.0, &mut digest, 0).ok()? };
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub struct VoxLock {
    handle: NonNull<c_void>,
    skip: usize,
    input: usize,
    emitted: usize,
    finished: bool,
    pub create_ms: f64,
    pub enrollment_ms: f64,
}

impl Drop for VoxLock {
    fn drop(&mut self) {
        unsafe { SherpaOnnxDestroyOnlineSpeechDenoiser(self.handle.as_ptr()) }
    }
}

struct Audio(NonNull<DenoisedAudio>);

impl Drop for Audio {
    fn drop(&mut self) {
        unsafe { SherpaOnnxDestroyDenoisedAudio(self.0.as_ptr()) }
    }
}

impl VoxLock {
    pub fn open(
        directory: &Path,
        precision: Precision,
        profile: &[f32],
        threads: i32,
        backend: Backend,
    ) -> Result<Self> {
        Self::open_with_ort_config(directory, precision, profile, threads, backend, None)
    }

    pub fn open_with_ort_config(
        directory: &Path,
        precision: Precision,
        profile: &[f32],
        threads: i32,
        backend: Backend,
        ort_config: Option<&Path>,
    ) -> Result<Self> {
        ensure!((1..=4).contains(&threads), "Invalid inference thread count");
        ensure!(
            (PROFILE_SAMPLES..=30 * RATE).contains(&profile.len())
                && profile.iter().all(|x| x.is_finite() && x.abs() <= 1.0)
                && profile.iter().any(|x| *x != 0.0),
            "Invalid enrollment PCM"
        );
        if backend == Backend::Cuda {
            crate::cuda::ensure_ready()?;
        }
        let [(enroll_name, enroll_hash), (chunk_name, chunk_hash)] = precision.files();
        let (_enroll_file, enroll) = verify_model(directory, enroll_name, enroll_hash)?;
        let (_chunk_file, chunk) = verify_model(directory, chunk_name, chunk_hash)?;
        let provider = if let Some(path) = ort_config {
            let path = path.canonicalize().context("Open ONNX Runtime config")?;
            let path = path
                .to_str()
                .context("ONNX Runtime config path is not valid Unicode")?;
            CString::new(format!("{}:{}", backend.label().to_ascii_lowercase(), path))?
        } else {
            backend.provider().to_owned()
        };
        let config = Config {
            model: ModelConfig {
                gtcrn: ptr::null(),
                num_threads: threads,
                debug: 0,
                provider: provider.as_ptr(),
                dpdfnet: ptr::null(),
                attenuation_limit: 0.0,
                voxlock: VoxLockConfig {
                    enroll_model: enroll.as_ptr(),
                    chunk_model: chunk.as_ptr(),
                    voice_sample: ptr::null(),
                },
            },
        };
        let create = Instant::now();
        let handle = NonNull::new(unsafe { SherpaOnnxCreateOnlineSpeechDenoiser(&config) })
            .context("Could not create VoxLock stream")?;
        let create_ms = create.elapsed().as_secs_f64() * 1000.0;
        let mut model = Self {
            handle,
            skip: 0,
            input: 0,
            emitted: 0,
            finished: false,
            create_ms,
            enrollment_ms: 0.0,
        };
        ensure!(
            unsafe { SherpaOnnxOnlineSpeechDenoiserGetSampleRate(handle.as_ptr()) } == RATE as i32
                && unsafe { SherpaOnnxOnlineSpeechDenoiserGetFrameShiftInSamples(handle.as_ptr()) }
                    == HOP as i32,
            "Unexpected VoxLock sample rate or hop"
        );
        let enrollment = Instant::now();
        ensure!(
            unsafe {
                SherpaOnnxOnlineSpeechDenoiserSetVoiceSample(
                    handle.as_ptr(),
                    profile.as_ptr(),
                    profile.len() as i32,
                    RATE as i32,
                )
            } == 0,
            "VoxLock rejected enrollment"
        );
        model.enrollment_ms = enrollment.elapsed().as_secs_f64() * 1000.0;
        model.assert_enrollment()?;
        Ok(model)
    }

    fn run(&self, samples: &[f32]) -> *const DenoisedAudio {
        unsafe {
            SherpaOnnxOnlineSpeechDenoiserRun(
                self.handle.as_ptr(),
                samples.as_ptr(),
                samples.len() as i32,
                RATE as i32,
            )
        }
    }

    fn assert_enrollment(&mut self) -> Result<()> {
        self.finished = true;
        let probe: [f32; HOP] = std::array::from_fn(|i| if i % 2 == 0 { 0.001 } else { -0.001 });
        if let Some(result) = NonNull::new(self.run(&probe).cast_mut()).map(Audio) {
            ensure!(
                unsafe { result.0.as_ref() }.n == 0,
                "VoxLock emitted audio before speaker lock"
            );
        }
        self.skip = HOP;
        self.input = 0;
        self.emitted = 0;
        self.finished = false;
        Ok(())
    }

    fn copy_output(
        &mut self,
        raw: *const DenoisedAudio,
        output: &mut Vec<f32>,
        padding: usize,
    ) -> Result<()> {
        let Some(result) = NonNull::new(raw.cast_mut()).map(Audio) else {
            return Ok(());
        };
        let data = unsafe { result.0.as_ref() };
        ensure!(
            data.n >= 0 && data.n as usize <= self.input - self.emitted + self.skip + padding,
            "Unexpected VoxLock output size"
        );
        if data.n == 0 {
            return Ok(());
        }
        ensure!(
            data.sample_rate == RATE as i32 && !data.samples.is_null(),
            "Invalid VoxLock output"
        );
        let samples = unsafe { slice::from_raw_parts(data.samples, data.n as usize) };
        ensure!(
            samples.iter().all(|x| x.is_finite()),
            "Nonfinite VoxLock output"
        );
        let discard = self.skip.min(samples.len());
        self.skip -= discard;
        let count = (samples.len() - discard).min(self.input - self.emitted);
        output.extend_from_slice(&samples[discard..discard + count]);
        self.emitted += count;
        Ok(())
    }

    pub fn process(&mut self, samples: &[f32], output: &mut Vec<f32>) -> Result<()> {
        ensure!(!self.finished, "VoxLock stream already finished");
        output.clear();
        ensure!(
            !samples.is_empty()
                && samples.len() <= i32::MAX as usize
                && samples.iter().all(|x| x.is_finite() && x.abs() <= 1.0),
            "Invalid input PCM"
        );
        self.input = self
            .input
            .checked_add(samples.len())
            .context("VoxLock stream sample count overflowed")?;
        self.finished = true;
        self.copy_output(self.run(samples), output, 0)?;
        self.finished = false;
        Ok(())
    }

    pub fn reset(&mut self) -> Result<()> {
        unsafe { SherpaOnnxOnlineSpeechDenoiserReset(self.handle.as_ptr()) };
        self.assert_enrollment()
    }

    pub fn warm_up(&mut self, batch: usize) -> Result<Duration> {
        ensure!(
            (1..=RATE * 80 / 1000).contains(&batch),
            "Invalid warm-up batch"
        );
        let silence = vec![0.0; batch];
        let mut output = Vec::with_capacity(batch + HOP);
        // Exercise initial and steady-state chunk shapes with the selected cadence.
        for _ in 0..(HOP * 4).div_ceil(batch) + 2 {
            self.process(&silence, &mut output)?;
        }
        ensure!(self.emitted > 0, "VoxLock warm-up produced no output");
        let reset = Instant::now();
        self.reset()?;
        Ok(reset.elapsed())
    }

    pub fn finish(&mut self, output: &mut Vec<f32>) -> Result<()> {
        ensure!(!self.finished, "VoxLock stream already finished");
        self.finished = true;
        output.clear();
        if self.input == 0 {
            return Ok(());
        }
        // Ensure overlap-add has started before native flush, which can return
        // the dry buffer on very short streams. Trim all synthetic padding.
        const WINDOW: usize = HOP * 4;
        let padding = WINDOW.saturating_sub(self.input);
        if padding != 0 {
            self.copy_output(self.run(&[0.0; WINDOW][..padding]), output, padding)?;
        }
        let raw = unsafe { SherpaOnnxOnlineSpeechDenoiserFlush(self.handle.as_ptr()) };
        self.copy_output(raw, output, padding)?;
        ensure!(
            self.emitted == self.input && self.skip == 0,
            "VoxLock EOF sample count did not match input"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav;

    #[test]
    fn rejects_missing_or_corrupt_enrollment_and_corrupt_model() -> Result<()> {
        let folder = tempfile::tempdir()?;
        let mut profile = vec![0.0; PROFILE_SAMPLES];
        assert!(VoxLock::open(folder.path(), Precision::Int8, &[], 1, Backend::Cpu).is_err());
        assert!(VoxLock::open(folder.path(), Precision::Int8, &profile, 1, Backend::Cpu).is_err());
        profile[0] = 0.05;
        let error = VoxLock::open(folder.path(), Precision::Int8, &profile, 1, Backend::Cpu)
            .err()
            .unwrap();
        assert!(error.to_string().contains("Open model"));
        std::fs::write(folder.path().join("enroll.onnx"), b"invalid")?;
        let error = VoxLock::open(folder.path(), Precision::Int8, &profile, 1, Backend::Cpu)
            .err()
            .unwrap();
        assert!(error.to_string().contains("hash mismatch"));
        Ok(())
    }

    #[test]
    #[ignore = "requires pinned weights and the downloaded public WAV fixture"]
    fn reset_uses_the_replacement_enrollment() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".deps/models");
        let fixtures = root.join("sherpa-onnx-voxlock-int8/test_wavs");
        let original = wav::read(&fixtures.join("en-voice-sample-f.wav"))?;
        let replacement = wav::read(&fixtures.join("zh-voice-sample-m.wav"))?;
        let input = wav::read(&fixtures.join("en-combined.wav"))?;
        let mut model = VoxLock::open(
            &root.join("voxlock-v2"),
            Precision::Int8,
            &original,
            1,
            Backend::Cpu,
        )?;
        let process = |model: &mut VoxLock| -> Result<Vec<f32>> {
            let mut output = Vec::new();
            let mut block = Vec::new();
            for samples in input[..RATE].chunks(256) {
                model.process(samples, &mut block)?;
                output.extend_from_slice(&block);
            }
            Ok(output)
        };
        let first = process(&mut model)?;
        let set = unsafe {
            SherpaOnnxOnlineSpeechDenoiserSetVoiceSample(
                model.handle.as_ptr(),
                replacement.as_ptr(),
                replacement.len() as i32,
                RATE as i32,
            )
        };
        assert_eq!(set, 0);
        model.assert_enrollment()?;
        let replaced = process(&mut model)?;
        model.reset()?;
        assert_eq!(replaced, process(&mut model)?);
        assert_ne!(first, replaced);
        Ok(())
    }

    #[test]
    #[ignore = "requires pinned weights and the downloaded public WAV fixture"]
    fn public_fixture_streaming_counts_reset_and_short_eof() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(".deps/models");
        let fixtures = root.join("sherpa-onnx-voxlock-int8/test_wavs");
        let profile = wav::read(&fixtures.join("en-voice-sample-f.wav"))?;
        let input = wav::read(&fixtures.join("en-combined.wav"))?;
        let mut model = VoxLock::open(
            &root.join("voxlock-v2"),
            Precision::Int8,
            &profile,
            1,
            Backend::Cpu,
        )?;
        let mut output = Vec::new();
        let mut reference = None;
        for chunk in [160, 256, 320, 640, 1280, 256] {
            model.reset()?;
            let mut audio = Vec::new();
            for samples in input[..RATE + 117].chunks(chunk) {
                model.process(samples, &mut output)?;
                audio.extend_from_slice(&output);
            }
            model.finish(&mut output)?;
            audio.extend_from_slice(&output);
            assert_eq!(audio.len(), RATE + 117);
            assert!(audio.iter().any(|s| s.abs() > 0.01));
            if chunk == 256 {
                if let Some(previous) = &reference {
                    assert_eq!(previous, &audio);
                } else {
                    reference = Some(audio);
                }
            }
        }
        for length in [1, HOP - 1, HOP, HOP + 1, HOP * 4 - 1] {
            model.reset()?;
            model.process(&input[..length], &mut output)?;
            let emitted = output.len();
            model.finish(&mut output)?;
            assert_eq!(emitted + output.len(), length);
        }
        Ok(())
    }
}
