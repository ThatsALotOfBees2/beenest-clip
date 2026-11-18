//! Builds the FFmpeg command lines for capture+encode and for clip muxing.
//!
//! FFmpeg runs as a child process: it owns the capture device and the
//! hardware encoder, and hands us an already-compressed stream over a pipe.
//! Only compressed bytes ever cross into this process.

use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use anyhow::{bail, Context, Result};
use clap::ValueEnum;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Capture {
    /// Pick the best backend for this OS.
    Auto,
    /// Windows: Desktop Duplication, frames stay on the GPU.
    Ddagrab,
    /// Windows: GDI capture (slow, CPU frames; compatibility fallback).
    Gdigrab,
    /// Linux: X11 screen capture.
    X11grab,
    /// macOS: AVFoundation screen capture.
    Avfoundation,
    /// Synthetic moving test pattern (no screen needed).
    Test,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Encoder {
    /// Probe hardware encoders and fall back to software.
    Auto,
    Nvenc,
    Amf,
    Qsv,
    Vaapi,
    Videotoolbox,
    X264,
}

impl Encoder {
    pub fn name(self) -> &'static str {
        match self {
            Encoder::Auto => "auto",
            Encoder::Nvenc => "h264_nvenc",
            Encoder::Amf => "h264_amf",
            Encoder::Qsv => "h264_qsv",
            Encoder::Vaapi => "h264_vaapi",
            Encoder::Videotoolbox => "h264_videotoolbox",
            Encoder::X264 => "libx264",
        }
    }

    /// Encoders worth trying on this OS, cheapest for the game first.
    pub fn candidates() -> &'static [Encoder] {
        if cfg!(windows) {
            &[Encoder::Nvenc, Encoder::Amf, Encoder::Qsv, Encoder::X264]
        } else if cfg!(target_os = "macos") {
            &[Encoder::Videotoolbox, Encoder::X264]
        } else {
            &[Encoder::Nvenc, Encoder::Vaapi, Encoder::Qsv, Encoder::X264]
        }
    }

    /// Can this encoder take Direct3D 11 textures without a download?
    fn takes_d3d11(self) -> bool {
        matches!(self, Encoder::Nvenc | Encoder::Amf)
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub ffmpeg: String,
    pub capture: Capture,
    pub encoder: Encoder,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub gop_secs: u32,
    pub display: Option<String>,
    pub vaapi_device: String,
    pub audio_format: Option<String>,
    pub audio_input: Option<String>,
    pub audio_kbps: u32,
}

impl Settings {
    pub fn resolved_capture(&self) -> Capture {
        match self.capture {
            Capture::Auto if cfg!(windows) => Capture::Ddagrab,
            Capture::Auto if cfg!(target_os = "macos") => Capture::Avfoundation,
            Capture::Auto => Capture::X11grab,
            c => c,
        }
    }

    fn has_audio(&self) -> bool {
        self.audio_input.is_some()
    }

    fn base(&self) -> Command {
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-nostats"]);
        cmd
    }

    /// Options that must precede the inputs.
    fn global_args(&self, enc: Encoder) -> Vec<String> {
        match enc {
            Encoder::Vaapi => vec!["-vaapi_device".into(), self.vaapi_device.clone()],
            _ => vec![],
        }
    }

    fn input_args(&self, capture: Capture, enc: Encoder) -> Vec<String> {
        let fps = self.fps.to_string();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        match capture {
            Capture::Ddagrab => {
                let mut graph = format!("ddagrab=framerate={fps}");
                if !enc.takes_d3d11() {
                    graph.push_str(",hwdownload,format=bgra");
                }
                s(&["-f", "lavfi", "-i", &graph])
            }
            Capture::Gdigrab => s(&["-f", "gdigrab", "-framerate", &fps, "-draw_mouse", "1", "-i", "desktop"]),
            Capture::X11grab => {
                let display = self
                    .display
                    .clone()
                    .or_else(|| std::env::var("DISPLAY").ok())
                    .unwrap_or_else(|| ":0".into());
                s(&["-f", "x11grab", "-framerate", &fps, "-draw_mouse", "1", "-i", &display])
            }
            Capture::Avfoundation => {
                let screen = self.display.clone().unwrap_or_else(|| "Capture screen 0".into());
                s(&["-f", "avfoundation", "-framerate", &fps, "-capture_cursor", "1", "-i", &format!("{screen}:none")])
            }
            Capture::Test => s(&["-re", "-f", "lavfi", "-i", &format!("testsrc2=size=1280x720:rate={fps}")]),
            Capture::Auto => unreachable!("capture is resolved before building arguments"),
        }
    }

    /// Encoder options, tuned for lowest cost rather than quality per bit:
    /// fastest preset, no B-frames, no lookahead, fixed keyframe interval.
    fn video_args(&self, enc: Encoder, gpu_frames: bool) -> Vec<String> {
        let rate = format!("{}k", self.bitrate_kbps);
        let gop = (self.fps * self.gop_secs).max(1).to_string();
        let mut a: Vec<&str> = vec!["-c:v", enc.name()];
        match enc {
            Encoder::Nvenc => a.extend(["-preset", "p1", "-rc", "cbr"]),
            Encoder::Amf => a.extend(["-quality", "speed", "-rc", "cbr"]),
            Encoder::Qsv => {
                a.extend(["-preset", "veryfast"]);
                if !gpu_frames {
                    a.extend(["-vf", "format=nv12"]);
                }
            }
            Encoder::Vaapi => a.extend(["-vf", "format=nv12,hwupload", "-rc_mode", "CBR"]),
            Encoder::Videotoolbox => a.extend(["-realtime", "1"]),
            Encoder::X264 => a.extend([
                "-preset", "ultrafast", "-tune", "zerolatency", "-pix_fmt", "yuv420p",
                "-sc_threshold", "0",
            ]),
            Encoder::Auto => unreachable!("encoder is resolved before building arguments"),
        }
        a.extend(["-b:v", &rate, "-maxrate", &rate, "-bufsize", &rate, "-g", &gop, "-bf", "0"]);
        a.into_iter().map(String::from).collect()
    }

    /// Run a few frames through an encoder to see whether it really works
    /// on this machine (being compiled into FFmpeg is not enough).
    pub fn probe(&self, enc: Encoder) -> bool {
        self.base()
            .args(self.global_args(enc))
            .args(["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30", "-frames:v", "3"])
            .args(self.video_args(enc, false))
            .args(["-f", "null", "-"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    pub fn pick_encoder(&self) -> Result<Encoder> {
        if self.encoder != Encoder::Auto {
            return Ok(self.encoder);
        }
        for &enc in Encoder::candidates() {
            if self.probe(enc) {
                return Ok(enc);
            }
        }
        bail!("no working H.264 encoder found in `{}`", self.ffmpeg)
    }

    /// Start capture + encode, writing an MPEG-TS stream to the child's stdout.
    pub fn spawn_recorder(&self, enc: Encoder) -> Result<Child> {
        let capture = self.resolved_capture();
        let gpu_frames = capture == Capture::Ddagrab && enc.takes_d3d11();
        let mut cmd = self.base();
        cmd.args(self.global_args(enc)).args(self.input_args(capture, enc));
        if let Some(input) = &self.audio_input {
            if let Some(format) = &self.audio_format {
                cmd.args(["-f", format]);
            }
            cmd.args(["-i", input]);
        }
        cmd.args(["-map", "0:v:0"]);
        if self.has_audio() {
            cmd.args(["-map", "1:a:0", "-c:a", "aac", "-b:a", &format!("{}k", self.audio_kbps)]);
        }
        cmd.args(self.video_args(enc, gpu_frames))
            // Flush every packet so the newest frames are in our buffer, not
            // in FFmpeg's, at the moment the user asks for a clip.
            // Constant frame rate: a frame captured too late is repeated and
            // counted as duplicated, which is how we detect capture falling
            // behind. Progress counters go to stderr once a second.
            .args(["-fps_mode", "cfr", "-progress", "pipe:2", "-stats_period", "1"])
            .args(["-f", "mpegts", "-flush_packets", "1", "-muxdelay", "0", "-muxpreload", "0", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.spawn().with_context(|| format!("could not start `{}`", self.ffmpeg))
    }

    /// Total stream bitrate in bits per second, for sizing the buffer.
    pub fn stream_bps(&self) -> u64 {
        let audio = if self.has_audio() { self.audio_kbps } else { 0 };
        (self.bitrate_kbps + audio) as u64 * 1000
    }
}

/// Wrap an MPEG-TS byte stream into an MP4 without re-encoding.
pub fn mux_to_mp4(ffmpeg: &str, ts: Vec<u8>, out: &Path) -> Result<()> {
    let part = out.with_extension("mp4.part");
    let mut child = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "mpegts", "-i", "pipe:0"])
        .args(["-c", "copy", "-movflags", "+faststart", "-f", "mp4"])
        .arg(&part)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not start `{ffmpeg}`"))?;

    // Feed stdin from its own thread so a chatty stderr cannot deadlock us.
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let writer = std::thread::spawn(move || stdin.write_all(&ts));
    let output = child.wait_with_output()?;
    let wrote = writer.join().expect("writer thread panicked");

    if !output.status.success() {
        let _ = std::fs::remove_file(&part);
        bail!("muxing failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    wrote.context("could not send clip data to the muxer")?;
    std::fs::rename(&part, out).with_context(|| format!("could not write {}", out.display()))
}
