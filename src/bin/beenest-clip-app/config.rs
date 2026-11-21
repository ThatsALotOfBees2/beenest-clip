//! Settings file for the background app.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub seconds: u64,
    pub hotkey: String,
    pub clips_dir: PathBuf,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub encoder: String,
    pub capture: String,
    pub audio_format: Option<String>,
    pub audio_input: Option<String>,
    pub port: u16,
    pub notifications: bool,
    pub extra_args: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            seconds: 30,
            hotkey: "alt+F10".into(),
            clips_dir: default_clips_dir(),
            fps: 60,
            bitrate_kbps: 12000,
            encoder: "auto".into(),
            capture: "auto".into(),
            audio_format: None,
            audio_input: None,
            port: 47810,
            notifications: true,
            extra_args: Vec::new(),
        }
    }
}

fn default_clips_dir() -> PathBuf {
    dirs::video_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Beenest Clips")
}

pub fn dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("beenest-clip")
}

pub fn path() -> PathBuf {
    dir().join("config.toml")
}

impl Config {
    /// Read the settings file, creating it with defaults on first run.
    pub fn load(path: &Path) -> Result<Config> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
            }
            std::fs::write(path, template())
                .with_context(|| format!("could not write {}", path.display()))?;
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("{} has a mistake in it", path.display()))
    }

    /// Arguments for `beenest-clip run`, plus the global ones before it.
    pub fn recorder_args(&self) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "--port".into(),
            self.port.to_string(),
            "run".into(),
            "--seconds".into(),
            self.seconds.to_string(),
            "--hotkey".into(),
            self.hotkey.clone(),
            "--out".into(),
            self.clips_dir.to_string_lossy().into_owned(),
            "--fps".into(),
            self.fps.to_string(),
            "--bitrate".into(),
            self.bitrate_kbps.to_string(),
            "--encoder".into(),
            self.encoder.clone(),
            "--capture".into(),
            self.capture.clone(),
        ];
        if let Some(f) = &self.audio_format {
            a.extend(["--audio-format".into(), f.clone()]);
        }
        if let Some(i) = &self.audio_input {
            a.extend(["--audio-input".into(), i.clone()]);
        }
        a.extend(self.extra_args.iter().cloned());
        a
    }
}

/// The file written on first run: every setting, with its default and a
/// line saying what it does.
fn template() -> String {
    let d = Config::default();
    // toml's own quoting handles backslashes in Windows paths.
    let clips = toml::Value::String(d.clips_dir.to_string_lossy().into_owned());
    let (audio_example_format, audio_example_input) = if cfg!(windows) {
        ("dshow", "audio=Stereo Mix (Realtek(R) Audio)")
    } else if cfg!(target_os = "macos") {
        ("avfoundation", ":0")
    } else {
        ("pulse", "@DEFAULT_MONITOR@")
    };
    format!(
        r#"# Beenest Clip settings. After changing this file, choose "Restart
# recorder" from the tray menu (or restart the app) for it to take effect.

# How many seconds a clip holds: the hotkey saves the last this-many seconds.
seconds = {seconds}

# Key combination that saves a clip, e.g. "alt+F10", "ctrl+shift+S", "F8".
hotkey = "{hotkey}"

# Where clips are saved.
clips_dir = {clips}

# Frames per second and video bitrate. Higher looks better but costs more.
fps = {fps}
bitrate_kbps = {bitrate}

# Video encoder: "auto" picks the best working one. Others: "nvenc" (NVIDIA),
# "amf" (AMD), "qsv" (Intel), "vaapi", "videotoolbox" (Mac), "x264" (CPU).
encoder = "auto"

# Screen capture method: "auto" picks the best one for this system.
capture = "auto"

# Audio is off until you name an FFmpeg audio input. Example for this system:
# audio_format = "{audio_example_format}"
# audio_input = "{audio_example_input}"

# Show a notification when a clip is saved.
notifications = true

# Local port the recorder listens on (for `beenest-clip save`).
port = {port}

# Extra arguments passed to `beenest-clip run`, for anything not above.
extra_args = []
"#,
        seconds = d.seconds,
        hotkey = d.hotkey,
        fps = d.fps,
        bitrate = d.bitrate_kbps,
        port = d.port,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_to_defaults() {
        let c: Config = toml::from_str(&template()).unwrap();
        let d = Config::default();
        assert_eq!(c.seconds, d.seconds);
        assert_eq!(c.hotkey, d.hotkey);
        assert_eq!(c.clips_dir, d.clips_dir);
        assert!(c.audio_input.is_none());
    }

    #[test]
    fn missing_keys_use_defaults_and_typos_are_errors() {
        let c: Config = toml::from_str("seconds = 15").unwrap();
        assert_eq!(c.seconds, 15);
        assert_eq!(c.fps, 60);
        assert!(toml::from_str::<Config>("secconds = 15").is_err());
    }

    #[test]
    fn recorder_args_include_audio_only_when_set() {
        let mut c = Config::default();
        assert!(!c.recorder_args().contains(&"--audio-input".to_string()));
        c.audio_input = Some("default".into());
        let a = c.recorder_args();
        assert!(a.windows(2).any(|w| w[0] == "--audio-input" && w[1] == "default"));
        assert_eq!(&a[..3], ["--port", "47810", "run"]);
    }
}
