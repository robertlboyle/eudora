"""Makes Eudora's own new-mail sounds (crates/eudora-app/ui/sounds/*.wav), synthesized here from
plain tones, so they are Eudora's and no one else's. Classic Eudora's own sounds
(eudora.wav, eudora-older.wav) are copied from its released source, not made here.

    py -3 scripts/make_sounds.py

Standard library only. Each sound: 16-bit mono, 44.1 kHz, a few tenths of a second.
"""

import math
import struct
import wave
from pathlib import Path

RATE = 44100
OUT = Path(__file__).resolve().parent.parent / "crates" / "eudora-app" / "ui" / "sounds"


def note(freq, start, length, volume=1.0, decay=6.0, partials=((1, 1.0),), attack=0.004):
    """One struck note: a sum of partials (multiple, strength) with a quick attack and an
    exponential fade, from `start` seconds for `length` seconds."""
    return (freq, start, length, volume, decay, partials, attack)


def render(notes, total, path, gain=0.5):
    samples = [0.0] * int(total * RATE)
    for freq, start, length, volume, decay, partials, attack in notes:
        first = int(start * RATE)
        for i in range(int(length * RATE)):
            n = first + i
            if n >= len(samples):
                break
            t = i / RATE
            env = min(1.0, t / attack) * math.exp(-decay * t)
            # (each partial fades a little faster than the one below it, as a bell's do)
            s = sum(strength * math.exp(-decay * (multiple - 1) * 0.35 * t) * math.sin(2 * math.pi * freq * multiple * t) for multiple, strength in partials)
            samples[n] += volume * env * s
    peak = max(1e-9, max(abs(s) for s in samples))
    # a short fade at the very end, so nothing clicks
    tail = int(0.02 * RATE)
    for i in range(tail):
        samples[-1 - i] *= i / tail
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(b"".join(struct.pack("<h", int(32767 * gain * s / peak)) for s in samples))
    print(f"{path.name}: {total:.2f} s, {path.stat().st_size:,} bytes")


BELL = ((1, 1.0), (2, 0.35), (3, 0.12), (4.2, 0.06))
SOFT = ((1, 1.0), (2, 0.18), (3, 0.05))
WOOD = ((1, 1.0), (3.9, 0.25), (9.8, 0.05))  # a marimba's bar: its overtones aren't whole multiples


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    # Gmail-style: a light, quick two-note rise, soft and round
    render([note(880.0, 0.00, 0.5, 0.8, 9, SOFT), note(1318.5, 0.09, 0.6, 1.0, 7, SOFT)], 0.7, OUT / "chime.wav")
    # Yahoo-style: a playful three-note run on wooden bars
    render([note(784.0, 0.00, 0.3, 0.9, 14, WOOD), note(1046.5, 0.08, 0.3, 0.9, 14, WOOD), note(1568.0, 0.16, 0.45, 1.0, 10, WOOD)], 0.62, OUT / "marimba.wav")
    # a plain, single bell (for a look whose program uses the system's sound, on a system Eudora
    # can't ask for its own)
    render([note(1174.7, 0.00, 0.9, 1.0, 5, BELL)], 0.9, OUT / "ding.wav")


if __name__ == "__main__":
    main()
