"""Score time-aligned 16 kHz PCM16 room recordings from a local JSON manifest."""

import argparse
from array import array
import json
import math
from pathlib import Path
import statistics
import sys
import wave


SAMPLE_RATE = 16_000
POWER_FLOOR = 1.0  # One PCM16 unit squared.


def read_pcm(path):
    with wave.open(str(path), "rb") as wav:
        if (wav.getframerate(), wav.getnchannels(), wav.getsampwidth()) != (SAMPLE_RATE, 1, 2):
            raise ValueError(f"{path} requires mono 16 kHz PCM16")
        frames = wav.getnframes()
        pcm = array("h", wav.readframes(frames))
        if len(pcm) != frames:
            raise ValueError(f"{path} is truncated")
    if sys.byteorder != "little":
        pcm.byteswap()
    return pcm


def mean_square(samples):
    return sum(sample * sample for sample in samples) / len(samples)


def si_sdr(samples, reference):
    mean_samples = statistics.fmean(samples)
    mean_reference = statistics.fmean(reference)
    centered_samples = [value - mean_samples for value in samples]
    centered_reference = [value - mean_reference for value in reference]
    reference_power = mean_square(centered_reference)
    if reference_power <= POWER_FLOOR:
        raise ValueError("Overlap reference has no measurable target signal")
    projection = statistics.fmean(
        a * b for a, b in zip(centered_samples, centered_reference, strict=True)
    ) / reference_power
    projected_power = projection**2 * reference_power
    error_power = mean_square([
        a - projection * b for a, b in zip(centered_samples, centered_reference, strict=True)
    ])
    if mean_square(centered_samples) <= POWER_FLOOR:
        return -120.0
    return 10 * math.log10(max(projected_power, POWER_FLOOR) / max(error_power, POWER_FLOOR))


def score(manifest):
    document = json.loads(manifest.read_text(encoding="utf-8"))
    base = manifest.parent
    attenuation, overlap_gain, segments = [], [], []
    for take in document["takes"]:
        raw = read_pcm(base / take["raw"])
        isolated = read_pcm(base / take["isolated"])
        reference = read_pcm(base / take["target_stem"]) if "target_stem" in take else None
        if len(raw) != len(isolated) or (reference is not None and len(raw) != len(reference)):
            raise ValueError("Trim or align all channels to the same sample count before scoring")
        for segment in take["segments"]:
            start, end, kind = segment["start"], segment["end"], segment["kind"]
            if (type(start) is not int or type(end) is not int or
                    not 0 <= start < end <= len(raw)):
                raise ValueError("Segment boundaries must be valid integer sample offsets")
            source, enhanced = raw[start:end], isolated[start:end]
            entry = {"take": take["raw"], **segment}
            if kind == "interferer":
                input_power = mean_square(source)
                if input_power <= POWER_FLOOR:
                    raise ValueError("Interferer segment has no measurable input signal")
                value = 10 * math.log10(input_power / max(mean_square(enhanced), POWER_FLOOR))
                attenuation.append(value)
                entry["attenuation_db"] = value
            elif kind == "overlap" and reference is not None:
                stem = reference[start:end]
                value = si_sdr(enhanced, stem) - si_sdr(source, stem)
                overlap_gain.append(value)
                entry["si_sdr_improvement_db"] = value
            else:
                raise ValueError("Use interferer segments or overlap segments with a target stem")
            entry["samples_at_pcm_limit"] = sum(value in (-32768, 32767) for value in enhanced)
            segments.append(entry)

    median_attenuation = statistics.median(attenuation) if attenuation else None
    p10_attenuation = sorted(attenuation)[math.ceil(len(attenuation) * 0.10) - 1] if attenuation else None
    median_overlap = statistics.median(overlap_gain) if overlap_gain else None
    return {
        "interferer_median_db": median_attenuation,
        "interferer_p10_db": p10_attenuation,
        "overlap_median_si_sdr_improvement_db": median_overlap,
        "quantitative_gate": (
            median_attenuation is not None and median_attenuation >= 20 and p10_attenuation >= 12
            and median_overlap is not None and median_overlap >= 5
        ),
        "segments": segments,
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    args = parser.parse_args()
    try:
        print(json.dumps(score(args.manifest), indent=2))
    except (KeyError, OSError, ValueError, wave.Error) as error:
        parser.exit(2, f"Room score: {error}\n")
