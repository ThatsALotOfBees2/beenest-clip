//! Beenest Clip background app.
//!
//! Double-click to start: no console window. It runs `beenest-clip run`
//! (which must sit next to this executable) as a hidden child process with
//! settings from a config file, keeps a log, shows a notification for each
//! saved clip, and shows a tray icon with a menu on Windows and macOS.

// No console window on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod config;
#[cfg(any(windows, target_os = "macos"))]
mod tray;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use config::Config;

pub const APP_NAME: &str = "Beenest Clip";

/// Something the app has to react to. Sent from worker threads and the menu.
pub enum Event {
    /// A line the recorder printed. `err` is true for its stderr.
    Line { generation: u32, err: bool, text: String },
    /// The recorder exited. `ok` is true for a requested, clean stop.
    Exited { generation: u32, ok: bool, status: String },
    Action(Action),
    /// A save requested from the menu failed.
    SaveFailed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    SaveClip,
    OpenClips,
    OpenSettings,
    Restart,
    OpenLog,
    Quit,
}

pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

#[derive(Clone, PartialEq, Eq)]
pub enum Status {
    Starting,
    Recording,
    Stopped(String),
}

pub struct App {
    pub config: Config,
    pub status: Status,
    recorder: Option<Recorder>,
    generation: u32,
    log: Arc<Mutex<Option<File>>>,
    /// Recent stderr lines, shown when the recorder stops unexpectedly.
    recent_errors: VecDeque<String>,
    sink: Sink,
}

struct Recorder {
    child: Arc<Mutex<Child>>,
    port: u16,
}

impl App {
    pub fn new(sink: Sink) -> Self {
        let log = open_log(&log_path());
        App {
            config: Config::default(),
            status: Status::Starting,
            recorder: None,
            generation: 0,
            log: Arc::new(Mutex::new(log)),
            recent_errors: VecDeque::new(),
            sink,
        }
    }

    fn log(&self, line: &str) {
        log_to(&self.log, line);
    }

    /// Load settings and start recording. Errors end up in `status`.
    pub fn start(&mut self) {
        self.status = Status::Starting;
        match Config::load(&config::path()) {
            Ok(c) => self.config = c,
            Err(e) => {
                let msg = format!("{e:#}");
                self.log(&format!("settings: {msg}"));
                alert("Settings problem", &format!("{msg}\n\nUsing the previous settings."));
            }
        }
        if self.recorder_running() {
            let port = self.config.port;
            alert("Already running", &format!("{APP_NAME} is already running (something answers on port {port})."));
            // Exit through the normal event path, so the UI closes too.
            (self.sink)(Event::Action(Action::Quit));
            self.status = Status::Stopped("already running".into());
            return;
        }
        if let Err(e) = self.spawn() {
            let msg = format!("{e:#}");
            self.log(&format!("could not start: {msg}"));
            self.status = Status::Stopped(msg.clone());
            alert("Could not start recording", &msg);
        }
    }

    fn spawn(&mut self) -> Result<()> {
        let port = self.config.port;
        std::fs::create_dir_all(&self.config.clips_dir)
            .with_context(|| format!("could not create {}", self.config.clips_dir.display()))?;

        let exe = recorder_exe()?;
        let args = self.config.recorder_args();
        self.log(&format!("starting {} {}", exe.display(), args.join(" ")));
        let mut cmd = Command::new(&exe);
        cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        hide_window(&mut cmd);
        let mut child = cmd.spawn().with_context(|| format!("could not run {}", exe.display()))?;

        self.generation += 1;
        let generation = self.generation;
        self.recent_errors.clear();
        for (stream, err) in [
            (child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>), false),
            (child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>), true),
        ] {
            let Some(stream) = stream else { continue };
            let (sink, log) = (self.sink.clone(), self.log.clone());
            std::thread::spawn(move || {
                for text in BufReader::new(stream).lines().map_while(Result::ok) {
                    log_to(&log, &text);
                    sink(Event::Line { generation, err, text });
                }
            });
        }

        let child = Arc::new(Mutex::new(child));
        {
            let (child, sink) = (child.clone(), self.sink.clone());
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_millis(250));
                let done = child.lock().unwrap().try_wait();
                match done {
                    Ok(None) => continue,
                    Ok(Some(s)) => break sink(Event::Exited { generation, ok: s.success(), status: s.to_string() }),
                    Err(e) => break sink(Event::Exited { generation, ok: false, status: e.to_string() }),
                }
            });
        }
        self.recorder = Some(Recorder { child, port });
        Ok(())
    }

    /// Is a recorder (ours or another app's) answering on our port?
    fn recorder_running(&self) -> bool {
        let addr = (Ipv4Addr::LOCALHOST, self.config.port).into();
        TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
    }

    /// Ask the recorder to stop, and make sure it has.
    fn stop(&mut self) {
        let Some(rec) = self.recorder.take() else { return };
        self.generation += 1; // its exit is expected, so ignore it
        let _ = send(rec.port, "quit");
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut child = rec.child.lock().unwrap();
        while Instant::now() < deadline {
            if !matches!(child.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    /// React to one event. Returns false when the app should exit.
    pub fn handle(&mut self, event: Event) -> bool {
        match event {
            Event::Line { generation, .. } | Event::Exited { generation, .. } if generation != self.generation => {}
            Event::Line { err: false, text, .. } => {
                if text == "recording" {
                    self.status = Status::Recording;
                } else if let Some(rest) = text.strip_prefix("saved ") {
                    let file = rest
                        .rsplit_once(" (")
                        .map(|(p, _)| Path::new(p).file_name().unwrap_or_default().to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if self.config.notifications {
                        notify("Clip saved", &file);
                    }
                }
            }
            Event::Line { err: true, text, .. } => {
                if let Some(msg) = text.strip_prefix("save failed: ") {
                    notify("Clip not saved", msg);
                } else if text.starts_with("warning: ") && text.contains("hotkey") {
                    notify("Hotkey unavailable", text.trim_start_matches("warning: "));
                }
                self.recent_errors.push_back(text);
                if self.recent_errors.len() > 6 {
                    self.recent_errors.pop_front();
                }
            }
            Event::Exited { ok: true, .. } => {
                // Stopped on purpose, e.g. by `beenest-clip quit`.
                self.log("recorder stopped by request; exiting");
                return false;
            }
            Event::Exited { status, .. } => {
                self.recorder = None;
                let detail = self
                    .recent_errors
                    .iter()
                    .map(|l| l.trim_start_matches("error: "))
                    .collect::<Vec<_>>()
                    .join("\n");
                let msg = if detail.is_empty() { format!("The recorder stopped ({status}).") } else { detail };
                self.log(&format!("recorder exited: {status}"));
                self.status = Status::Stopped(msg.clone());
                let next = if cfg!(any(windows, target_os = "macos")) {
                    "Fix the problem, then choose \"Restart recorder\" from the tray menu."
                } else {
                    "Fix the problem, then start the app again."
                };
                alert("Recording stopped", &format!("{msg}\n\n{next}"));
            }
            Event::SaveFailed(msg) => notify("Clip not saved", &msg),
            Event::Action(action) => return self.act(action),
        }
        true
    }

    fn act(&mut self, action: Action) -> bool {
        match action {
            Action::SaveClip => {
                let Some(port) = self.recorder.as_ref().map(|r| r.port) else {
                    notify("Clip not saved", "Not recording right now.");
                    return true;
                };
                let sink = self.sink.clone();
                std::thread::spawn(move || {
                    if let Err(e) = send(port, "save") {
                        sink(Event::SaveFailed(format!("{e:#}")));
                    }
                });
            }
            Action::OpenClips => {
                let _ = std::fs::create_dir_all(&self.config.clips_dir);
                open(&self.config.clips_dir, false);
            }
            Action::OpenSettings => {
                let _ = Config::load(&config::path()); // creates it if missing
                open(&config::path(), true);
            }
            Action::OpenLog => open(&log_path(), true),
            Action::Restart => {
                self.stop();
                self.start();
            }
            Action::Quit => {
                self.stop();
                return false;
            }
        }
        true
    }

    pub fn tooltip(&self) -> String {
        match &self.status {
            Status::Starting => format!("{APP_NAME}: starting"),
            Status::Recording => format!(
                "{APP_NAME}: recording\n{} saves the last {} s",
                self.config.hotkey, self.config.seconds
            ),
            Status::Stopped(_) => format!("{APP_NAME}: stopped (see log)"),
        }
    }

    pub fn shutdown(&mut self) {
        self.stop();
    }
}

fn log_path() -> PathBuf {
    config::dir().join("beenest-clip.log")
}

/// Append to the log, starting afresh (keeping one old copy) once it is
/// over 1 MB. Appending means a second copy of the app that exits straight
/// away cannot wipe the running one's log.
fn open_log(path: &Path) -> Option<File> {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > 1_000_000) {
        let _ = std::fs::rename(path, path.with_extension("old.log"));
    }
    std::fs::OpenOptions::new().create(true).append(true).open(path).ok()
}

fn log_to(log: &Mutex<Option<File>>, line: &str) {
    if let Some(f) = log.lock().unwrap().as_mut() {
        let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(f, "{stamp}  {line}");
    }
}

/// The command-line recorder, expected next to this executable.
fn recorder_exe() -> Result<PathBuf> {
    let name = format!("beenest-clip{}", std::env::consts::EXE_SUFFIX);
    let exe = std::env::current_exe()?;
    let path = exe.parent().map(|d| d.join(&name)).unwrap_or_default();
    if !path.is_file() {
        bail!("{name} was not found next to {}. Keep both files in the same folder.", exe.display());
    }
    Ok(path)
}

fn hide_window(_cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        _cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

/// Send one command to the recorder's control port.
fn send(port: u16, command: &str) -> Result<String> {
    let mut conn = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).context("the recorder is not running")?;
    conn.set_read_timeout(Some(Duration::from_secs(30)))?;
    writeln!(conn, "{command}")?;
    let mut reply = String::new();
    conn.read_to_string(&mut reply)?;
    let reply = reply.trim_end();
    if let Some(msg) = reply.strip_prefix("error: ") {
        bail!("{msg}");
    }
    Ok(reply.to_string())
}

/// Open a folder in the file manager, or a text file in an editor.
fn open(path: &Path, text: bool) {
    let mut cmd = if cfg!(windows) {
        Command::new(if text { "notepad" } else { "explorer" })
    } else if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        if text {
            c.arg("-t");
        }
        c
    } else {
        Command::new("xdg-open")
    };
    let _ = cmd.arg(path).spawn();
}

/// A desktop notification. Failures are ignored: it is only feedback.
pub fn notify(title: &str, body: &str) {
    let (title, body) = (title.to_string(), body.to_string());
    // Some platforms block while showing it.
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .appname(APP_NAME)
            .summary(&title)
            .body(&body)
            .timeout(notify_rust::Timeout::Milliseconds(4000))
            .show();
    });
}

/// Something the user must see: a message box on Windows, a notification
/// elsewhere. Never blocks the caller.
pub fn alert(title: &str, body: &str) {
    #[cfg(windows)]
    {
        let (title, body) = (format!("{APP_NAME}: {title}"), body.to_string());
        std::thread::spawn(move || {
            use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONWARNING, MB_OK};
            let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
            let (t, b) = (wide(&title), wide(&body));
            unsafe { MessageBoxW(std::ptr::null_mut(), b.as_ptr(), t.as_ptr(), MB_OK | MB_ICONWARNING) };
        });
    }
    #[cfg(not(windows))]
    {
        eprintln!("{title}: {body}");
        notify(title, body);
    }
}

fn main() {
    let _ = std::fs::create_dir_all(config::dir());
    #[cfg(any(windows, target_os = "macos"))]
    tray::run();
    #[cfg(not(any(windows, target_os = "macos")))]
    headless();
}

/// Linux: no tray icon; run until the recorder is stopped
/// (`beenest-clip quit`) or this process is killed.
#[cfg(not(any(windows, target_os = "macos")))]
fn headless() {
    let (tx, rx) = std::sync::mpsc::channel();
    let tx = Mutex::new(tx);
    let mut app = App::new(Arc::new(move |e| {
        let _ = tx.lock().unwrap().send(e);
    }));
    app.start();
    eprintln!("{}", app.tooltip());
    if let Status::Stopped(why) = &app.status {
        if why != "already running" {
            std::process::exit(1);
        }
    }
    for event in rx {
        if !app.handle(event) {
            break;
        }
        if let Status::Stopped(_) = app.status {
            eprintln!("{}", app.tooltip());
            app.shutdown();
            std::process::exit(1);
        }
    }
    app.shutdown();
}
