use std::{
    marker::PhantomData,
    ptr,
    rc::Rc,
    slice,
    time::{Duration, Instant},
};

use crate::{PROFILE_SAMPLES, RATE};
use anyhow::{Context, Result, bail, ensure};
use windows::{
    Win32::{
        Devices::FunctionDiscovery::{PKEY_Device_FriendlyName, PKEY_DeviceInterface_FriendlyName},
        Foundation::{
            CloseHandle, ERROR_NOT_FOUND, HANDLE, PROPERTYKEY, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Media::{
            Audio::*,
            Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT},
        },
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
                CoTaskMemFree, CoUninitialize, STGM_READ,
                StructuredStorage::{PropVariantClear, PropVariantToStringAlloc},
            },
            Threading::{
                AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
                SetEvent, WaitForMultipleObjects,
            },
        },
    },
    core::{HSTRING, Interface, PCWSTR, PWSTR, w},
};

pub struct Event(HANDLE);

// Windows events can be signaled and waited on concurrently. The owned handle
// stays open until the last scoped worker borrowing it has exited.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    pub fn new(manual_reset: bool) -> Result<Self> {
        Ok(Self(unsafe {
            CreateEventW(None, manual_reset, false, PCWSTR::null())?
        }))
    }

    pub fn signal(&self) {
        unsafe {
            let _ = SetEvent(self.0);
        }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub fn wait<const N: usize>(events: [&Event; N], ms: u32) -> Result<Option<usize>> {
    let handles = events.map(|event| event.0);
    let wake = unsafe { WaitForMultipleObjects(&handles, false, ms) };
    if wake == WAIT_TIMEOUT {
        return Ok(None);
    }
    let index = wake.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
    ensure!(index < N, "Windows audio wait failed");
    Ok(Some(index))
}

pub struct Com(PhantomData<Rc<()>>);

impl Com {
    pub fn init() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(Self(PhantomData))
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}

pub struct AudioPriority(Option<HANDLE>);

impl AudioPriority {
    pub fn register() -> Self {
        Self::register_task(w!("Pro Audio"))
    }

    pub fn register_processing() -> Self {
        // Keep capture and render in the higher-priority Pro Audio task.
        Self::register_task(w!("Audio"))
    }

    fn register_task(task: PCWSTR) -> Self {
        Self(unsafe { AvSetMmThreadCharacteristicsW(task, &mut 0) }.ok())
    }
}

impl Drop for AudioPriority {
    fn drop(&mut self) {
        if let Some(handle) = self.0 {
            unsafe {
                let _ = AvRevertMmThreadCharacteristics(handle);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub standard_cable: bool,
}

pub struct Devices {
    pub microphones: Vec<Device>,
    pub cables: Vec<Device>,
    pub default_mic: Option<String>,
}

pub fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER) }
        .context("Open Windows audio devices")
}

fn take_string(value: PWSTR) -> Result<String> {
    ensure!(
        !value.is_null(),
        "Windows returned an empty endpoint property"
    );
    unsafe {
        let result = value.to_string();
        CoTaskMemFree(Some(value.0.cast()));
        Ok(result?)
    }
}

fn property(device: &IMMDevice, key: &PROPERTYKEY) -> Result<String> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ)?;
        let mut value = store.GetValue(key)?;
        let text = PropVariantToStringAlloc(&value);
        let _ = PropVariantClear(&mut value);
        take_string(text?)
    }
}

fn names(device: &IMMDevice) -> Result<(String, bool, bool)> {
    let name = property(device, &PKEY_Device_FriendlyName)?;
    let interface = property(device, &PKEY_DeviceInterface_FriendlyName).unwrap_or_default();
    let description = format!("{name} {interface}").to_lowercase();
    let vb = description.contains("vb-audio virtual cable");
    Ok((name, vb || description.contains("deckx audio cable"), vb))
}

fn list(
    enumerator: &IMMDeviceEnumerator,
    direction: EDataFlow,
    cable: bool,
) -> Result<Vec<Device>> {
    let endpoints = unsafe { enumerator.EnumAudioEndpoints(direction, DEVICE_STATE_ACTIVE)? };
    let mut devices = Vec::new();
    for index in 0..unsafe { endpoints.GetCount()? } {
        let Ok(endpoint) = (unsafe { endpoints.Item(index) }) else {
            continue;
        };
        let Ok((name, is_cable, vb)) = names(&endpoint) else {
            continue;
        };
        if cable != is_cable {
            continue;
        }
        let Ok(id) = unsafe { endpoint.GetId() }
            .map_err(anyhow::Error::from)
            .and_then(take_string)
        else {
            continue;
        };
        let lower = name.to_lowercase();
        let standard_cable = direction == eRender
            && vb
            && (lower == "cable input" || lower.starts_with("cable input ("));
        devices.push(Device {
            id,
            name,
            standard_cable,
        });
    }
    Ok(devices)
}

pub fn enumerate() -> Result<Devices> {
    let _com = Com::init()?;
    let enumerator = enumerator()?;
    let microphones = list(&enumerator, eCapture, false)?;
    let cables = list(&enumerator, eRender, true)?;
    let default_mic = [eCommunications, eConsole]
        .into_iter()
        .find_map(|role| unsafe {
            let endpoint = enumerator.GetDefaultAudioEndpoint(eCapture, role).ok()?;
            let id = take_string(endpoint.GetId().ok()?).ok()?;
            microphones.iter().any(|mic| mic.id == id).then_some(id)
        });
    Ok(Devices {
        microphones,
        cables,
        default_mic,
    })
}

pub(crate) fn device_unavailable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<windows::core::Error>()
        .is_some_and(|error| {
            matches!(
                error.code(),
                AUDCLNT_E_DEVICE_INVALIDATED
                    | AUDCLNT_E_RESOURCES_INVALIDATED
                    | AUDCLNT_E_SERVICE_NOT_RUNNING
            ) || error.code() == ERROR_NOT_FOUND.to_hresult()
        })
}

pub fn cable_recording_id(devices: &IMMDeviceEnumerator, selected: &str) -> Result<String> {
    let endpoint = unsafe { devices.GetDevice(&HSTRING::from(selected))? };
    let (name, _, vb) = names(&endpoint)?;
    let lower = name.to_lowercase();
    ensure!(
        vb && lower.contains(" input"),
        "Latency measurement needs VB-CABLE with its standard endpoint names."
    );
    let expected = lower.replacen(" input", " output", 1);
    let mut matches = list(devices, eCapture, true)?
        .into_iter()
        .filter(|device| device.name.to_lowercase() == expected);
    let selected = matches
        .next()
        .context("The matching CABLE Output recording endpoint is unavailable.")?;
    ensure!(
        matches.next().is_none(),
        "Multiple matching cable recording endpoints were found."
    );
    Ok(selected.id)
}

pub struct Packet<'a> {
    pub samples: Option<&'a [f32]>,
    pub frames: usize,
    pub tick: Option<u64>,
    pub gap: bool,
}

pub struct Stream {
    client: IAudioClient,
    capture: Option<IAudioCaptureClient>,
    render: Option<IAudioRenderClient>,
    pub event: Event,
    pub capacity: usize,
    short_render: Option<ShortRender>,
    started: bool,
    seen: bool,
}

#[derive(Clone, Copy)]
struct ShortRender {
    rate: usize,
    channels: usize,
    period: usize,
}

struct MixFormat(*mut WAVEFORMATEX);

impl Drop for MixFormat {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

fn short_period(rate: u32, fundamental: u32, min: u32, max: u32) -> Option<u32> {
    if fundamental == 0 || min == 0 || min > max {
        return None;
    }
    let smallest = min.div_ceil(fundamental);
    let largest = max / fundamental;
    if smallest > largest {
        return None;
    }
    let target = (rate / 200).div_ceil(fundamental);
    Some(target.clamp(smallest, largest) * fundamental)
}

fn shorter_than_legacy(period: u32, rate: u32, legacy_hns: i64) -> bool {
    i64::from(period) * 10_000_000 < legacy_hns * i64::from(rate)
}

fn open_short_render(device: &IMMDevice) -> Result<(IAudioClient, ShortRender)> {
    const EXTENSIBLE: u16 = 0xfffe;
    unsafe {
        let client: IAudioClient3 = device.Activate(CLSCTX_INPROC_SERVER, None)?;
        let format = MixFormat(client.GetMixFormat()?);
        ensure!(!format.0.is_null(), "Missing cable mix format");
        let spec = format.0.read();
        let rate = spec.nSamplesPerSec;
        let channels = spec.nChannels;
        ensure!(
            (RATE as u32..=192_000).contains(&rate)
                && matches!(channels, 1 | 2)
                && spec.wBitsPerSample == 32
                && spec.nBlockAlign == channels * 4,
            "Unsupported cable mix format"
        );
        let float = spec.wFormatTag == WAVE_FORMAT_IEEE_FLOAT as u16
            || (spec.wFormatTag == EXTENSIBLE
                && spec.cbSize as usize
                    >= size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()
                && {
                    let extended = format.0.cast::<WAVEFORMATEXTENSIBLE>().read();
                    let subtype = extended.SubFormat;
                    subtype == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
                        && extended.Samples.wValidBitsPerSample == 32
                });
        ensure!(float, "Cable mix format is not float PCM");
        let (mut fundamental, mut min, mut max) = (0, 0, 0);
        client.GetSharedModeEnginePeriod(format.0, &mut 0, &mut fundamental, &mut min, &mut max)?;
        let period = short_period(rate, fundamental, min, max).context("No cable audio period")?;
        let mut legacy = 0;
        client.GetDevicePeriod(Some(&mut legacy), None)?;
        ensure!(
            (1..=10_000_000).contains(&legacy) && shorter_than_legacy(period, rate, legacy),
            "Cable does not offer a shorter render period"
        );
        client.InitializeSharedAudioStream(
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            period,
            format.0,
            None,
        )?;
        let mut actual_format = MixFormat(ptr::null_mut());
        let mut actual = 0;
        client.GetCurrentSharedModeEnginePeriod(&mut actual_format.0, &mut actual)?;
        ensure!(
            !actual_format.0.is_null()
                && (*actual_format.0).nSamplesPerSec == rate
                && (min..=max).contains(&actual)
                && actual.is_multiple_of(fundamental)
                && shorter_than_legacy(actual, rate, legacy),
            "Windows did not open a shorter cable period"
        );
        Ok((
            client.cast()?,
            ShortRender {
                rate: rate as usize,
                channels: channels as usize,
                period: actual as usize,
            },
        ))
    }
}

impl Stream {
    pub fn open(
        devices: &IMMDeviceEnumerator,
        id: &str,
        direction: EDataFlow,
        cable: bool,
    ) -> Result<Self> {
        Self::open_endpoint(devices, id, direction, cable, false)
    }

    pub(crate) fn open_render(
        devices: &IMMDeviceEnumerator,
        id: &str,
        prefer_short_period: bool,
    ) -> Result<Self> {
        Self::open_endpoint(devices, id, eRender, true, prefer_short_period)
    }

    fn open_endpoint(
        devices: &IMMDeviceEnumerator,
        id: &str,
        direction: EDataFlow,
        cable: bool,
        prefer_short_period: bool,
    ) -> Result<Self> {
        unsafe {
            let device = devices
                .GetDevice(&HSTRING::from(id))
                .context("Find selected audio endpoint")?;
            let endpoint: IMMEndpoint = device.cast()?;
            let (_, is_cable, vb_cable) = names(&device)?;
            ensure!(
                endpoint.GetDataFlow()? == direction && is_cable == cable,
                "Choose a microphone and a recognized virtual cable"
            );
            if prefer_short_period
                && vb_cable
                && let Ok((client, spec)) = open_short_render(&device)
                && let Ok(stream) = Self::initialized(client, direction, Some(spec))
            {
                return Ok(stream);
            }
            // A failed or partially initialized client cannot be reused.
            let client: IAudioClient = device.Activate(CLSCTX_INPROC_SERVER, None)?;
            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
                nChannels: 1,
                nSamplesPerSec: RATE as u32,
                nAvgBytesPerSec: (RATE * 4) as u32,
                nBlockAlign: 4,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_NOPERSIST
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            client
                .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, &format, None)
                .context("Open Windows audio in shared mode")?;
            Self::initialized(client, direction, None)
        }
    }

    fn initialized(
        client: IAudioClient,
        direction: EDataFlow,
        short_render: Option<ShortRender>,
    ) -> Result<Self> {
        let event = Event::new(false)?;
        unsafe {
            client.SetEventHandle(event.0)?;
            let (capture, render) = if direction == eCapture {
                (Some(client.GetService()?), None)
            } else {
                (None, Some(client.GetService()?))
            };
            let capacity = client.GetBufferSize()? as usize;
            Ok(Self {
                client,
                capture,
                render,
                event,
                capacity,
                short_render,
                started: false,
                seen: false,
            })
        }
    }

    pub fn period(&self) -> Result<usize> {
        if let Some(spec) = self.short_render {
            return Ok(spec.period);
        }
        let mut period = 0;
        unsafe {
            self.client.GetDevicePeriod(Some(&mut period), None)?;
        }
        ensure!(
            (1..=10_000_000).contains(&period),
            "Invalid Windows audio period"
        );
        Ok((period as usize * RATE).div_ceil(10_000_000))
    }

    pub(crate) fn short_render_rate(&self) -> Option<usize> {
        self.short_render.map(|spec| spec.rate)
    }

    pub fn start(&mut self) -> Result<()> {
        unsafe {
            self.client.Start().context("Start audio endpoint")?;
        }
        self.started = true;
        Ok(())
    }

    pub fn capture(&mut self, mut consume: impl FnMut(Packet<'_>) -> Result<()>) -> Result<bool> {
        let capture = self
            .capture
            .as_ref()
            .context("Endpoint does not capture audio")?;
        let mut received = false;
        unsafe {
            while capture.GetNextPacketSize()? > 0 {
                let (mut data, mut frames, mut flags, mut tick) = (ptr::null_mut(), 0, 0, 0);
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut tick))?;
                if frames == 0 {
                    break;
                }
                let gap = self.seen && flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0;
                self.seen = true;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let samples = (!silent && !data.is_null())
                    .then(|| slice::from_raw_parts(data.cast::<f32>(), frames as usize));
                let tick = (tick != 0 && flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0)
                    .then_some(tick);
                let result = consume(Packet {
                    samples,
                    frames: frames as usize,
                    tick,
                    gap,
                });
                capture.ReleaseBuffer(frames)?;
                result?;
                received = true;
            }
        }
        Ok(received)
    }

    pub fn render(&self, target: usize, produce: impl FnOnce(&mut [f32]) -> bool) -> Result<usize> {
        let render = self
            .render
            .as_ref()
            .context("Endpoint does not render audio")?;
        unsafe {
            let padding = self.client.GetCurrentPadding()? as usize;
            let frames = target.min(self.capacity).saturating_sub(padding);
            if frames == 0 {
                return Ok(padding);
            }
            let ptr = render.GetBuffer(frames as u32)?;
            let channels = self.short_render.map_or(1, |spec| spec.channels);
            ptr.write_bytes(0, frames * channels * size_of::<f32>());
            let samples = slice::from_raw_parts_mut(ptr.cast::<f32>(), frames * channels);
            let flags = if produce(&mut samples[..frames]) {
                if channels == 2 {
                    for frame in (0..frames).rev() {
                        let sample = samples[frame];
                        samples[2 * frame..2 * frame + 2].fill(sample);
                    }
                }
                0
            } else {
                AUDCLNT_BUFFERFLAGS_SILENT.0 as u32
            };
            render.ReleaseBuffer(frames as u32, flags)?;
            Ok(padding)
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if self.started {
            unsafe {
                let _ = self.client.Stop();
            }
        }
    }
}

pub fn peak(samples: &[f32]) -> f32 {
    samples
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()))
}

pub fn select_enrollment(recording: &[f32]) -> Result<Vec<f32>> {
    ensure!(
        recording.len() >= PROFILE_SAMPLES,
        "Voice sample was too short"
    );
    let bin = RATE / 50;
    let rms: Vec<_> = recording
        .chunks_exact(bin)
        .map(|chunk| (chunk.iter().map(|x| x * x).sum::<f32>() / bin as f32).sqrt())
        .collect();
    let bins = PROFILE_SAMPLES / bin;
    let (start, activity) = rms
        .windows(bins)
        .enumerate()
        .map(|(start, bins)| (start, bins.iter().filter(|&&x| x >= 0.006).count()))
        .max_by_key(|&(_, active)| active)
        .context("Voice sample was too short")?;
    ensure!(
        activity >= 100,
        "Speak for more of the recording and move closer to your microphone."
    );
    let selection = &recording[start * bin..start * bin + PROFILE_SAMPLES];
    ensure!(
        selection.iter().all(|x| x.is_finite())
            && selection.iter().filter(|x| x.abs() >= 0.995).count() < PROFILE_SAMPLES / 100,
        "Voice sample is clipping. Lower microphone gain and try again."
    );
    Ok(selection.to_vec())
}

pub fn record(id: &str, stop: &Event, mut level: impl FnMut(f32)) -> Result<Vec<f32>> {
    let _com = Com::init()?;
    let mut stream = Stream::open(&enumerator()?, id, eCapture, false)?;
    let _priority = AudioPriority::register();
    let mut recording = Vec::with_capacity(RATE * 6);
    stream.start()?;
    let started = Instant::now();
    while recording.len() < RATE * 6 {
        ensure!(
            started.elapsed() < Duration::from_secs(9),
            "Recording timed out."
        );
        match wait([stop, &stream.event], 1500)? {
            Some(0) => bail!("Recording canceled"),
            Some(1) => {
                stream.capture(|packet| {
                    ensure!(!packet.gap, "The microphone stream was interrupted.");
                    let count = packet.frames.min(RATE * 6 - recording.len());
                    if let Some(samples) = packet.samples {
                        let samples = &samples[..count];
                        level(peak(samples));
                        recording.extend_from_slice(samples);
                    } else {
                        level(0.0);
                        recording.resize(recording.len() + count, 0.0);
                    }
                    Ok(())
                })?;
            }
            _ => bail!("The microphone stopped responding."),
        }
    }
    select_enrollment(&recording)
}

#[cfg(test)]
mod short_period_tests {
    use super::*;
    #[test]
    fn short_render_period_respects_device_limits_and_improves_on_legacy() {
        assert_eq!(short_period(48_000, 1, 128, 480), Some(240));
        assert_eq!(short_period(48_000, 32, 128, 480), Some(256));
        assert_eq!(short_period(44_100, 16, 128, 200), Some(192));
        assert_eq!(short_period(48_000, 160, 320, 480), Some(320));
        assert_eq!(short_period(48_000, 32, 241, 250), None);
        assert_eq!(short_period(48_000, 0, 128, 480), None);
        assert!(shorter_than_legacy(240, 48_000, 100_000));
        assert!(!shorter_than_legacy(480, 48_000, 100_000));
    }
}
