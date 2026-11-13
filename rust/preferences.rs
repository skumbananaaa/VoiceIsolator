use crate::{
    PROFILE_SAMPLES,
    engine::{Gains, Processing},
    wasapi::Devices,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    slice,
};
use windows::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, HLOCAL, LocalFree},
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
        System::Registry::{
            HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegDeleteKeyValueW,
            RegGetValueW, RegSetKeyValueW,
        },
    },
    core::{PCWSTR, w},
};

const SETTINGS: PCWSTR = w!("Software\\VoiceIsolator");
const STARTUP: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const STARTUP_NAME: PCWSTR = w!("VoiceIsolator");
const MAX_STARTUP_COMMAND: usize = 260;
const MAX_ID: usize = 1023;

pub struct Profile {
    pub microphone: String,
    pub samples: Vec<f32>,
}

fn clear<T: Copy + Default>(values: &mut [T]) {
    for value in values {
        unsafe {
            std::ptr::write_volatile(value, T::default());
        }
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        clear(&mut self.samples);
    }
}

struct Blob(CRYPT_INTEGER_BLOB);

impl Blob {
    fn data(&self) -> &[u8] {
        if self.0.cbData == 0 {
            return &[];
        }
        unsafe { slice::from_raw_parts(self.0.pbData, self.0.cbData as usize) }
    }
}

impl Drop for Blob {
    fn drop(&mut self) {
        if !self.0.pbData.is_null() {
            unsafe {
                clear(slice::from_raw_parts_mut(
                    self.0.pbData,
                    self.0.cbData as usize,
                ));
                let _ = LocalFree(Some(HLOCAL(self.0.pbData.cast())));
            }
        }
    }
}

pub fn profile_path() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("LOCALAPPDATA").context("Could not locate your local app data folder")?,
    )
    .join("VoiceIsolator/voice-profile.dat"))
}

pub const MAX_PROFILE_NAME: usize = 40;
pub const SAVED_VOICE: &str = "Saved voice";

fn profile_name_valid(name: &str) -> bool {
    !name.is_empty()
        && name == name.trim()
        && name.encode_utf16().count() <= MAX_PROFILE_NAME
        && !name.ends_with('.')
        && !name
            .chars()
            .any(|c| c.is_control() || r#"<>:"/\|?*"#.contains(c))
        && !name.eq_ignore_ascii_case(SAVED_VOICE)
}

pub fn named_profile_path(legacy: &Path, name: Option<&str>) -> Result<PathBuf> {
    let Some(name) = name else {
        return Ok(legacy.to_owned());
    };
    ensure!(
        profile_name_valid(name),
        "Use a name of 1–40 characters without Windows filename punctuation. \"Saved voice\" is reserved."
    );
    Ok(legacy
        .with_file_name("profiles")
        .join(format!("voice-{name}.dat")))
}

pub fn voice_profiles(legacy: &Path) -> Result<Vec<Option<String>>> {
    let mut profiles = Vec::new();
    if legacy.try_exists()? {
        profiles.push(None);
    }
    let directory = legacy.with_file_name("profiles");
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(profiles),
        Err(error) => return Err(error.into()),
    };
    let mut named = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && let Some(filename) = entry.file_name().to_str()
            && let Some(name) = filename
                .strip_prefix("voice-")
                .and_then(|s| s.strip_suffix(".dat"))
            && profile_name_valid(name)
        {
            named.push(Some(name.to_owned()));
        }
    }
    named.sort_unstable();
    profiles.extend(named);
    Ok(profiles)
}

fn crypt(bytes: &mut [u8], encrypt: bool) -> Result<Blob> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len().try_into()?,
        pbData: bytes.as_mut_ptr(),
    };
    let mut output = Blob(CRYPT_INTEGER_BLOB::default());
    unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                w!("VoiceIsolator enrollment"),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )?;
        } else {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )?;
        }
    }
    Ok(output)
}

fn valid(profile: &Profile) -> bool {
    let chars = profile.microphone.encode_utf16().count();
    (1..=MAX_ID).contains(&chars)
        && !profile.microphone.contains('\0')
        && profile.samples.len() == PROFILE_SAMPLES
        && profile
            .samples
            .iter()
            .all(|x| x.is_finite() && x.abs() <= 1.0)
        && profile.samples.iter().any(|x| *x != 0.0)
}

fn decode(bytes: &[u8]) -> Result<Profile> {
    ensure!(bytes.len() >= 8, "Saved voice profile is incomplete");
    let version = u32::from_le_bytes(bytes[..4].try_into()?);
    let chars = u32::from_le_bytes(bytes[4..8].try_into()?) as usize;
    ensure!(
        version == 1
            && (1..=MAX_ID).contains(&chars)
            && bytes.len() == 8 + chars * 2 + PROFILE_SAMPLES * 4,
        "Saved voice profile format is unsupported"
    );
    let microphone = bytes[8..8 + chars * 2]
        .chunks_exact(2)
        .map(|x| u16::from_le_bytes([x[0], x[1]]))
        .collect::<Vec<_>>();
    let profile = Profile {
        microphone: String::from_utf16(&microphone)?,
        samples: bytes[8 + chars * 2..]
            .chunks_exact(4)
            .map(|x| f32::from_le_bytes(x.try_into().unwrap()))
            .collect(),
    };
    ensure!(
        valid(&profile),
        "Saved voice profile contains invalid audio or endpoint data"
    );
    Ok(profile)
}

pub fn load_profile(path: &Path) -> Result<Option<Profile>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(262_145).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 262_144, "Saved voice profile is too large");
    let plain =
        crypt(&mut bytes, false).context("Windows could not decrypt the saved voice profile")?;
    Ok(Some(decode(plain.data())?))
}

pub fn save_profile(path: &Path, profile: &Profile) -> Result<()> {
    write_profile(path, profile, false)
}

pub fn create_profile(path: &Path, profile: &Profile) -> Result<()> {
    write_profile(path, profile, true)
}

fn write_profile(path: &Path, profile: &Profile, create: bool) -> Result<()> {
    ensure!(
        valid(profile),
        "The recording cannot be saved as a voice profile"
    );
    let chars = profile.microphone.encode_utf16().count();
    let mut plain = Vec::with_capacity(8 + chars * 2 + PROFILE_SAMPLES * 4);
    plain.extend_from_slice(&1_u32.to_le_bytes());
    plain.extend_from_slice(&(chars as u32).to_le_bytes());
    for unit in profile.microphone.encode_utf16() {
        plain.extend_from_slice(&unit.to_le_bytes());
    }
    for sample in &profile.samples {
        plain.extend_from_slice(&sample.to_le_bytes());
    }
    let encrypted = crypt(&mut plain, true);
    clear(&mut plain);
    let encrypted = encrypted.context("Windows could not encrypt the voice profile")?;
    let directory = path.parent().context("Invalid profile location")?;
    fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(encrypted.data())?;
    temporary.as_file().sync_all()?;
    let temporary = temporary.into_temp_path();
    if create {
        temporary
            .persist_noclobber(path)
            .context("Use a name that is not already saved")?;
    } else {
        temporary.persist(path)?;
    }
    Ok(())
}

pub fn delete_profile(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choices {
    pub microphone: String,
    pub cable: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub choices: Choices,
    #[serde(default)]
    pub voice_profile: Option<String>,
    pub processing: Processing,
    pub gains: Gains,
    pub dark: bool,
    pub resume: bool,
    #[serde(default = "enabled")]
    pub auto_updates: bool,
    #[serde(default)]
    pub last_update_check: u64,
}

fn enabled() -> bool {
    true
}

impl Settings {
    pub fn load() -> Result<Self> {
        let path = profile_path()?.with_file_name("settings.json");
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err.into()),
        };
        let settings: Self = serde_json::from_reader(file.take(16_385))?;
        ensure!(
            settings.processing.valid() && settings.gains.valid(),
            "Invalid saved processing settings"
        );
        Ok(settings)
    }

    pub fn save(&self) -> Result<()> {
        let path = profile_path()?.with_file_name("settings.json");
        let directory = path.parent().context("Invalid settings location")?;
        fs::create_dir_all(directory)?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(&mut temporary, self)?;
        temporary.as_file().sync_all()?;
        temporary.into_temp_path().persist(path)?;
        Ok(())
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            choices: Choices::load(),
            voice_profile: None,
            processing: Processing::default(),
            gains: Gains::default(),
            dark: dark_mode().unwrap_or(true),
            resume: false,
            auto_updates: true,
            last_update_check: 0,
        }
    }
}

pub fn startup_enabled() -> bool {
    let Ok(expected) = startup_command() else {
        return false;
    };
    let mut command = [0_u16; MAX_STARTUP_COMMAND];
    let mut bytes = size_of_val(&command) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            STARTUP,
            STARTUP_NAME,
            RRF_RT_REG_SZ,
            None,
            Some(command.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    status.is_ok()
        && (bytes as usize == size_of_val(expected.as_slice()))
        && String::from_utf16_lossy(&command[..expected.len()])
            .eq_ignore_ascii_case(&String::from_utf16_lossy(&expected))
}

fn startup_command() -> Result<Vec<u16>> {
    let exe = std::env::current_exe()?;
    let mut command = vec![b'"' as u16];
    command.extend(exe.as_os_str().encode_wide());
    command.extend("\" --startup\0".encode_utf16());
    ensure!(
        command.len() <= MAX_STARTUP_COMMAND,
        "The installed path is too long for Windows startup"
    );
    Ok(command)
}

pub fn set_startup(enabled: bool) -> Result<()> {
    if !enabled {
        let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, STARTUP, STARTUP_NAME) };
        if status != ERROR_FILE_NOT_FOUND {
            status.ok()?;
        }
        return Ok(());
    }
    let command = startup_command()?;
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            STARTUP,
            STARTUP_NAME,
            REG_SZ.0,
            Some(command.as_ptr().cast()),
            size_of_val(command.as_slice()) as u32,
        )
        .ok()?;
    }
    Ok(())
}

fn endpoint_key(cable: bool) -> PCWSTR {
    if cable {
        w!("CableEndpoint")
    } else {
        w!("MicrophoneEndpoint")
    }
}

fn read_endpoint(cable: bool) -> String {
    let mut buffer = [0_u16; MAX_ID + 1];
    let mut bytes = size_of_val(&buffer) as u32;
    if unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            SETTINGS,
            endpoint_key(cable),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
        .ok()
    }
    .is_err()
    {
        return String::new();
    }
    let end = buffer.iter().position(|&x| x == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

impl Choices {
    pub fn load() -> Self {
        Self {
            microphone: read_endpoint(false),
            cable: read_endpoint(true),
        }
    }

    pub fn resolve(&self, devices: &Devices, profile_id: Option<&str>) -> Self {
        let microphone = [
            Some(self.microphone.as_str()),
            profile_id,
            devices.default_mic.as_deref(),
        ]
        .into_iter()
        .flatten()
        .find(|id| devices.microphones.iter().any(|d| d.id == *id))
        .or_else(|| (devices.microphones.len() == 1).then(|| devices.microphones[0].id.as_str()))
        .unwrap_or_default()
        .to_owned();
        let cable = if devices.cables.iter().any(|d| d.id == self.cable) {
            self.cable.clone()
        } else if devices.cables.len() == 1 {
            devices.cables[0].id.clone()
        } else {
            let mut standard = devices.cables.iter().filter(|d| d.standard_cable);
            standard
                .next()
                .filter(|_| standard.next().is_none())
                .map(|d| d.id.clone())
                .unwrap_or_default()
        };
        Self { microphone, cable }
    }
}

pub fn dark_mode() -> Option<bool> {
    let mut dark = 0_u32;
    let mut size = size_of::<u32>() as u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            SETTINGS,
            w!("DarkMode"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut dark as *mut u32).cast()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    Some(dark != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasapi::Device;

    #[test]
    fn voice_profiles_keep_legacy_recording_and_refuse_to_overwrite_existing_names() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let legacy = directory.path().join("voice-profile.dat");
        let recording = |sample| Profile {
            microphone: "mic".into(),
            samples: vec![sample; PROFILE_SAMPLES],
        };
        save_profile(&legacy, &recording(0.1))?;
        let original = fs::read(&legacy)?;
        let test = named_profile_path(&legacy, Some("Test voice"))?;
        create_profile(&test, &recording(0.2))?;
        assert!(create_profile(&test, &recording(0.3)).is_err());
        assert_eq!(load_profile(&test)?.unwrap().samples[0], 0.2);
        assert_eq!(
            voice_profiles(&legacy)?,
            vec![None, Some("Test voice".into())]
        );
        assert!(named_profile_path(&legacy, Some("../other")).is_err());
        assert!(named_profile_path(&legacy, Some("saved voice")).is_err());
        delete_profile(&test)?;
        assert_eq!(voice_profiles(&legacy)?, vec![None]);
        assert_eq!(fs::read(&legacy)?, original);
        assert_eq!(load_profile(&legacy)?.unwrap().samples[0], 0.1);
        Ok(())
    }

    #[test]
    fn encrypted_profile_round_trip_and_replacement() -> Result<()> {
        let temp = tempfile::tempdir_in(std::env::current_dir()?)?;
        let path = temp.path().join("Röst/voice-profile.dat");
        let mut profile = Profile {
            microphone: "Mikrofon 🎙".to_owned(),
            samples: vec![0.1; PROFILE_SAMPLES],
        };
        assert!(load_profile(&path)?.is_none());
        save_profile(&path, &profile).context("initial profile save")?;
        let loaded = load_profile(&path)?.unwrap();
        assert_eq!(loaded.microphone, profile.microphone);
        assert_eq!(loaded.samples, profile.samples);
        profile.samples[0] = f32::NAN;
        assert!(save_profile(&path, &profile).is_err());
        assert_eq!(load_profile(&path)?.unwrap().samples[0], 0.1);
        profile.samples[0] = -0.2;
        save_profile(&path, &profile).context("replace existing profile")?;
        assert_eq!(load_profile(&path)?.unwrap().samples[0], -0.2);
        delete_profile(&path)?;
        assert!(load_profile(&path)?.is_none());
        Ok(())
    }

    #[test]
    fn picks_preferred_devices_and_only_unambiguous_cable() {
        let device = |id: &str, standard_cable| Device {
            id: id.to_owned(),
            name: id.to_owned(),
            standard_cable,
        };
        let devices = Devices {
            microphones: vec![device("default", false), device("enrolled", false)],
            cables: vec![device("main", true), device("second", false)],
            default_mic: Some("default".to_owned()),
        };
        let choice = Choices::default().resolve(&devices, Some("enrolled"));
        assert_eq!(choice.microphone, "enrolled");
        assert_eq!(choice.cable, "main");
        let choice = Choices {
            microphone: "default".to_owned(),
            cable: "second".to_owned(),
        }
        .resolve(&devices, Some("enrolled"));
        assert_eq!(choice.microphone, "default");
        assert_eq!(choice.cable, "second");
    }
}
