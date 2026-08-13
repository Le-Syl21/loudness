# loudness

Automatic loudness normalization plugin for Visual Pinball — measures PinMAME
ROM sound and PUP pack audio to EBU R128, then applies a per-source gain on the
audio bus.

## The problem, measured

A cabinet plays several audio sources that were never mixed together: the rom,
an AltSound pack, a PUP pack, the table's own samples. Nobody agreed on a level,
so the gap between them is whatever it happens to be. Measured on real content:

| Source | Loudness | |
|---|---:|---|
| Whirlwind, rom sound | −23.6 LUFS | |
| Attack From Mars, rom sound | −44.0 LUFS | **20 LU below Whirlwind** |
| Jumanji PUP pack | −16.8 LUFS median | 23.3 LU between its own clips |
| Guns N' Roses PUP pack | −17.6 LUFS median | 15.4 LU between its own clips |

Twenty LU between two tables is not a preference, it is a table you cannot hear
followed by one that makes people jump. And inside a single pack, the spread is
just as wide: on Jumanji every game clip sits at `Volume 200`, the most its
author could give it, and still lands 6 to 14 LU below the attract loop that
plays when nobody is at the machine.

## What it does about it

Measure once, apply **one constant gain**. A constant offset leaves every ratio
inside a source untouched, so the flipper click keeps its bite — that is the
whole reason for measuring loudness rather than peaks, and for never reaching
for a compressor.

Three rules follow from it:

- **Normalize the source, never the individual sound.** Bringing every sound to
  the target would put the click at the level of the background music.
- **The ceiling wins over the target.** When a boost would push true peaks past
  −1 dBTP, it is the target that gives way, not the transients. What does not
  fit is reported, not applied.
- **Nothing is re-encoded.** The gain goes into fields that already exist: the
  `GAIN` column of an AltSound csv, the `Volume` columns of a PUP pack. Original
  files are kept as `.bak`, and the audio itself is never touched.

PUP packs are the one case corrected *from the inside* as well: their clips come
from unrelated sources and were never mixed together, so the spread between them
is nobody's intention. Even there the correction is bounded — outliers are
pulled back to the edge of a band around the median, not onto the median, so a
clip the author wanted loud stays the loudest.

## Usage

```sh
# AltSound: measure a pack and report the gain it needs
loudness scan path/to/altsound/afm_113b

# AltSound: write the gain into the csv
loudness apply path/to/altsound/afm_113b

# PUP: measure a pack and report how spread out its clips are (reads only)
loudness pup-scan "path/to/pupvideos/jumanji"

# PUP: write the corrected volumes into triggers.pup
loudness pup-apply "path/to/pupvideos/jumanji"
```

Both `apply` commands keep the original as `.bak` and drop a `loudness.json`
next to the pack recording what was measured and what was written.

Useful flags: `--target` and `--ceiling` for AltSound, `--band` for how far a
PUP clip may sit from the median before it is corrected, `--force` to redo work
that is already done, `--verbose` to list every file measured.

## Where the gain lands

| Source | Corrected through | Notes |
|---|---|---|
| AltSound pack | `GAIN` column, same factor on every row | column tops out at 100, the remainder goes to the bus |
| PUP pack | `Volume` in `triggers.pup` and `playlists.pup` | percentages, `0` means silent and is never touched |
| PinMAME rom | `AudioSource.pinmame.Gain` per table | *not implemented yet* |
| Table samples | `AudioSource.<id>.Gain` per table | *not implemented yet*, they live inside the `.vpx` |

`AudioSource.<id>.Gain` is a VPX 10.8.1 feature, which is what this targets.

## Not covered

- **PinSound legacy packs** (`jingle/`, `music/`, `sfx/`, `voice/` folders with
  no csv). They carry no gain field, so nothing can be written into them.
- **Packs mixing sample rates or channel layouts** in one AltSound folder: the
  tool refuses to conclude rather than return a number it cannot stand behind.
- Codecs Symphonia does not decode, HE-AAC among them. A file that fails is
  reported and skipped; a video with no audio track at all is normal in a PUP
  pack and is counted separately.

## Doing the work only once

A pack carries a `loudness.json` recording its fingerprint, the target, and a
signature of the measuring engine. On the next run, nothing is re-measured
unless one of those changed — 25 s down to 0.14 s on a 575-file pack.

The engine signature is not a version number. A patch release of the meter may
change nothing, or may move every measurement, and the number does not say
which. So the engine signs itself by behaviour: it measures a synthetic
reference signal built to exercise K-weighting, gating and inter-sample peaks,
and hashes the outcome. Same numbers, same signature, no rescan — and updating
an unrelated crate costs nobody anything.

## Building

```sh
cargo build --release
```

Pure Rust, no native dependency: [Symphonia](https://github.com/pdeljanov/Symphonia)
for decoding, [ebur128](https://github.com/sdroege/ebur128) for the measurement.
