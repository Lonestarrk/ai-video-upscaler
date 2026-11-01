//! Everything that is not user interface: settings, FFmpeg, the AI providers, the frame
//! pipeline (padding, prompts, consistency) and the export job.

pub mod aspect;
pub mod cache;
pub mod ffmpeg;
pub mod ffmpeg_download;
pub mod frame;
pub mod job;
pub mod paths;
pub mod process;
pub mod project;
pub mod prompt;
pub mod provider;
pub mod review;
pub mod settings;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
