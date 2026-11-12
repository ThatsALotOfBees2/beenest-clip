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
- `ffmpeg` on your `PATH` (or pass `--ffmpeg <path>` / set `BEENEST_FFMPEG`)

## Usage

```sh
cargo build --release

beenest-clip run                      # 60 s buffer, clips go to ./clips
beenest-clip run --seconds 120 --bitrate 20000 --out ~/Videos/clips

beenest-clip save                     # from another terminal: save the buffer
beenest-clip save --seconds 30        # save only the last 30 s
beenest-clip status                   # buffer length, size, measured bitrate
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

## Status

Prototype. Tested end to end on Linux (X11 capture, x264, audio, hotkey,
buffer wraparound). The Windows and macOS code paths compile but have not
been run on those systems, and no hardware encoder has been exercised yet.

Not built yet: native Wayland capture, native Windows audio loopback,
per-game detection, a UI, and the benchmark harness.

The control port (`--port`, default 47810) listens on localhost only, but any
local program can use it to trigger a save.

## Tests

```sh
cargo test
```
