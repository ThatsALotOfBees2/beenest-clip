//! `beenest-clip bench`: measures what recording costs on this machine.
//!
//! Phases, each `--duration` seconds:
//! 1. baseline: a synthetic CPU-bound "game" runs alone
//! 2. idle: the recorder runs alone (its own CPU and memory)
//! 3. load: the recorder and the game run together (the game's slowdown,
//!    and whether capture keeps up when the CPU is contended)
//! 4. saves: a few clips are saved and timed
//!
//! The recorder is this same binary started with `run`, so the numbers
//! include everything a user would actually run: our process plus FFmpeg.
//!
//! Not measured: GPU load and frame times of a real game. The synthetic game
//! only competes for CPU.

use std::hint::black_box;
use std::io::{BufRead, BufReader};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Args)]
pub struct BenchArgs {
    /// Seconds per measured phase.
    #[arg(short, long, default_value_t = 10)]
    duration: u64,
    /// Worker threads in the synthetic game; 0 skips the load phases.
    /// Defaults to one per logical CPU, so the CPU is fully contended.
    #[arg(long)]
    threads: Option<usize>,
    /// Number of clips to save in the last phase.
    #[arg(long, default_value_t = 3)]
    saves: u32,
    /// Write the results as JSON to this file.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Compare with an earlier `--json` result and exit with an error if
    /// anything got worse by more than `--tolerance`.
    #[arg(long)]
    baseline: Option<PathBuf>,
    /// Allowed regression, in percent, before `--baseline` fails.
    #[arg(long, default_value_t = 10.0)]
    tolerance: f64,
    /// Arguments passed to the recorder's `run`, after `--`.
    /// Example: `beenest-clip bench -- --capture test --encoder x264`
    #[arg(last = true)]
    run_args: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Report {
    /// The recorder's own description of its setup.
    recorder: String,
    cpus: usize,
    phase_secs: u64,
    idle: IdlePhase,
    load: Option<LoadPhase>,
    saves: Saves,
}

#[derive(Serialize, Deserialize)]
struct IdlePhase {
    /// Recorder CPU, percent of one core (100 = one core fully busy).
    cpu_avg: f64,
    cpu_p95: f64,
    /// Recorder CPU as a percent of the whole machine.
    cpu_machine: f64,
    rss_peak_mb: f64,
    encoder: EncoderPhase,
}

#[derive(Serialize, Deserialize)]
struct LoadPhase {
    threads: usize,
    baseline: FrameStats,
    recording: FrameStats,
    /// How much slower the game ran while recording, in percent.
    fps_loss_pct: f64,
    cpu_avg: f64,
    encoder: EncoderPhase,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
struct FrameStats {
    fps: f64,
    avg_ms: f64,
    p99_ms: f64,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
struct EncoderPhase {
    fps: f64,
    /// Frames repeated because capture was late, percent of frames encoded.
    dup_pct: f64,
    drop_pct: f64,
    speed: f64,
    bitrate_kbps: f64,
}

#[derive(Serialize, Deserialize)]
struct Saves {
    count: usize,
    min_ms: f64,
    median_ms: f64,
    max_ms: f64,
    clip_secs: f64,
    clip_mb: f64,
    /// Whether the last clip decoded without errors.
    decodes: bool,
}

pub fn run(ffmpeg: &str, args: BenchArgs) -> Result<()> {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    let threads = args.threads.unwrap_or(cpus);
    let secs = args.duration.max(1);
    let phase = Duration::from_secs(secs);
    let total = secs * (1 + threads.min(1) as u64 * 2) + args.saves as u64 + 10;
    eprintln!("benchmarking for about {total}s; keep the machine otherwise idle");

    let mut game = None;
    if threads > 0 {
        let work = calibrate();
        eprintln!("[1/4] game alone ({threads} threads)");
        game = Some((work, run_game(threads, work, phase)));
    }

    let out = std::env::temp_dir().join(format!("beenest-bench-{}", std::process::id()));
    let mut rec = Recorder::start(ffmpeg, &out, &args.run_args)?;
    let mut sampler = Sampler::new(rec.pid())?;
    // Let the encoder settle and give the CPU counters a first reading.
    std::thread::sleep(Duration::from_secs(3));
    sampler.find_children();
    sampler.sample();

    eprintln!("[2/4] recorder alone");
    let before = rec.stats()?;
    let (cpu, rss) = sampler.run_for(phase);
    let encoder = encoder_phase(&before, &rec.stats()?);
    let idle = IdlePhase {
        cpu_avg: mean(&cpu),
        cpu_p95: percentile(&cpu, 0.95),
        cpu_machine: mean(&cpu) / cpus as f64,
        rss_peak_mb: rss as f64 / 1e6,
        encoder,
    };

    let load = match game {
        Some((work, baseline)) => {
            eprintln!("[3/4] recorder and game together");
            let before = rec.stats()?;
            let handle = std::thread::spawn(move || run_game(threads, work, phase));
            let (cpu, _) = sampler.run_for(phase);
            let recording = handle.join().expect("game thread panicked");
            let encoder = encoder_phase(&before, &rec.stats()?);
            Some(LoadPhase {
                threads,
                baseline,
                recording,
                fps_loss_pct: (1.0 - recording.fps / baseline.fps) * 100.0,
                cpu_avg: mean(&cpu),
                encoder,
            })
        }
        None => None,
    };

    eprintln!("[4/4] saving {} clips", args.saves);
    let saves = rec.saves(args.saves.max(1), ffmpeg)?;
    let label = std::mem::take(&mut rec.label);
    drop(rec);
    let _ = std::fs::remove_dir_all(&out);

    let report = Report { recorder: label, cpus, phase_secs: secs, idle, load, saves };
    print_report(&report);

    if let Some(path) = &args.json {
        std::fs::write(path, serde_json::to_string_pretty(&report)? + "\n")
            .with_context(|| format!("could not write {}", path.display()))?;
        println!("\nwrote {}", path.display());
    }
    if let Some(path) = &args.baseline {
        let text = std::fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
        let old: Report = serde_json::from_str(&text).with_context(|| format!("{} is not a bench result", path.display()))?;
        compare(&old, &report, args.tolerance)?;
    }
    if !report.saves.decodes {
        bail!("the saved clip did not decode");
    }
    Ok(())
}

/// A recorder started as a child process, killed when dropped.
struct Recorder {
    child: Child,
    port: u16,
    out: PathBuf,
    /// The recorder's own description of its setup.
    label: String,
}

impl Recorder {
    fn start(ffmpeg: &str, out: &Path, run_args: &[String]) -> Result<Self> {
        let port = free_port()?;
        let mut child = Command::new(std::env::current_exe()?)
            .args(["--port", &port.to_string(), "--ffmpeg", ffmpeg, "run", "--hotkey", "none", "--out"])
            .arg(out)
            .args(run_args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("could not start the recorder")?;

        let stderr = child.stderr.take().expect("stderr was piped");
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("  recorder: {line}");
            }
        });
        let (tx, rx) = mpsc::channel();
        let stdout = child.stdout.take().expect("stdout was piped");
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        let mut rec = Recorder { child, port, out: out.to_path_buf(), label: String::new() };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(line) if line == "recording" => break,
                Ok(line) => {
                    if let Some(label) = line.strip_prefix("beenest-clip: ") {
                        eprintln!("recorder: {label}");
                        rec.label = label.to_string();
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => bail!("the recorder did not start recording within 30s"),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let status = rec.child.wait()?;
                    bail!("the recorder exited before recording ({status})");
                }
            }
        }
        // Keep draining stdout so the recorder never blocks on a full pipe.
        std::thread::spawn(move || for _ in rx {});
        Ok(rec)
    }

    fn pid(&self) -> Pid {
        Pid::from_u32(self.child.id())
    }

    fn stats(&self) -> Result<Stats> {
        Stats::parse(&crate::send(self.port, "stats")?)
    }

    /// Save `n` clips of the whole buffer a second apart, timing each one
    /// from the client's side, then check the last one decodes.
    fn saves(&self, n: u32, ffmpeg: &str) -> Result<Saves> {
        let mut times = Vec::new();
        let mut last = None;
        for i in 0..n {
            if i > 0 {
                std::thread::sleep(Duration::from_secs(1));
            }
            let start = Instant::now();
            let reply = crate::send(self.port, "save")?;
            times.push(start.elapsed().as_secs_f64() * 1000.0);
            last = Some(reply);
        }
        let reply = last.unwrap_or_default();
        // "saved PATH (6.2s, 6.4 MB, took 105 ms)"
        let path = reply
            .strip_prefix("saved ")
            .and_then(|r| r.rsplit_once(" ("))
            .map(|(p, _)| PathBuf::from(p))
            .with_context(|| format!("unexpected save reply: {reply}"))?;
        let path = if path.is_absolute() { path } else { self.out.join(path.file_name().unwrap_or_default()) };
        let (clip_secs, decodes) = probe_clip(ffmpeg, &path);
        let clip_mb = std::fs::metadata(&path).map_or(0.0, |m| m.len() as f64 / 1e6);
        times.sort_by(f64::total_cmp);
        Ok(Saves {
            count: times.len(),
            min_ms: times[0],
            median_ms: percentile(&times, 0.5),
            max_ms: times[times.len() - 1],
            clip_secs,
            clip_mb,
            decodes,
        })
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // FFmpeg exits by itself once its output pipe closes.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> Result<u16> {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("no free local port")?;
    Ok(l.local_addr()?.port())
}

/// Decode the whole clip, as a player would, and report its length.
fn probe_clip(ffmpeg: &str, path: &Path) -> (f64, bool) {
    let out = Command::new(ffmpeg)
        .args(["-hide_banner", "-v", "error", "-xerror", "-i"])
        .arg(path)
        .args(["-map", "0", "-f", "null", "-"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    let decodes = matches!(&out, Ok(o) if o.status.success() && o.stderr.is_empty());
    let secs = Command::new(ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(path)
        .output()
        .ok()
        .and_then(|o| {
            let text = String::from_utf8_lossy(&o.stderr).into_owned();
            let d = text.split("Duration: ").nth(1)?.split(',').next()?.to_string();
            let mut parts = d.trim().split(':').map(|p| p.parse::<f64>().ok());
            Some(parts.next()?? * 3600.0 + parts.next()?? * 60.0 + parts.next()??)
        })
        .unwrap_or(0.0);
    (secs, decodes)
}

/// Counters from the recorder's `stats` command.
struct Stats {
    at: Instant,
    frames: f64,
    dup: f64,
    drop: f64,
    speed: f64,
    total_bytes: f64,
}

impl Stats {
    fn parse(text: &str) -> Result<Self> {
        let mut s = Stats::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v: f64 = match v.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            match k {
                "frames" => s.frames = v,
                "dup_frames" => s.dup = v,
                "drop_frames" => s.drop = v,
                "speed" => s.speed = v,
                "total_bytes" => s.total_bytes = v,
                _ => {}
            }
        }
        Ok(s)
    }
}

impl Default for Stats {
    fn default() -> Self {
        Stats { at: Instant::now(), frames: 0.0, dup: 0.0, drop: 0.0, speed: 0.0, total_bytes: 0.0 }
    }
}

fn encoder_phase(a: &Stats, b: &Stats) -> EncoderPhase {
    let secs = b.at.duration_since(a.at).as_secs_f64().max(1e-3);
    let frames = (b.frames - a.frames).max(0.0);
    let pct = |n: f64| if frames > 0.0 { n.max(0.0) / frames * 100.0 } else { 0.0 };
    EncoderPhase {
        fps: frames / secs,
        dup_pct: pct(b.dup - a.dup),
        drop_pct: pct(b.drop - a.drop),
        speed: b.speed,
        bitrate_kbps: (b.total_bytes - a.total_bytes) * 8.0 / 1000.0 / secs,
    }
}

/// Samples the CPU and memory of the recorder and its FFmpeg children.
struct Sampler {
    sys: System,
    root: Pid,
    pids: Vec<Pid>,
}

impl Sampler {
    fn new(root: Pid) -> Result<Self> {
        let sys = System::new().context("cannot read process information on this system")?;
        Ok(Sampler { sys, root, pids: vec![root] })
    }

    fn kind() -> ProcessRefreshKind {
        ProcessRefreshKind::nothing().with_cpu().with_memory().with_exe(UpdateKind::Never)
    }

    /// Find the recorder's FFmpeg process. Done once, so steady-state
    /// sampling only touches the processes we care about.
    fn find_children(&mut self) {
        self.sys.refresh_processes_specifics(ProcessesToUpdate::All, true, Self::kind());
        self.pids = std::iter::once(self.root)
            .chain(self.sys.processes().iter().filter(|(_, p)| p.parent() == Some(self.root)).map(|(pid, _)| *pid))
            .collect();
    }

    /// Returns (CPU percent of one core, resident bytes), summed over the tree.
    fn sample(&mut self) -> (f64, u64) {
        self.sys.refresh_processes_specifics(ProcessesToUpdate::Some(&self.pids), true, Self::kind());
        self.pids.iter().filter_map(|p| self.sys.process(*p)).fold((0.0, 0), |(c, m), p| {
            (c + p.cpu_usage() as f64, m + p.memory())
        })
    }

    /// Sample twice a second for `d`; returns CPU samples and peak memory.
    fn run_for(&mut self, d: Duration) -> (Vec<f64>, u64) {
        let start = Instant::now();
        self.sample(); // reset the CPU counters to this phase
        let mut cpu = Vec::new();
        let mut peak = 0;
        while start.elapsed() < d {
            std::thread::sleep(Duration::from_millis(500));
            let (c, m) = self.sample();
            cpu.push(c);
            peak = peak.max(m);
        }
        (cpu, peak)
    }
}

/// One unit of the synthetic game's per-frame work.
fn spin(n: u64) -> u64 {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
    }
    black_box(x)
}

/// Pick a per-thread work size that makes one frame take about 8 ms on an
/// idle core (a 120 fps game).
fn calibrate() -> u64 {
    let target = 0.008;
    let mut n = 100_000u64;
    for _ in 0..4 {
        let t = Instant::now();
        spin(black_box(n));
        let took = t.elapsed().as_secs_f64().max(1e-6);
        n = ((n as f64) * target / took).clamp(1_000.0, 1e10) as u64;
    }
    n
}

/// Run the synthetic game: each frame, every thread does `work` units, and
/// the frame ends when the slowest thread finishes, like a game's job system.
fn run_game(threads: usize, work: u64, d: Duration) -> FrameStats {
    let start = Arc::new(Barrier::new(threads + 1));
    let end = Arc::new(Barrier::new(threads + 1));
    let stop = Arc::new(AtomicBool::new(false));
    let workers: Vec<_> = (0..threads)
        .map(|_| {
            let (start, end, stop) = (start.clone(), end.clone(), stop.clone());
            std::thread::spawn(move || loop {
                start.wait();
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                spin(work);
                end.wait();
            })
        })
        .collect();

    let began = Instant::now();
    let mut frames = Vec::new();
    while began.elapsed() < d {
        let t = Instant::now();
        start.wait();
        end.wait();
        frames.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let wall = began.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    start.wait();
    for w in workers {
        let _ = w.join();
    }

    let fps = frames.len() as f64 / wall;
    let avg_ms = mean(&frames);
    frames.sort_by(f64::total_cmp);
    FrameStats { fps, avg_ms, p99_ms: percentile(&frames, 0.99) }
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

/// `q`-quantile of `v` (sorted or not), nearest-rank.
fn percentile(v: &[f64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let i = ((q * s.len() as f64).ceil() as usize).clamp(1, s.len()) - 1;
    s[i]
}

fn print_report(r: &Report) {
    let enc = |e: &EncoderPhase| {
        format!(
            "{:.1} fps, {:.1}% duplicated, {:.1}% dropped, speed {:.2}x, {:.0} kbit/s",
            e.fps, e.dup_pct, e.drop_pct, e.speed, e.bitrate_kbps
        )
    };
    println!("\nbeenest-clip benchmark");
    println!("  recorder     {}", r.recorder);
    println!("  machine      {} logical CPUs, {}s per phase", r.cpus, r.phase_secs);

    let i = &r.idle;
    println!("\nRecorder alone");
    println!(
        "  CPU          {:.1}% of one core avg, {:.1}% p95  ({:.1}% of the machine)",
        i.cpu_avg, i.cpu_p95, i.cpu_machine
    );
    println!("  memory       {:.0} MB peak (recorder + FFmpeg, including the buffer)", i.rss_peak_mb);
    println!("  encoder      {}", enc(&i.encoder));

    if let Some(l) = &r.load {
        println!("\nWith a CPU-bound game on {} threads", l.threads);
        println!(
            "  game fps     {:.1} -> {:.1}  ({:+.1}%)",
            l.baseline.fps, l.recording.fps, -l.fps_loss_pct
        );
        println!("  frame time   avg {:.2} -> {:.2} ms, p99 {:.2} -> {:.2} ms",
            l.baseline.avg_ms, l.recording.avg_ms, l.baseline.p99_ms, l.recording.p99_ms);
        println!("  recorder CPU {:.1}% of one core", l.cpu_avg);
        println!("  encoder      {}", enc(&l.encoder));
    }

    let s = &r.saves;
    println!("\nSaves ({})", s.count);
    println!("  latency      min {:.0} ms, median {:.0} ms, max {:.0} ms", s.min_ms, s.median_ms, s.max_ms);
    println!(
        "  last clip    {:.1}s, {:.1} MB, {}",
        s.clip_secs,
        s.clip_mb,
        if s.decodes { "decodes cleanly" } else { "DECODE ERRORS" }
    );
}

/// Lower is better for every metric checked. A metric regresses only if it
/// is worse by more than `tolerance` percent *and* by more than a small
/// absolute amount, so noise on near-zero values does not fail the run.
fn compare(old: &Report, new: &Report, tolerance: f64) -> Result<()> {
    let mut rows: Vec<(&str, f64, f64, f64)> = vec![
        ("recorder CPU, % of one core", old.idle.cpu_avg, new.idle.cpu_avg, 2.0),
        ("recorder memory, MB", old.idle.rss_peak_mb, new.idle.rss_peak_mb, 10.0),
        ("duplicated frames, idle %", old.idle.encoder.dup_pct, new.idle.encoder.dup_pct, 1.0),
        ("save latency median, ms", old.saves.median_ms, new.saves.median_ms, 50.0),
    ];
    if let (Some(o), Some(n)) = (&old.load, &new.load) {
        rows.push(("game fps loss, %", o.fps_loss_pct, n.fps_loss_pct, 2.0));
        rows.push(("duplicated frames, load %", o.encoder.dup_pct, n.encoder.dup_pct, 1.0));
    }

    println!("\nCompared with baseline (tolerance {tolerance}%)");
    let mut failed = Vec::new();
    for (name, a, b, slack) in rows {
        let worse = b > a * (1.0 + tolerance / 100.0) && b - a > slack;
        let mark = if worse { "WORSE" } else { "ok" };
        println!("  {name:<30} {a:>9.2} -> {b:>9.2}  {mark}");
        if worse {
            failed.push(name);
        }
    }
    if !failed.is_empty() {
        bail!("regressed: {}", failed.join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_nearest_rank() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&v, 0.5), 50.0);
        assert_eq!(percentile(&v, 0.99), 99.0);
        assert_eq!(percentile(&v, 1.0), 100.0);
        assert_eq!(percentile(&[3.0, 1.0, 2.0], 0.0), 1.0);
        assert_eq!(percentile(&[], 0.5), 0.0);
    }

    #[test]
    fn encoder_phase_uses_deltas() {
        let t = Instant::now();
        let a = Stats { at: t, frames: 100.0, dup: 5.0, total_bytes: 0.0, ..Default::default() };
        let b = Stats { at: t + Duration::from_secs(10), frames: 700.0, dup: 11.0, total_bytes: 1_250_000.0, ..Default::default() };
        let e = encoder_phase(&a, &b);
        assert!((e.fps - 60.0).abs() < 1e-9);
        assert!((e.dup_pct - 1.0).abs() < 1e-9);
        assert!((e.bitrate_kbps - 1000.0).abs() < 1e-9);
    }
}
