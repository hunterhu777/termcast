# termcast

A terminal media player for mp4 and webm. Decodes with libav, plays audio
through the system device, and draws video as text in your terminal.

Black-and-white ASCII by default; colour and half-block rendering are opt-in.

```
termcast video.mp4
termcast --color --half-block video.webm
termcast --fps 15 --cols 200 video.mp4
```

## Requirements

- **Rust** 1.75 or newer
- **ffmpeg 8.x** development libraries and **pkg-config**

The ffmpeg dependency is the only real setup hurdle: `ffmpeg-next` binds the C
libraries directly, so the headers must be present at build time and the
major version has to match. The `Cargo.toml` pins `ffmpeg-next = "8.1"` against
ffmpeg 8.1.

```sh
brew install ffmpeg pkgconf          # macOS
```

If your distribution ships a different ffmpeg major version, change the
`ffmpeg-next` version in `Cargo.toml` to match it (7.x for ffmpeg 7, and so on).

## Build

```sh
cargo build --release
./target/release/termcast --help
```

## Rendering modes

Two independent axes: which renderer, and whether it uses colour.

| | black & white (default) | `--color` |
|---|---|---|
| **ASCII** (default) | one glyph per cell, single constant colour | glyph plus per-cell foreground |
| **`--half-block`** | greyscale, two pixels per cell | full colour, two pixels per cell |

**ASCII** picks a glyph from a luminance ramp, one pixel per cell. A per-frame
contrast stretch (2nd/98th percentile) keeps real footage from collapsing into
the mid-tones. It works anywhere, including over SSH and when redirected to a
file.

**Half-block** prints `U+2580` in every cell with the foreground set to the top
pixel and the background to the bottom one, so each cell carries two vertically
stacked pixels. That doubles vertical resolution and removes glyph-shape
texture, at roughly double the escape traffic. It needs a font that draws the
glyph without seams.

`--ascii-bg` gives the ASCII renderer a per-cell background chosen so that
`coverage x fg + (1 - coverage) x bg` equals the pixel's true value. The cell
then integrates to the correct luminance and the tone response is close to
linear, rather than bunched at the dark end the way glyph coverage alone is.
Useful on terminals that cannot render `U+2580`.

Output is redirected-friendly: when stdout is not a terminal, termcast streams
whole frames as plain rows with no cursor addressing, so `termcast v.mp4 > out.txt`
produces something readable.

## Options

| flag | effect |
|---|---|
| `--color` | render in colour (default is black and white) |
| `--half-block` | two pixels per cell via `U+2580` |
| `--ascii-bg` | per-cell background in the ASCII renderer |
| `--no-bg` | keep the terminal's own background instead of painting black |
| `--cols N` | cap picture width in characters; height follows, preserving aspect |
| `--fps N` | cap presented frames per second |
| `--start SEC` / `--duration SEC` | play a segment |
| `--no-audio` | do not open an audio device |
| `--stats` | live sync/performance overlay |
| `--stats-log FILE` | per-frame timings as CSV, plus a summary on exit |

## Controls

| key | action |
|---|---|
| `space` | pause / resume |
| `left` / `right` | seek 5s |
| `PgDn` / `PgUp` | seek 60s |
| `up` / `down` | volume |
| `m` | mute |
| `s` | toggle the stats overlay |
| `q` / `Esc` / `Ctrl-C` | quit |

## How it works

Audio is the master clock. The device reports how many samples it has actually
consumed, and video frames are presented against that: early frames wait, late
frames are dropped. A wall clock takes over for files with no audio track.

One thread owns the ffmpeg input and both decoders. Sharing libav contexts
across threads buys nothing here, since the video decoders are already
internally threaded.

The cell grid is double-buffered. Only cells that changed are written, and runs
sharing a colour are coalesced into a single escape sequence, which matters
because a full repaint of a large grid is hundreds of kilobytes.

Two details that are easy to get wrong:

- **Seeks re-base the clock.** A seek lands on the nearest keyframe at or
  *before* the requested time, so trusting the requested position makes every
  subsequent frame look late and get dropped permanently. The clock is reset to
  the first decoded timestamp after the seek instead.
- **The producer never starves audio.** A full video channel is normal, since
  decoding outruns real time, and parking there is the back-pressure that paces
  the pipeline. But parking long enough to empty the audio queue stalls the
  clock, and a stalled clock disables frame dropping, so a slow terminal makes
  playback run slow rather than skip. The producer discards a video frame only
  when the audio buffer drops below 250ms.

## If playback stutters

Large terminal windows are the usual cause: the cost is your emulator
compositing the grid, not decoding. `--cols` shrinks the picture (halving it
quarters the cell count, since height follows width) and `--fps` reduces
repaints. Run with `--stats-log` and compare the `draw` column, which is
termcast's own work, against `write`, which is time spent handing bytes to the
terminal.

## Tests

The renderers can be checked against a synthetic gradient: cell brightness must
increase monotonically from black to white in every mode.

```sh
ffmpeg -f lavfi -i "gradients=s=320x64:c0=black:c1=white:x0=0:y0=0:x1=320:y1=0" \
       -t 2 -pix_fmt yuv420p /tmp/grad.mp4
termcast --cols 64 /tmp/grad.mp4 > /tmp/out.txt
```

## Status

Developed and tested on macOS (Apple silicon) against ffmpeg 8.1, with
h264/mp4, hevc/mp4, AV1/webm, and audio-only mp3. The dependencies
(`cpal`, `crossterm`) are cross-platform but other platforms are untested.
