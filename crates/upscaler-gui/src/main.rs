// Keep a console in debug builds so panics are visible.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use upscaler_core::paths;
use upscaler_core::settings::Settings;
use upscaler_gui::app::UpscalerApp;
use upscaler_gui::i18n;

const ICON_PNG: &[u8] = include_bytes!("../assets/icon-256.png");

fn init_logging() {
    let path = paths::log_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Keep the log small: start over when it passes 5 MB.
    if std::fs::metadata(&path)
        .map(|m| m.len() > 5 << 20)
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(&path);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);
    let config = simplelog::ConfigBuilder::new()
        .set_time_format_rfc3339()
        .build();
    let mut loggers: Vec<Box<dyn simplelog::SharedLogger>> = Vec::new();
    if let Ok(f) = file {
        loggers.push(simplelog::WriteLogger::new(
            log::LevelFilter::Info,
            config.clone(),
            f,
        ));
    }
    if cfg!(debug_assertions) {
        loggers.push(simplelog::SimpleLogger::new(log::LevelFilter::Info, config));
    }
    let _ = simplelog::CombinedLogger::init(loggers);
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("panic: {info}");
        default(info);
    }));
}

fn main() -> eframe::Result {
    init_logging();
    log::info!("AI Video Upscaler {} starting", upscaler_core::version());
    let settings = Settings::load();
    // A video given on the command line (or through "Open with" / dropped on the exe) opens at start.
    let initial = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_file());
    i18n::apply(&settings.language);
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1500.0, 920.0])
        .with_min_inner_size([900.0, 600.0])
        .with_title("AI Video Upscaler")
        .with_drag_and_drop(true);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(ICON_PNG) {
        viewport = viewport.with_icon(icon);
    }
    // wgpu (DirectX 12 / Metal) on Windows and macOS; OpenGL elsewhere, or when asked for
    // with UPSCALER_RENDERER=glow (old GPUs and virtual machines).
    let glow = !cfg!(any(windows, target_os = "macos"))
        || std::env::var("UPSCALER_RENDERER").is_ok_and(|v| v == "glow");
    let renderer = if glow {
        eframe::Renderer::Glow
    } else {
        eframe::Renderer::Wgpu
    };
    // wgpu otherwise picks Vulkan, which crashed with a validation error on an AMD RX 7900 XTX.
    if cfg!(windows) && std::env::var_os("WGPU_BACKEND").is_none() {
        std::env::set_var("WGPU_BACKEND", "dx12");
    }
    let options = eframe::NativeOptions {
        viewport,
        renderer,
        ..Default::default()
    };
    eframe::run_native(
        "AI Video Upscaler",
        options,
        Box::new(move |cc| Ok(Box::new(UpscalerApp::new(cc, settings, initial)))),
    )
}
