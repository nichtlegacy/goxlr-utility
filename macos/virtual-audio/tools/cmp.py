#!/usr/bin/env python3
"""Compare mono 48 kHz float WAVs from the GoXLR meter."""

import argparse
import struct
from pathlib import Path

import numpy as np


def read_wave(path):
    raw = path.read_bytes()
    if raw[:4] != b"RIFF" or raw[8:12] != b"WAVE":
        raise ValueError(f"{path}: expected a RIFF WAV")
    sample_data = None
    wave_format = None
    offset = 12
    while offset + 8 <= len(raw):
        name, size = struct.unpack_from("<4sI", raw, offset)
        start = offset + 8
        if start + size > len(raw):
            raise ValueError(f"{path}: truncated WAV chunk")
        if name == b"fmt " and size >= 16:
            wave_format = struct.unpack_from("<HHIIHH", raw, start)
        if name == b"data" and size and size % 4 == 0:
            sample_data = np.frombuffer(raw, dtype="<f4", count=size // 4, offset=start)
        offset = start + size + (size % 2)
    if wave_format != (3, 1, 48000, 192000, 4, 32) or sample_data is None:
        raise ValueError(f"{path}: expected mono 48 kHz IEEE float PCM")
    return sample_data


def correlation(reference, capture):
    size = 1 << (len(reference) + len(capture) - 1).bit_length()
    left = np.fft.rfft(reference, size)
    right = np.fft.rfft(capture, size)
    return np.fft.irfft(right * left.conj(), size)


def bit_equal(left, right):
    return left.view("<u4") == right.view("<u4")


def print_match(label, match):
    edges = np.diff(np.r_[False, ~match, False].astype("i1"))
    lengths = np.flatnonzero(edges == -1) - np.flatnonzero(edges == 1)
    runs = np.count_nonzero(lengths >= 16)
    print(f"{label}: {match.mean():.6%}, {runs} mismatch runs of at least 16 frames")


def compare(reference, capture, interpolation):
    corr = correlation(reference, capture)
    # Ignore the zero-padded middle of the circular FFT result.
    corr[len(capture):len(corr) - len(reference) + 1] = -np.inf
    lag = int(np.argmax(corr))
    if lag >= len(capture):
        lag -= len(corr)
    first = max(1000, lag + 1000)
    last = min(len(capture), len(reference) + lag) - 10
    if last <= first:
        raise ValueError("Too little aligned audio after trimming 1000 leading and 10 trailing frames")

    base = reference[first - lag:last - lag]
    output = capture[first:last]
    print(f"Lag: {lag} frames, overlap: {len(output)} frames after trimming")
    print_match("Tolerance (atol=1e-7, rtol=1e-5)", np.isclose(base, output, atol=1e-7))
    print_match("Exact float bits", bit_equal(base, output))

    blocks = capture[:len(capture) // 512 * 512].reshape(-1, 512)
    leading = blocks[:, :96]
    nonconstant = np.any(leading != leading[:, :1], axis=1)
    repeated = np.all(bit_equal(leading, blocks[:, 416:]), axis=1) & nonconstant
    print(f"416/96 repeats: {repeated.sum()} of {nonconstant.sum()} nonconstant 512-frame blocks")

    if not interpolation:
        return

    # Reconstruct the bridge's f32 interpolation by fitting one constant phase for the whole
    # overlap. Try either adjacent source frame and check nearby f32 phase values.
    best_phase = 0.0
    best_match = bit_equal(base, output)
    residual = (output - base).astype("f8")
    for direction in (-1, 1):
        delta = reference[first - lag + direction:last - lag + direction] - base
        weights = delta.astype("f8")
        energy = weights @ weights
        if energy == 0:
            continue
        estimate = float(np.clip((weights @ residual) / energy, 0.0, 1.0))
        tick = round(estimate * 2**24)
        candidates = [estimate] + [i / 2**24 for i in range(max(0, tick - 8), min(2**24, tick + 8) + 1)]
        for phase in candidates:
            match = bit_equal(base + delta * np.float32(phase), output)
            if match.sum() > best_match.sum():
                best_match, best_phase = match, direction * phase
    print_match(f"Bridge reconstruction at phase {best_phase:+.9f} samples", best_match)


def find_tone(reference, capture):
    if len(capture) < len(reference):
        raise ValueError("The captured WAV must be at least as long as the reference tone")
    dot = correlation(reference, capture)[:len(capture) - len(reference) + 1]
    start = int(np.argmax(np.abs(dot)))
    excerpt = capture[start:start + len(reference)].astype("f8")
    energy = np.dot(reference.astype("f8"), reference.astype("f8"))
    norm = np.sqrt(energy * (excerpt @ excerpt))
    print(f"Tone start: {start / 48000:.4f}s, gain: {dot[start] / energy:+.6f}, "
          f"normalized correlation: {dot[start] / norm if norm else 0.0:+.6f}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--bridge-interpolation", action="store_true", help="also compare one fitted constant phase")
    mode.add_argument("--tone", action="store_true", help="find a short reference tone in a longer capture")
    parser.add_argument("reference", type=Path)
    parser.add_argument("capture", type=Path)
    args = parser.parse_args()
    try:
        reference = read_wave(args.reference)
        capture = read_wave(args.capture)
        if np.ptp(reference) == 0 or np.ptp(capture) == 0:
            raise ValueError("Both WAVs must contain a varying signal for alignment")
        if args.tone:
            find_tone(reference, capture)
        else:
            compare(reference, capture, args.bridge_interpolation)
    except (OSError, ValueError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
