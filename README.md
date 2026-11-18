# beenest-clip

A low-overhead instant-replay tool: it records your screen into a rolling
buffer in memory, and a hotkey saves the last N seconds as an MP4.

## How it works

1. An FFmpeg child process captures the screen and encodes it, on a hardware
   encoder when one works (NVENC, AMF, Quick Sync, VAAPI, VideoToolbox), with
   software x264 as the fallback.
2. The compressed stream is piped into a fixed-size ring buffer that is
   allocated once and indexed by keyframe.
3. Saving copies the bytes from the right keyframe onwards and wraps them in
   an MP4. Nothing is re-encoded, so a save takes around 100 ms.

Only compressed data passes through the Rust process, which sits at roughly
0.1% CPU and 4 MB of memory plus the buffer itself.

## Requirements

- Rust (stable) to build
- `ffmpeg` 5.1 or newer on your `PATH` (or pass `--ffmpeg <path>` / set `BEENEST_FFMPEG`)

## Usage

```sh
cargo build --release

beenest-clip run                      # 60 s buffer, clips go to ./clips
beenest-clip run --seconds 120 --bitrate 20000 --out ~/Videos/clips

beenest-clip save                     # from another terminal: save the buffer
beenest-clip save --seconds 30        # save only the last 30 s
beenest-clip status                   # buffer, bitrate, frames encoded / duplicated
beenest-clip encoders                 # which encoders work on this machine
```

While `run` is active a clip is saved by any of:

- the global hotkey (default `alt+F10`, change with `--hotkey`, disable with `--hotkey none`)
- pressing Enter in its terminal
- `beenest-clip save`

Clips start on a keyframe, so they can be up to `--keyframe-secs` (default 2)
longer than asked.

### Audio

Audio is off unless you name an FFmpeg audio input:

```sh
# Linux, desktop audio through PulseAudio / PipeWire
beenest-clip run --audio-format pulse --audio-input @DEFAULT_MONITOR@
# Windows, a DirectShow loopback device such as Stereo Mix
beenest-clip run --audio-format dshow --audio-input "audio=Stereo Mix"
```

### Capture backends

| OS | Default | Notes |
|---|---|---|
| Windows | `ddagrab` | Frames stay on the GPU when the encoder is NVENC or AMF. `--capture gdigrab` is a slow fallback. |
| Linux | `x11grab` | X11 only. On Wayland it sees XWayland windows only. |
| macOS | `avfoundation` | Needs screen-recording permission; the hotkey needs accessibility permission. |

`--capture test` records a synthetic pattern, for trying things out without a screen.

## Benchmark

`beenest-clip bench` measures what recording costs on your machine. Anything
after `--` is passed to `run`, so you can benchmark any setup:

```sh
beenest-clip bench                                  # your default setup
beenest-clip bench -- --encoder nvenc --fps 120
beenest-clip bench --json before.json               # save the results
beenest-clip bench --baseline before.json           # fail if anything got >10% worse
```

It runs four phases (10 s each by default, set with `--duration`):

1. **Game alone:** a synthetic CPU-bound game, one thread per CPU, that
   renders "frames" of fixed work. This is the baseline.
2. **Recorder alone:** CPU and memory of the recorder and its FFmpeg process.
3. **Both together:** how much the game slowed down, and whether capture kept
   up. A frame captured too late is repeated, and counted as *duplicated*.
4. **Saves:** a few clips are saved and timed, and the last one is fully
   decoded to check it is valid.

Example from a 4-CPU Linux VM, x264 at 1080p60, static screen:

```
Recorder alone
  CPU          73.6% of one core avg, 85.6% p95  (18.4% of the machine)
  memory       125 MB peak (recorder + FFmpeg, including the buffer)
  encoder      60.4 fps, 0.0% duplicated, 0.0% dropped, speed 1.00x

With a CPU-bound game on 4 threads
  game fps     114.6 -> 82.8  (-27.7%)

Saves (3)
  latency      min 85 ms, median 86 ms, max 88 ms
```

What it does not measure yet:

- **GPU load.** The synthetic game only competes for the CPU, so it
  understates the cost of GPU capture and encoding, and overstates the cost
  of software encoding relative to a real, GPU-bound game.
- **A real game's frame times.** Use PresentMon (Windows) or MangoHud
  (Linux) alongside a real game for that.

`--capture test` generates its picture with the CPU, which inflates the
recorder's CPU figure; use it for checking that things work, not for
absolute numbers.

## Status

Prototype. Tested end to end on Linux (X11 capture, x264, audio, hotkey,
buffer wraparound). The Windows and macOS code paths compile but have not
been run on those systems, and no hardware encoder has been exercised yet.

Not built yet: native Wayland capture, native Windows audio loopback,
per-game detection, a UI, and GPU metrics in the benchmark.

The control port (`--port`, default 47810) listens on localhost only, but any
local program can use it to trigger a save.

## Tests

```sh
cargo test
```
