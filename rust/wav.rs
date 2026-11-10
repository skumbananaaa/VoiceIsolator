use crate::RATE;
use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
};

const MAX_SAMPLES: usize = RATE * 3600;

pub fn read(path: &Path) -> Result<Vec<f32>> {
    let file = File::open(path).with_context(|| format!("Open {}", path.display()))?;
    let len = file.metadata()?.len();
    let mut file = BufReader::new(file);
    let mut riff = [0; 12];
    file.read_exact(&mut riff)?;
    ensure!(
        &riff[..4] == b"RIFF" && &riff[8..] == b"WAVE",
        "Expected a RIFF/WAVE file"
    );
    let end = 8 + u32::from_le_bytes(riff[4..8].try_into()?) as u64;
    ensure!((12..=len).contains(&end), "Invalid RIFF size");
    let mut format = false;
    let mut data = None;
    let mut offset = 12;
    while offset < end {
        ensure!(end - offset >= 8, "Truncated WAV chunk");
        file.seek(SeekFrom::Start(offset))?;
        let mut chunk = [0; 8];
        file.read_exact(&mut chunk)?;
        let size = u32::from_le_bytes(chunk[4..].try_into()?) as u64;
        let start = offset + 8;
        offset = start + size + (size & 1);
        ensure!(offset <= end, "WAV chunk extends beyond RIFF data");
        match &chunk[..4] {
            b"fmt " => {
                ensure!(!format && size >= 16, "Invalid WAV format chunk");
                let mut actual = [0; 16];
                file.read_exact(&mut actual)?;
                ensure!(actual == pcm_format(), "Expected mono 16 kHz PCM16 WAV");
                format = true;
            }
            b"data" => {
                ensure!(
                    data.is_none() && size > 0 && size % 2 == 0 && size / 2 <= MAX_SAMPLES as u64,
                    "Invalid WAV data size or multiple data chunks"
                );
                data = Some((start, size as usize));
            }
            _ => {}
        }
    }
    ensure!(format, "Missing WAV format");
    let (start, size) = data.context("Missing WAV audio data")?;
    file.seek(SeekFrom::Start(start))?;
    let mut pcm = vec![0; size];
    file.read_exact(&mut pcm)?;
    Ok(pcm
        .chunks_exact(2)
        .map(|s| i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0)
        .collect())
}

fn pcm_format() -> [u8; 16] {
    [1, 0, 1, 0, 0x80, 0x3e, 0, 0, 0x00, 0x7d, 0, 0, 2, 0, 16, 0]
}

pub fn write_new(path: &Path, samples: &[f32]) -> Result<()> {
    ensure!(
        (1..=MAX_SAMPLES).contains(&samples.len()) && samples.iter().all(|x| x.is_finite()),
        "Cannot write invalid or oversized audio"
    );
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    let size = samples.len() as u32 * 2;
    {
        let mut file = BufWriter::new(temporary.as_file_mut());
        file.write_all(b"RIFF")?;
        file.write_all(&(size + 36).to_le_bytes())?;
        file.write_all(b"WAVEfmt \x10\x00\x00\x00")?;
        file.write_all(&pcm_format())?;
        file.write_all(b"data")?;
        file.write_all(&size.to_le_bytes())?;
        for &sample in samples {
            let pcm = (sample.clamp(-1.0, 32767.0 / 32768.0) * 32768.0).round() as i16;
            file.write_all(&pcm.to_le_bytes())?;
        }
        file.flush()?;
    }
    temporary.persist_noclobber(path)?;
    Ok(())
}
