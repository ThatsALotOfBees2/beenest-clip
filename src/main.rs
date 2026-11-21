mod bench;
mod ffmpeg;
mod hotkey;
mod ring;
mod ts;

use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use ffmpeg::{Capture, Encoder, Settings};
use ring::Ring;
use ts::{Scanner, PACKET, SYNC};

#[derive(Parser)]
#[command(version, about = "Low-overhead instant-replay clip tool")]
struct Cli {
    /// Local port the running recorder listens on for commands.
    #[arg(long, global = true, default_value_t = 47810)]
    port: u16,

    /// Path to the ffmpeg executable.
    #[arg(long, global = true, env = "BEENEST_FFMPEG", default_value = "ffmpeg")]
    ffmpeg: String,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Record into the replay buffer until stopped.
    Run(RunArgs),
    /// Ask the running recorder to save a clip.
    Save {
        /// Clip length in seconds (default: the whole buffer).
        #[arg(short, long)]
        seconds: Option<u64>,
    },
    /// Show what the running recorder is holding.
    Status,
    /// Stop the running recorder.
    Quit,
    /// List the encoders that work on this machine.
    Encoders,
    /// Measure what recording costs: CPU, memory, dropped frames, the
    /// slowdown of a CPU-bound workload, and save latency.
    Bench(bench::BenchArgs),
}

#[derive(Args)]
struct RunArgs {
    /// Length of the replay buffer in seconds.
    #[arg(short, long, default_value_t = 60)]
    seconds: u64,
    /// Directory clips are saved to.
    #[arg(short, long, default_value = "clips")]
    out: PathBuf,
    /// Global hotkey that saves a clip, or "none".
    #[arg(long, default_value = "alt+F10")]
    hotkey: String,
    #[arg(long, value_enum, default_value = "auto")]
    capture: Capture,
    #[arg(long, value_enum, default_value = "auto")]
    encoder: Encoder,
    #[arg(long, default_value_t = 60)]
    fps: u32,
    /// Video bitrate in kbit/s.
    #[arg(long, default_value_t = 12000)]
    bitrate: u32,
    /// Seconds between keyframes; clips start on a keyframe.
    #[arg(long, default_value_t = 2)]
    keyframe_secs: u32,
    /// X11 display (":0") or macOS screen device name.
    #[arg(long)]
    display: Option<String>,
    /// Render node used by the VAAPI encoder.
    #[arg(long, default_value = "/dev/dri/renderD128")]
    vaapi_device: String,
    /// FFmpeg audio input format, e.g. "pulse" or "dshow".
    #[arg(long)]
    audio_format: Option<String>,
    /// FFmpeg audio input, e.g. "default" or "audio=Stereo Mix". Enables audio.
    #[arg(long)]
    audio_input: Option<String>,
    /// Audio bitrate in kbit/s.
    #[arg(long, default_value_t = 160)]
    audio_bitrate: u32,
}

struct Buffer {
    ring: Ring,
    scanner: Scanner,
}

struct App {
    buffer: Mutex<Buffer>,
    /// Serialises saves so two triggers cannot write the same file.
    saving: Mutex<()>,
    ffmpeg: String,
    out: PathBuf,
    encoder: &'static str,
    buffer_secs: u64,
    started: Instant,
    progress: Mutex<Progress>,
}

/// Encoder counters FFmpeg reports about once a second.
#[derive(Default, Clone, Copy)]
struct Progress {
    frames: u64,
    dup_frames: u64,
    drop_frames: u64,
    speed: f32,
}

impl Progress {
    /// Take one `key=value` line of FFmpeg's `-progress` output.
    /// Returns false if the line is not progress output.
    fn update(&mut self, line: &str) -> bool {
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
            return false;
        }
        let value = value.trim();
        match key {
            "frame" => self.frames = value.parse().unwrap_or(self.frames),
            "dup_frames" => self.dup_frames = value.parse().unwrap_or(self.dup_frames),
            "drop_frames" => self.drop_frames = value.parse().unwrap_or(self.drop_frames),
            "speed" => self.speed = value.trim_end_matches('x').parse().unwrap_or(self.speed),
            _ => {}
        }
        true
    }
}

fn main() {
    let mut cli = Cli::parse();
    cli.ffmpeg = ffmpeg::locate(cli.ffmpeg);
    let result = match cli.command {
        Cmd::Run(args) => run(cli.ffmpeg, cli.port, args),
        Cmd::Save { seconds } => {
            let arg = seconds.map(|s| s.to_string()).unwrap_or_default();
            request(cli.port, &format!("save {arg}"))
        }
        Cmd::Status => request(cli.port, "status"),
        Cmd::Quit => request(cli.port, "quit"),
        Cmd::Encoders => encoders(cli.ffmpeg),
        Cmd::Bench(args) => bench::run(&cli.ffmpeg, args),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn settings(ffmpeg: String, a: &RunArgs) -> Settings {
    Settings {
        ffmpeg,
        capture: a.capture,
        encoder: a.encoder,
        fps: a.fps.max(1),
        bitrate_kbps: a.bitrate,
        gop_secs: a.keyframe_secs.max(1),
        display: a.display.clone(),
        vaapi_device: a.vaapi_device.clone(),
        audio_format: a.audio_format.clone(),
        audio_input: a.audio_input.clone(),
        audio_kbps: a.audio_bitrate,
    }
}

fn run(ffmpeg: String, port: u16, args: RunArgs) -> Result<()> {
    let cfg = settings(ffmpeg, &args);

    // Claim the control port first: it doubles as the single-instance lock.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .with_context(|| format!("port {port} is taken; is beenest-clip already running?"))?;
    std::fs::create_dir_all(&args.out)
        .with_context(|| format!("could not create {}", args.out.display()))?;

    let capture = cfg.resolved_capture();
    if capture == Capture::X11grab && std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v == "wayland") {
        eprintln!("warning: this is a Wayland session; X11 capture will only see XWayland windows");
    }
    ffmpeg::check_installed(&cfg.ffmpeg)?;
    let encoder = cfg.pick_encoder()?;

    // Sized for the buffer plus one keyframe interval (a clip starts on the
    // keyframe before the cut-off), with headroom for container overhead.
    let secs = args.seconds.max(1);
    let capacity = cfg.stream_bps() / 8 * (secs + cfg.gop_secs as u64) * 5 / 4;
    let app = Arc::new(App {
        buffer: Mutex::new(Buffer {
            ring: Ring::new(capacity as usize, Duration::from_secs(secs)),
            scanner: Scanner::new(),
        }),
        saving: Mutex::new(()),
        ffmpeg: cfg.ffmpeg.clone(),
        out: args.out.clone(),
        encoder: encoder.name(),
        buffer_secs: secs,
        started: Instant::now(),
        progress: Mutex::new(Progress::default()),
    });

    let mut child = cfg.spawn_recorder(encoder)?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    {
        let app = app.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if !app.progress.lock().unwrap().update(&line) {
                    eprintln!("ffmpeg: {line}");
                }
            }
        });
    }

    println!(
        "beenest-clip: {capture:?} capture, {} at {} fps / {} kbit/s, {secs}s buffer ({} MB)",
        encoder.name(),
        cfg.fps,
        cfg.bitrate_kbps,
        capacity / 1_000_000
    );

    {
        let app = app.clone();
        std::thread::spawn(move || {
            let outcome = record(&app, stdout);
            let status = child.wait().map(|s| s.to_string()).unwrap_or_default();
            match outcome {
                Ok(()) => eprintln!("error: capture stopped ({status})"),
                Err(e) => eprintln!("error: capture failed: {e:#} ({status})"),
            }
            std::process::exit(1);
        });
    }
    {
        let app = app.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let app = app.clone();
                std::thread::spawn(move || serve(&app, conn));
            }
        });
    }
    if std::io::stdin().is_terminal() {
        let app = app.clone();
        std::thread::spawn(move || {
            for _ in std::io::stdin().lines().map_while(Result::ok) {
                report(save(&app, None));
            }
        });
        println!("press Enter here to save a clip, Ctrl+C to quit");
    }

    if args.hotkey.eq_ignore_ascii_case("none") {
        park();
    }
    println!("hotkey: {}", args.hotkey);
    let on_press = {
        let app = app.clone();
        move || {
            let app = app.clone();
            std::thread::spawn(move || report(save(&app, None)));
        }
    };
    if let Err(e) = hotkey::run(&args.hotkey, on_press) {
        eprintln!("warning: {e:#}; use `beenest-clip save` instead");
    }
    park()
}

fn park() -> ! {
    loop {
        std::thread::park();
    }
}

/// Pump the encoded stream from FFmpeg into the ring, noting keyframes.
fn record(app: &App, mut stream: impl Read) -> Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut filled = 0;
    let mut announced = false;
    loop {
        let n = stream.read(&mut buf[filled..])?;
        if n == 0 {
            return Ok(());
        }
        filled += n;
        let now = Instant::now();

        let mut off = 0;
        let mut guard = app.buffer.lock().unwrap();
        let Buffer { ring, scanner } = &mut *guard;
        while filled - off >= PACKET {
            if buf[off] != SYNC {
                off += 1; // lost alignment; hunt for the next packet start
                continue;
            }
            let pkt: &[u8; PACKET] = buf[off..off + PACKET].try_into().unwrap();
            if scanner.scan(pkt) {
                ring.mark(now);
                if !announced {
                    announced = true;
                    println!("recording");
                }
            }
            ring.push(pkt);
            off += PACKET;
        }
        drop(guard);

        buf.copy_within(off..filled, 0);
        filled -= off;
    }
}

/// Save the last `seconds` (or the whole buffer) as an MP4.
fn save(app: &App, seconds: Option<u64>) -> Result<String> {
    let _one_at_a_time = app.saving.lock().unwrap();
    let began = Instant::now();

    // Hold the buffer lock only for the copy; muxing happens outside it.
    let (tables, data, clip_start) = {
        let b = app.buffer.lock().unwrap();
        let wanted = Duration::from_secs(seconds.unwrap_or(app.buffer_secs));
        let since = began.checked_sub(wanted).unwrap_or(app.started);
        let tables = b.scanner.tables();
        let snap = b.ring.snapshot(since);
        match (tables, snap) {
            (Some(t), Some((d, at))) => (t, d, at),
            _ => bail!("nothing recorded yet"),
        }
    };
    let length = began.duration_since(clip_start).as_secs_f32();
    let size = data.len();

    let mut ts = Vec::with_capacity(tables.len() + size);
    ts.extend_from_slice(&tables);
    ts.extend_from_slice(&data);
    drop(data);

    let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let mut path = app.out.join(format!("clip_{stamp}.mp4"));
    let mut n = 2;
    while path.exists() {
        path = app.out.join(format!("clip_{stamp}_{n}.mp4"));
        n += 1;
    }
    ffmpeg::mux_to_mp4(&app.ffmpeg, ts, &path)?;

    Ok(format!(
        "saved {} ({length:.1}s, {:.1} MB, took {} ms)",
        path.display(),
        size as f64 / 1e6,
        began.elapsed().as_millis()
    ))
}

fn report(result: Result<String>) {
    match result {
        Ok(msg) => println!("{msg}"),
        Err(e) => eprintln!("save failed: {e:#}"),
    }
}

fn status(app: &App) -> String {
    let now = Instant::now();
    let s = app.buffer.lock().unwrap().ring.stats(now);
    let p = *app.progress.lock().unwrap();
    let uptime = now.duration_since(app.started).as_secs_f64();
    let kbps = if s.buffered.as_secs_f64() > 0.0 {
        s.bytes as f64 * 8.0 / 1000.0 / s.buffered.as_secs_f64()
    } else {
        0.0
    };
    format!(
        "encoder: {}\nuptime: {uptime:.0}s\nbuffered: {:.1}s in {} keyframe segments\n\
         buffer use: {:.1} of {:.1} MB\nmeasured bitrate: {kbps:.0} kbit/s\ntotal encoded: {:.1} MB\n\
         frames encoded: {} ({} duplicated, {} dropped), speed {:.2}x",
        app.encoder,
        s.buffered.as_secs_f64(),
        s.segments,
        s.bytes as f64 / 1e6,
        s.capacity as f64 / 1e6,
        s.total_written as f64 / 1e6,
        p.frames,
        p.dup_frames,
        p.drop_frames,
        p.speed,
    )
}

/// The same figures as `status`, as `key=value` lines for tools.
fn stats(app: &App) -> String {
    let now = Instant::now();
    let s = app.buffer.lock().unwrap().ring.stats(now);
    let p = *app.progress.lock().unwrap();
    format!(
        "encoder={}\nuptime_secs={:.3}\nbuffered_secs={:.3}\nbuffer_bytes={}\ncapacity_bytes={}\n\
         total_bytes={}\nframes={}\ndup_frames={}\ndrop_frames={}\nspeed={}",
        app.encoder,
        now.duration_since(app.started).as_secs_f64(),
        s.buffered.as_secs_f64(),
        s.bytes,
        s.capacity,
        s.total_written,
        p.frames,
        p.dup_frames,
        p.drop_frames,
        p.speed,
    )
}

/// Handle one control connection: a single command line, a single reply.
fn serve(app: &App, mut conn: TcpStream) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
    let mut line = String::new();
    if BufReader::new(&conn).take(64).read_line(&mut line).is_err() {
        return;
    }
    let mut words = line.split_whitespace();
    let reply = match words.next() {
        Some("save") => match words.next().map(str::parse::<u64>) {
            Some(Err(_)) => "error: seconds must be a whole number".to_string(),
            secs => match save(app, secs.and_then(Result::ok)) {
                Ok(msg) => {
                    println!("{msg}");
                    msg
                }
                Err(e) => format!("error: {e:#}"),
            },
        },
        Some("status") => status(app),
        Some("stats") => stats(app),
        Some("quit") => {
            let _ = writeln!(conn, "stopping");
            let _ = conn.flush();
            println!("stopped by request");
            // FFmpeg exits by itself once its output pipe closes.
            std::process::exit(0);
        }
        _ => "error: unknown command".to_string(),
    };
    let _ = writeln!(conn, "{reply}");
}

/// Client side: send a command to the running recorder and return the reply.
pub(crate) fn send(port: u16, command: &str) -> Result<String> {
    let mut conn = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .context("beenest-clip is not running (start it with `beenest-clip run`)")?;
    writeln!(conn, "{command}")?;
    let mut reply = String::new();
    conn.read_to_string(&mut reply)?;
    let reply = reply.trim_end();
    if let Some(msg) = reply.strip_prefix("error: ") {
        bail!("{msg}");
    }
    Ok(reply.to_string())
}

fn request(port: u16, command: &str) -> Result<()> {
    println!("{}", send(port, command)?);
    Ok(())
}

fn encoders(ffmpeg: String) -> Result<()> {
    let cfg = Settings {
        ffmpeg,
        capture: Capture::Test,
        encoder: Encoder::Auto,
        fps: 30,
        bitrate_kbps: 4000,
        gop_secs: 2,
        display: None,
        vaapi_device: "/dev/dri/renderD128".into(),
        audio_format: None,
        audio_input: None,
        audio_kbps: 0,
    };
    let mut chosen = false;
    for &enc in Encoder::candidates() {
        let ok = cfg.probe(enc);
        let note = match (ok, chosen) {
            (true, false) => "works (would be used)",
            (true, true) => "works",
            (false, _) => "unavailable",
        };
        chosen |= ok;
        println!("{:<20} {note}", enc.name());
    }
    if !chosen {
        bail!("no working H.264 encoder found");
    }
    Ok(())
}
