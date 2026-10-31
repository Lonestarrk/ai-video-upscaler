//! Where the app keeps its settings, log and caches.

use std::path::PathBuf;

pub const APP_DIR_NAME: &str = "AI Video Upscaler";

/// `%APPDATA%\AI Video Upscaler` on Windows, `~/Library/Application Support/...` on macOS.
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_DIR_NAME)
}

/// `%LOCALAPPDATA%\AI Video Upscaler` on Windows: previews, export jobs, downloaded tools.
pub fn local_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_DIR_NAME)
}

pub fn settings_file() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn log_file() -> PathBuf {
    local_data_dir().join("upscaler.log")
}

/// FFmpeg downloaded by the app lands here (`bin/ffmpeg.exe`, `bin/ffprobe.exe`).
pub fn tools_dir() -> PathBuf {
    local_data_dir().join("tools")
}

/// The default work folder (previews and export jobs); the settings can move it.
pub fn default_work_dir() -> PathBuf {
    local_data_dir().join("work")
}
