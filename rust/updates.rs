use anyhow::{Context, Result, ensure};
use minisign_verify::{PublicKey, Signature};
use serde::Deserialize;
use std::{
    ffi::c_void,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::windows::fs::OpenOptionsExt,
    process::{self, Command},
    ptr,
    sync::atomic::{AtomicBool, Ordering::Relaxed},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tempfile::TempPath;
use voiceisolator::{isolation::sha256, preferences::profile_path};
use windows::{
    Win32::Networking::WinHttp::*,
    core::{PCWSTR, w},
};

const REPO: &str = "/skumbananaaa/VoiceIsolator";
const INSTALLER: &str = "VoiceIsolator-Setup-x64.exe";
const MANIFEST: &str = "VoiceIsolator-release.json";
const SIGNATURE: &str = "VoiceIsolator-release.json.minisig";
const MAX_INSTALLER: u64 = 256 * 1024 * 1024;

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub version: String,
    installer: String,
    size: u64,
    sha256: String,
}

fn version(text: &str) -> Option<[u16; 3]> {
    let mut fields = text.split('.');
    let mut result = [0; 3];
    for number in &mut result {
        let field = fields.next()?;
        if !field.bytes().all(|byte| byte.is_ascii_digit())
            || (field.len() > 1 && field.starts_with('0'))
        {
            return None;
        }
        *number = field.parse().ok()?;
    }
    fields.next().is_none().then_some(result)
}

pub fn configured() -> bool {
    option_env!("VOICEISOLATOR_UPDATE_PUBLIC_KEY")
        .is_some_and(|key| PublicKey::from_base64(key).is_ok())
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn verify_manifest(key: &PublicKey, tag: &str, body: &[u8], signature: &str) -> Result<Release> {
    let signature = Signature::decode(signature).context("Invalid release manifest signature")?;
    key.verify(body, &signature, false)
        .context("Release manifest signature verification failed")?;
    let release: Release = serde_json::from_slice(body)?;
    ensure!(
        tag.strip_prefix('v') == Some(release.version.as_str())
            && version(&release.version).is_some()
            && release.installer == INSTALLER
            && (1..=MAX_INSTALLER).contains(&release.size)
            && release.sha256.len() == 64
            && release
                .sha256
                .bytes()
                .all(|x| x.is_ascii_digit() || (b'a'..=b'f').contains(&x)),
        "The signed manifest does not describe the expected release installer"
    );
    Ok(release)
}

pub fn check() -> Result<Option<Release>> {
    let key = option_env!("VOICEISOLATOR_UPDATE_PUBLIC_KEY")
        .context("This build has no update verification key")?;
    let key = PublicKey::from_base64(key).context("Invalid update verification key")?;
    let path = format!("/repos{REPO}/releases/latest");
    let release: GitHubRelease =
        serde_json::from_slice(&get(w!("api.github.com"), &path, 1024 * 1024)?)?;
    let Some(latest) = release.tag_name.strip_prefix('v').and_then(version) else {
        return Ok(None);
    };
    let current = version(env!("CARGO_PKG_VERSION")).context("Invalid app version")?;
    if release.draft || release.prerelease || latest <= current {
        return Ok(None);
    }
    let manifest = get_asset(&release.tag_name, MANIFEST, 16 * 1024)?;
    let signature = get_asset(&release.tag_name, SIGNATURE, 4 * 1024)?;
    Ok(Some(verify_manifest(
        &key,
        &release.tag_name,
        &manifest,
        std::str::from_utf8(&signature)?,
    )?))
}

fn asset_path(tag: &str, asset: &str) -> String {
    format!("{REPO}/releases/download/{tag}/{asset}")
}

fn get_asset(tag: &str, asset: &str, limit: u64) -> Result<Vec<u8>> {
    get(w!("github.com"), &asset_path(tag, asset), limit)
}

fn get(host: PCWSTR, path: &str, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    HttpBody::open(host, path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "GitHub response exceeds its size limit"
    );
    Ok(bytes)
}

pub struct Setup {
    locked: File,
    path: TempPath,
}

impl Setup {
    pub fn launch(self) -> Result<()> {
        let Setup { locked, mut path } = self;
        Command::new(&path as &std::path::Path)
            .arg(format!("/UPDATEFROMPID={}", process::id()))
            .spawn()
            .context("Could not open VoiceIsolator Setup")?;
        drop(locked);
        path.disable_cleanup(true);
        Ok(())
    }
}

fn directory() -> Result<std::path::PathBuf> {
    Ok(profile_path()?.with_file_name("updates"))
}

pub fn cleanup() {
    let Ok(dir) = directory() else { return };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with("VoiceIsolator-Setup-")
            && name.ends_with(".exe")
            && entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(24 * 3600))
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

pub fn download(release: Release, cancel: &AtomicBool) -> Result<Setup> {
    let dir = directory()?;
    fs::create_dir_all(&dir)?;
    let mut target = tempfile::Builder::new()
        .prefix("VoiceIsolator-Setup-")
        .suffix(".exe")
        .tempfile_in(dir)?;
    let path = asset_path(&format!("v{}", release.version), INSTALLER);
    let mut body = HttpBody::open(w!("github.com"), &path)?.take(release.size + 1);
    let mut buffer = [0; 64 * 1024];
    let mut bytes = 0;
    while !cancel.load(Relaxed) {
        let read = body.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        bytes += read as u64;
        ensure!(
            bytes <= release.size,
            "Update installer exceeds its signed size"
        );
        target.write_all(&buffer[..read])?;
    }
    ensure!(!cancel.load(Relaxed), "Update download canceled");
    ensure!(
        bytes == release.size,
        "Update installer size differs from the signed manifest"
    );
    target.as_file().sync_all()?;
    let path = target.into_temp_path();
    let mut locked = OpenOptions::new().read(true).share_mode(1).open(&path)?;
    verify_installer(&release, locked.metadata()?.len(), &mut locked)?;
    Ok(Setup { locked, path })
}

fn verify_installer(release: &Release, size: u64, data: impl Read) -> Result<()> {
    ensure!(
        size == release.size,
        "Update installer size differs from the signed manifest"
    );
    ensure!(
        sha256(data)? == release.sha256,
        "Update installer hash differs from the signed manifest"
    );
    Ok(())
}

struct Handle(*mut c_void);

impl Handle {
    fn new(handle: *mut c_void) -> Result<Self> {
        if handle.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self(handle))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = unsafe { WinHttpCloseHandle(self.0) };
    }
}

struct HttpBody {
    request: Handle,
    _connection: Handle,
    _session: Handle,
}

impl HttpBody {
    fn open(host: PCWSTR, path: &str) -> Result<Self> {
        let path: Vec<_> = path.encode_utf16().chain([0]).collect();
        unsafe {
            let session = Handle::new(WinHttpOpen(
                w!("VoiceIsolator"),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ))?;
            WinHttpSetTimeouts(session.0, 5_000, 10_000, 15_000, 15_000)?;
            let connection = Handle::new(WinHttpConnect(session.0, host, 443, 0))?;
            let request = Handle::new(WinHttpOpenRequest(
                connection.0,
                w!("GET"),
                PCWSTR(path.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                ptr::null(),
                WINHTTP_FLAG_SECURE,
            ))?;
            WinHttpSetOption(
                Some(request.0),
                WINHTTP_OPTION_REDIRECT_POLICY,
                Some(&WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP.to_ne_bytes()),
            )?;
            WinHttpSendRequest(request.0, None, None, 0, 0, 0)?;
            WinHttpReceiveResponse(request.0, ptr::null_mut())?;
            let mut status = 0_u32;
            let mut length = size_of_val(&status) as u32;
            WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some((&mut status as *mut u32).cast()),
                &mut length,
                ptr::null_mut(),
            )?;
            ensure!(status == 200, "GitHub returned HTTP {status}");
            Ok(Self {
                request,
                _connection: connection,
                _session: session,
            })
        }
    }
}

impl Read for HttpBody {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let mut count = 0;
        unsafe {
            WinHttpReadData(
                self.request.0,
                bytes.as_mut_ptr().cast(),
                bytes.len() as u32,
                &mut count,
            )
        }
        .map_err(io::Error::other)?;
        Ok(count as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_must_be_stable_numeric_tags() {
        assert!(version("0.1.9").unwrap() < version("0.1.10").unwrap());
        for invalid in [
            "v0.1.0",
            "0.1.0-rc.1",
            "0.01.0",
            "0.+1.0",
            "0.1",
            "0.1.0.0",
            "../../setup",
        ] {
            assert!(version(invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn signed_manifest_rejects_wrong_tag_and_tampering() -> Result<()> {
        const KEY: &str = "RWRR0ZYxWtkMULOvSxCATwSvytXFV02VGGRN5r1OaCIV6SBO1+w0e4Zl";
        const MANIFEST: &[u8] = br#"{"version":"0.1.1","installer":"VoiceIsolator-Setup-x64.exe","size":4,"sha256":"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"}"#;
        const SIGNATURE: &str = "untrusted comment: signature from minisign secret key\n\
RURR0ZYxWtkMUGpCEdXh9bLPgmel9a6RttwjJYtIkWHi/xmpfj9y+hWa6U1XIMcriI1b1t0+52oaZHcHGHxSsPWb6KKcayqRLAQ=\n\
trusted comment: VoiceIsolator updater test fixture\n\
G0SfGaBQyn3bCTrG99poFO33zmZ0B7fgXI/B2FgqL8s2sbL9L4qManeLRnIzjL246hu1GZx/rpcsew3Ji6dpCQ==\n";
        let key = PublicKey::from_base64(KEY)?;
        let release = verify_manifest(&key, "v0.1.1", MANIFEST, SIGNATURE)?;
        verify_installer(&release, 4, &b"test"[..])?;
        assert!(verify_installer(&release, 3, &b"tes"[..]).is_err());
        assert!(verify_installer(&release, 4, &b"fail"[..]).is_err());
        assert!(verify_manifest(&key, "v0.1.2", MANIFEST, SIGNATURE).is_err());
        let mut changed = MANIFEST.to_vec();
        changed.push(b' ');
        assert!(verify_manifest(&key, "v0.1.1", &changed, SIGNATURE).is_err());
        Ok(())
    }
}
