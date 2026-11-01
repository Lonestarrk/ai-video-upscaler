//! Audio: the side panel's "Ljud" section, the settings tab "Ljud-AI" (the local AI audio
//! pack) and the Swedish/English texts for the audio job's stages and warnings.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use eframe::egui::{self, Color32};
use upscaler_core::audio::ai::{
    Device, PACK_MISSING, PACK_MISSING_BOTH, PACK_MISSING_ENHANCE, PACK_MISSING_SEPARATION,
};
use upscaler_core::audio::job::{self as audio_job, AudioStage, Kind};
use upscaler_core::audio::pack;
use upscaler_core::audio::source::SourceAudio;
use upscaler_core::settings::{AudioMode, LfeLevel, SurroundEngine};

use super::style::{primary_button, section, BAD, GOOD, WARN};
use crate::app::UpscalerApp;
use crate::i18n::{english, format_bytes, format_duration, tr};
use crate::worker;

/// The installed pack's size on disk (about 5.7 GB with the GPU torch), for the install button.
pub const PACK_SIZE_BYTES: u64 = 5_700_000_000;

/// Rough local-AI separation speed on a CPU: seconds of work per minute of audio (measured:
/// 250 s on the CPU against about 20 s on the GPU).
pub const CPU_SECONDS_PER_AUDIO_MINUTE: f64 = 250.0;
/// Enhancement (Apollo) per pass on a CPU, seconds per minute of audio. Measured only on the
/// GPU (about 42 s per minute and pass); assumed to slow down on the CPU as much as the
/// separation does (250 / 20), so about 525 s. A 5.1 result takes three passes (fronts,
/// centre, surrounds), stereo and mono one.
pub const CPU_ENHANCE_SECONDS_PER_AUDIO_MINUTE: f64 = 42.0 * 250.0 / 20.0;

/// The AI audio pack exists for Windows only (AMD ROCm torch wheels); elsewhere only the
/// classic engine is offered.
pub fn ai_supported() -> bool {
    cfg!(target_os = "windows")
}

/// What the source audio is, as far as the UI knows.
#[derive(Clone, Copy)]
enum Source<'a> {
    /// No video open.
    NoVideo,
    Reading,
    Failed(&'a str),
    None,
    Some(&'a SourceAudio),
}

fn source_of(app: &UpscalerApp) -> Source<'_> {
    match &app.video {
        None => Source::NoVideo,
        Some(v) => match &v.audio {
            None => Source::Reading,
            Some(Err(e)) => Source::Failed(e),
            Some(Ok(None)) => Source::None,
            Some(Ok(Some(a))) => Source::Some(a),
        },
    }
}

fn codec_name(c: &str) -> String {
    match c {
        "ac3" => "AC-3".into(),
        "eac3" => "E-AC-3".into(),
        "aac" => "AAC".into(),
        "mp3" => "MP3".into(),
        "mp2" => "MP2".into(),
        "flac" => "FLAC".into(),
        "opus" => "Opus".into(),
        "vorbis" => "Vorbis".into(),
        c if c.starts_with("pcm") => "PCM".into(),
        c => c.to_uppercase(),
    }
}

fn describe(a: &SourceAudio) -> String {
    let layout = match a.channels {
        1 => tr("Mono", "Mono").to_string(),
        2 if a.dolby_surround => tr(
            "Stereo med surroundmatris (flaggad i AC-3)",
            "Stereo with a surround matrix (flagged in AC-3)",
        )
        .to_string(),
        2 => tr("Stereo", "Stereo").to_string(),
        6 => "5.1".to_string(),
        n => format!("{n} {}", tr("kanaler", "channels")),
    };
    format!("{layout}, {}", codec_name(&a.codec))
}

fn engine_label(e: SurroundEngine) -> &'static str {
    match e {
        SurroundEngine::LocalAi => tr("AI (lokal, grafikkort)", "AI (local, GPU)"),
        SurroundEngine::Classic => tr("Klassisk (utan AI)", "Classic (no AI)"),
    }
}

fn lfe_label(l: LfeLevel) -> &'static str {
    match l {
        LfeLevel::Subtle => tr("Subtil", "Subtle"),
        LfeLevel::Normal => tr("Normal", "Normal"),
        LfeLevel::Strong => tr("Kraftig", "Strong"),
        LfeLevel::Cinema => tr("Biokänsla", "Cinema feel"),
    }
}

/// What the local AI would do in an export started now: separate, and how many
/// enhancement passes. Judged from the plan when the source is known, else from the settings.
/// When the settings use no AI at all (the settings tab), the separation stands in.
fn ai_work(app: &UpscalerApp) -> (bool, u32) {
    let s = &app.settings;
    let passes = |channels: usize| match (s.audio_enhance, channels) {
        (false, _) => 0,
        (true, 6) => 3,
        (true, _) => 1,
    };
    let planned = match source_of(app) {
        Source::Some(a) => audio_job::plan_job(s, Some(a), true).ok(),
        _ => None,
    };
    let work = match planned {
        Some(p) => (p.kind == Kind::UpmixAi, passes(p.channels())),
        None => {
            let surround = s.audio_mode == AudioMode::Surround;
            (
                surround && s.surround_engine == SurroundEngine::LocalAi,
                passes(if surround { 6 } else { 2 }),
            )
        }
    };
    if work == (false, 0) {
        (true, 0)
    } else {
        work
    }
}

/// CPU seconds per minute of audio for that work.
fn cpu_seconds_per_minute((separate, passes): (bool, u32)) -> f64 {
    let sep = if separate {
        CPU_SECONDS_PER_AUDIO_MINUTE
    } else {
        0.0
    };
    sep + passes as f64 * CPU_ENHANCE_SECONDS_PER_AUDIO_MINUTE
}

/// "the CPU – about N h per episode" for a video of `duration` seconds and `per_minute`
/// seconds of work per minute of audio.
fn cpu_estimate(duration: Option<f64>, per_minute: f64) -> String {
    match duration.filter(|d| *d > 0.0) {
        Some(d) => {
            let secs = d / 60.0 * per_minute;
            if secs >= 3600.0 {
                format!(
                    "{} {:.0} {}",
                    tr("processorn – cirka", "the CPU – about"),
                    (secs / 3600.0).round(),
                    tr("tim per avsnitt", "h per episode")
                )
            } else {
                format!(
                    "{} {:.0} {}",
                    tr("processorn – cirka", "the CPU – about"),
                    (secs / 60.0).round().max(1.0),
                    tr("min för den här videon", "min for this video")
                )
            }
        }
        None => format!(
            "{} {:.0} {}",
            tr("processorn – cirka", "the CPU – about"),
            (per_minute / 60.0).round().max(1.0),
            tr("tim per timme ljud", "h per hour of audio")
        ),
    }
}

/// Where the local AI runs, from the cached pack status. `None` when the pack is missing.
fn device_line(app: &UpscalerApp) -> Option<(Color32, String, Option<String>)> {
    let Some(st) = &app.audio_pack.status else {
        return Some((
            Color32::GRAY,
            tr("Kontrollerar ljud-AI…", "Checking the audio AI…").into(),
            None,
        ));
    };
    match &st.device {
        None => None,
        Some(Device::Gpu(name)) => Some((
            GOOD,
            format!("{} {name}", tr("Körs på grafikkortet:", "Runs on the GPU:")),
            None,
        )),
        Some(Device::Cpu) => Some((
            WARN,
            format!(
                "{} {}",
                tr("Körs på", "Runs on"),
                cpu_estimate(
                    app.video.as_ref().map(|v| v.info.duration),
                    cpu_seconds_per_minute(ai_work(app))
                )
            ),
            st.cpu_reason.clone(),
        )),
    }
}

/// The side panel's "Ljud" section.
pub fn side_section(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    section(ui, tr("Ljud", "Audio"));
    let src = source_of(app);
    let (mono, surround_src) = match src {
        Source::Some(a) => (a.channels == 1, a.is_surround()),
        _ => (false, false),
    };
    let reading = matches!(src, Source::Reading);
    let failed = matches!(src, Source::Failed(_));
    let known: Option<SourceAudio> = match src {
        Source::Some(a) => Some(a.clone()),
        _ => None,
    };
    match src {
        Source::NoVideo => {}
        Source::Reading => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    egui::RichText::new(tr("Läser ljudspåret…", "Reading the audio track…"))
                        .weak()
                        .small(),
                );
            });
        }
        Source::Failed(e) => {
            ui.label(
                egui::RichText::new(tr(
                    "Ljudspåret kunde inte läsas.",
                    "The audio track could not be read.",
                ))
                .small()
                .color(BAD),
            )
            .on_hover_text(e.to_string());
        }
        Source::None => {
            // Nothing to choose: the saved choices stay for the next video.
            ui.label(
                egui::RichText::new(tr("Videon har inget ljud.", "The video has no audio."))
                    .weak()
                    .small(),
            );
            return;
        }
        Source::Some(a) => {
            ui.label(
                egui::RichText::new(format!("{} {}", tr("Källa:", "Source:"), describe(a)))
                    .weak()
                    .small(),
            );
        }
    }

    // Choices the source or the platform rule out are corrected, so the export never
    // starts with settings it would refuse.
    let s = &mut app.settings;
    if mono && s.audio_mode == AudioMode::Surround {
        s.audio_mode = AudioMode::Original;
    }
    if !ai_supported() {
        s.surround_engine = SurroundEngine::Classic;
        s.audio_enhance = false;
    }
    if s.surround_engine == SurroundEngine::Classic && s.lfe_level == LfeLevel::Cinema {
        s.lfe_level = LfeLevel::Strong;
    }

    let locked = app.exporting();
    ui.add_enabled_ui(!locked, |ui| {
        let s = &mut app.settings;
        let mut mode = s.audio_mode;
        ui.horizontal(|ui| {
            ui.radio_value(&mut mode, AudioMode::Original, tr("Oförändrat", "Unchanged"))
                .on_hover_text(tr(
                    "Originalljudet kopieras som det är (MP4).",
                    "The original audio is copied as it is (MP4).",
                ));
            if ui
                .add_enabled(!mono, egui::RadioButton::new(mode == AudioMode::Surround, "5.1"))
                .on_hover_text(tr(
                    "Gör riktigt 5.1 av stereoljudet: dialog i mitten, musik och effekter runt om, bas i LFE.",
                    "Makes real 5.1 from the stereo: dialogue in the centre, music and effects around, bass in the LFE.",
                ))
                .on_disabled_hover_text(tr(
                    "Källan är mono: 5.1 kan inte göras av en enda kanal.",
                    "The source is mono: 5.1 cannot be made from a single channel.",
                ))
                .clicked()
            {
                mode = AudioMode::Surround;
            }
        });
        if mono {
            ui.label(
                egui::RichText::new(tr(
                    "5.1 går inte: källan är mono.",
                    "5.1 is not possible: the source is mono.",
                ))
                .small()
                .weak(),
            );
        }
        if mode != s.audio_mode {
            if mode == AudioMode::Surround {
                s.surround_engine = if ai_supported() {
                    SurroundEngine::LocalAi
                } else {
                    SurroundEngine::Classic
                };
            }
            s.audio_mode = mode;
        }

        if s.audio_mode == AudioMode::Surround {
            egui::Grid::new("audio_grid")
                .num_columns(2)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.label(tr("Motor", "Engine"));
                    ui.add_enabled_ui(!surround_src, |ui| {
                        egui::ComboBox::from_id_salt("surround_engine")
                            .selected_text(engine_label(s.surround_engine))
                            .show_ui(ui, |ui| {
                                if ui
                                    .add_enabled(
                                    ai_supported(),
                                    egui::Button::selectable(
                                        s.surround_engine == SurroundEngine::LocalAi,
                                        engine_label(SurroundEngine::LocalAi),
                                    ),
                                )
                                .on_disabled_hover_text(tr(
                                    "Ljud-AI finns bara för Windows.",
                                    "The audio AI is only available on Windows.",
                                ))
                                .clicked()
                                {
                                    s.surround_engine = SurroundEngine::LocalAi;
                                }
                                ui.selectable_value(
                                    &mut s.surround_engine,
                                    SurroundEngine::Classic,
                                    engine_label(SurroundEngine::Classic),
                                );
                            });
                    })
                    .response
                    .on_hover_text(tr(
                        "AI delar upp ljudet i tal, musik och effekter och placerar dem i rummet. Klassisk tar själv ut det som ligger mitt i stereobilden till mitthögtalaren: snabbt och utan AI, och dialogen hamnar stadigt i mitten, men den skiljer inte tal från musik eller effekter som också ligger i mitten, så de flyttar dit också.",
                        "The AI splits the sound into speech, music and effects and places them in the room. Classic extracts what sits in the middle of the stereo image to the centre speaker itself: fast and without AI, and dialogue sits firmly in the centre, but it cannot tell speech from music or effects that are also centred, so those move to the centre too.",
                    ))
                    .on_disabled_hover_text(tr(
                        "Källan är redan 5.1: kanalerna behålls, ingen uppmixning.",
                        "The source is already 5.1: its channels are kept, no upmix.",
                    ));
                    ui.end_row();

                    ui.label(tr("Bas (LFE)", "Bass (LFE)"));
                    let classic = s.surround_engine == SurroundEngine::Classic;
                    ui.add_enabled_ui(!surround_src, |ui| {
                        egui::ComboBox::from_id_salt("lfe_level")
                            .selected_text(lfe_label(s.lfe_level))
                            .show_ui(ui, |ui| {
                                for l in LfeLevel::ALL {
                                    let enabled = !(l == LfeLevel::Cinema && classic);
                                    if ui
                                        .add_enabled(
                                        enabled,
                                        egui::Button::selectable(s.lfe_level == l, lfe_label(l)),
                                    )
                                    .on_disabled_hover_text(tr(
                                        "Biokänsla bygger extra bas av effektljudet, som bara AI-motorn tar fram.",
                                        "Cinema feel builds extra bass from the effects, which only the AI engine separates.",
                                    ))
                                    .clicked()
                                    {
                                        s.lfe_level = l;
                                    }
                                }
                            });
                    })
                    .response
                    .on_disabled_hover_text(tr(
                        "Källan är redan 5.1: dess LFE-kanal behålls.",
                        "The source is already 5.1: its LFE channel is kept.",
                    ));
                    ui.end_row();
                });
        }

        ui.add_enabled(
            ai_supported(),
            egui::Checkbox::new(
                &mut s.audio_enhance,
                tr("Förbättra ljudkvaliteten", "Improve the audio quality"),
            ),
        )
        .on_hover_text(tr(
            "Lokal AI reparerar spår av förlustkomprimering (AC-3, MP3, AAC): matta diskanter och \"plåtigt\" ljud. Basen (LFE) rörs aldrig. Kräver Ljud-AI.",
            "Local AI repairs traces of lossy compression (AC-3, MP3, AAC): dull treble and \"metallic\" sound. The bass (LFE) is never touched. Needs the audio AI.",
        ))
        .on_disabled_hover_text(tr(
            "Ljud-AI finns bara för Windows.",
            "The audio AI is only available on Windows.",
        ));
    });

    status_lines(ui, app, known.as_ref(), reading, failed);
}

/// Whether an export started now would use the AI audio pack.
pub fn export_needs_ai(app: &UpscalerApp) -> bool {
    let s = &app.settings;
    if !s.audio_active() {
        return false;
    }
    match source_of(app) {
        Source::Some(a) => audio_job::plan_job(s, Some(a), true).is_ok_and(|p| p.needs_ai()),
        Source::None | Source::Failed(_) => false,
        Source::NoVideo | Source::Reading => audio_job::may_use_ai(s),
    }
}

/// Whether an export started now makes new audio (and so an MKV), as far as the source is
/// known: not when nothing changes the audio, such as 5.1 chosen for a 5.1 source without
/// enhancement.
pub fn export_makes_audio(app: &UpscalerApp) -> bool {
    let s = &app.settings;
    if !s.audio_active() {
        return false;
    }
    match source_of(app) {
        Source::Some(a) => !audio_job::keeps_original(s, a),
        Source::None => false,
        Source::NoVideo | Source::Reading | Source::Failed(_) => true,
    }
}

/// Where the work runs, what the file gets, and the "pack missing" warning.
fn status_lines(
    ui: &mut egui::Ui,
    app: &mut UpscalerApp,
    src: Option<&SourceAudio>,
    reading: bool,
    failed: bool,
) {
    let s = &app.settings;
    if !s.audio_active() {
        ui.label(
            egui::RichText::new(tr(
                "Originalljudet kopieras oförändrat (MP4).",
                "The original audio is copied unchanged (MP4).",
            ))
            .weak()
            .small(),
        );
        return;
    }
    if reading {
        return; // the spinner above says it; the lines follow once the source is known
    }
    if failed {
        ui.label(
            egui::RichText::new(tr(
                "Utan läsbart ljudspår går det inte att exportera med nytt ljud. Välj Oförändrat.",
                "Without a readable audio track the export cannot make new audio. Choose Unchanged.",
            ))
            .small()
            .color(BAD),
        );
        return;
    }
    if src.is_some_and(|a| audio_job::keeps_original(s, a)) {
        ui.label(
            egui::RichText::new(tr(
                "Källan är redan 5.1 – ljudet behålls som det är (MP4).",
                "The source is already 5.1 – the audio is kept as is (MP4).",
            ))
            .weak()
            .small(),
        );
        return;
    }
    let plan = src.map(|a| audio_job::plan_job(s, Some(a), true));
    let (needs_ai, channels) = match &plan {
        Some(Ok(p)) => (p.needs_ai(), p.channels()),
        Some(Err(e)) => {
            ui.label(
                egui::RichText::new(format!(
                    "{} {}",
                    tr("Går inte:", "Not possible:"),
                    plan_error_text(&e.to_string())
                ))
                .small()
                .color(BAD),
            );
            return;
        }
        // Source not known yet: judge by the settings alone.
        None => (
            audio_job::may_use_ai(s),
            if s.audio_mode == AudioMode::Surround {
                6
            } else {
                2
            },
        ),
    };
    if needs_ai {
        match device_line(app) {
            Some((color, text, reason)) => {
                let r = ui.label(egui::RichText::new(text).small().color(color));
                if let Some(reason) = reason {
                    r.on_hover_text(format!(
                        "{} {reason}",
                        tr("Varför processorn:", "Why the CPU:")
                    ));
                }
            }
            None => {
                let (separate, passes) = ai_work(app);
                pack_missing(ui, app, separate, passes > 0);
            }
        }
    } else if matches!(&plan, Some(Ok(p)) if p.kind == Kind::UpmixClassic) {
        ui.label(
            egui::RichText::new(tr(
                "Körs på processorn, utan AI (snabbt).",
                "Runs on the CPU, without AI (fast).",
            ))
            .weak()
            .small(),
        );
    }
    let out = if channels == 6 {
        tr(
            "MKV: FLAC 5.1 + AC-3 5.1 + original",
            "MKV: FLAC 5.1 + AC-3 5.1 + original",
        )
    } else {
        tr("MKV: FLAC stereo + original", "MKV: FLAC stereo + original")
    };
    ui.label(egui::RichText::new(out).small());
}

/// The "pack missing" box, suggesting what fits what the AI is needed for.
fn pack_missing(ui: &mut egui::Ui, app: &mut UpscalerApp, separate: bool, enhance: bool) {
    let text = match (separate, enhance) {
        (true, true) => tr(
            "Ljud-AI är inte installerat. Installera det, eller välj Klassisk motor och stäng av Förbättra ljudkvaliteten.",
            "The audio AI is not installed. Install it, or choose the Classic engine and turn off Improve the audio quality.",
        ),
        (false, true) => tr(
            "Ljud-AI är inte installerat. Installera det, eller stäng av Förbättra ljudkvaliteten.",
            "The audio AI is not installed. Install it, or turn off Improve the audio quality.",
        ),
        _ => tr(
            "Ljud-AI är inte installerat. Installera det, eller välj Klassisk motor.",
            "The audio AI is not installed. Install it, or choose the Classic engine.",
        ),
    };
    egui::Frame::new()
        .fill(WARN.linear_multiply(0.12))
        .stroke(egui::Stroke::new(1.0, WARN.linear_multiply(0.6)))
        .corner_radius(4.0)
        .inner_margin(6.0)
        .show(ui, |ui| {
            ui.label(egui::RichText::new(format!("⚠ {text}")).small().color(WARN));
            if ui.button(tr("Öppna Ljud-AI…", "Open Audio AI…")).clicked() {
                app.settings_ui.open = true;
                app.settings_ui.tab = crate::ui::settings_window::TAB_AUDIO_AI;
            }
        });
}

// ------------------------------------------------------------ settings tab

fn install_stage(stage: &str) -> &'static str {
    match stage {
        pack::stage::PYTHON => tr("Hämtar Python…", "Downloading Python…"),
        pack::stage::PIP => tr("Installerar pip…", "Installing pip…"),
        pack::stage::TORCH_ROCM => tr(
            "Hämtar och installerar PyTorch för grafikkortet (ROCm)…",
            "Downloading and installing PyTorch for the GPU (ROCm)…",
        ),
        pack::stage::TORCH_CPU => tr(
            "Installerar PyTorch för processorn…",
            "Installing PyTorch for the CPU…",
        ),
        pack::stage::MSST => tr("Hämtar MSST…", "Downloading MSST…"),
        pack::stage::PACKAGES => tr("Installerar Python-paket…", "Installing Python packages…"),
        pack::stage::MODELS => tr("Hämtar AI-modellerna…", "Downloading the AI models…"),
        pack::stage::CHECK => tr("Kontrollerar paketet…", "Checking the pack…"),
        _ => tr("Installerar…", "Installing…"),
    }
}

fn device_name(d: &Device) -> String {
    match d {
        Device::Gpu(name) => format!("{} {name}", tr("grafikkortet", "the GPU")),
        Device::Cpu => tr("processorn", "the CPU").into(),
    }
}

/// Settings tab "Ljud-AI": install / remove the pack, where it runs, and a device test.
pub fn pack_tab(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    ui.label(
        egui::RichText::new(tr("Ljud-AI", "Audio AI"))
            .strong()
            .size(16.0),
    );
    ui.label(tr(
        "Lokal AI som gör 5.1 av stereoljud (delar upp tal, musik och effekter) och förbättrar ljudkvaliteten. Allt körs på den här datorn, helst på ett AMD-grafikkort. Paketet (Python, PyTorch och modellerna) laddas ner en gång.",
        "Local AI that makes 5.1 from stereo (separates speech, music and effects) and improves the audio quality. Everything runs on this computer, preferably on an AMD GPU. The pack (Python, PyTorch and the models) is downloaded once.",
    ));
    ui.add_space(8.0);
    if !ai_supported() {
        ui.label(
            egui::RichText::new(tr(
                "Ljud-AI finns bara för Windows. Klassisk 5.1 (utan AI) fungerar ändå.",
                "The audio AI is only available on Windows. Classic 5.1 (no AI) still works.",
            ))
            .color(WARN),
        );
        return;
    }

    let p = &app.audio_pack;
    egui::Grid::new("audio_pack")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            ui.label(tr("Status", "Status"));
            match &p.status {
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(tr("Kontrollerar…", "Checking…"));
                    });
                }
                Some(st) if st.installed => {
                    ui.label(
                        egui::RichText::new(format!(
                            "✔ {} ({})",
                            tr("Installerat", "Installed"),
                            format_bytes(st.size_bytes)
                        ))
                        .color(GOOD),
                    );
                }
                Some(st) => {
                    let text = if st.size_bytes > 0 {
                        format!(
                            "✖ {} ({} {})",
                            tr("Inte klart", "Not complete"),
                            format_bytes(st.size_bytes),
                            tr("på disken", "on disk")
                        )
                    } else {
                        format!("✖ {}", tr("Inte installerat", "Not installed"))
                    };
                    ui.label(egui::RichText::new(text).color(BAD));
                }
            }
            ui.end_row();
            ui.label(tr("Plats", "Location"));
            ui.label(egui::RichText::new(pack::pack_dir().display().to_string()).monospace());
            ui.end_row();
            if let Some(st) = &p.status {
                if let Some(d) = &st.device {
                    ui.label(tr("Körs på", "Runs on"));
                    ui.vertical(|ui| match d {
                        Device::Gpu(_) => {
                            ui.label(egui::RichText::new(device_name(d)).color(GOOD));
                        }
                        Device::Cpu => {
                            ui.label(
                                egui::RichText::new(cpu_estimate(
                                    app.video.as_ref().map(|v| v.info.duration),
                                    cpu_seconds_per_minute(ai_work(app)),
                                ))
                                .color(WARN),
                            );
                            if let Some(r) = &st.cpu_reason {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} {r}",
                                        tr("Varför processorn:", "Why the CPU:")
                                    ))
                                    .small()
                                    .weak(),
                                );
                            }
                        }
                    });
                    ui.end_row();
                }
            }
        });
    ui.add_space(6.0);

    let installed = p.status.as_ref().is_some_and(|s| s.installed);
    let on_disk = p.status.as_ref().is_some_and(|s| s.size_bytes > 0);
    let exporting = app.exporting();
    if let Some((stage, frac, cancel)) = &p.installing {
        ui.label(install_stage(stage));
        ui.add(egui::ProgressBar::new(*frac).show_percentage());
        if ui.button(tr("Avbryt", "Cancel")).clicked() {
            cancel.store(true, Ordering::Relaxed);
        }
    } else if let Some(since) = p.removing {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(format!(
                "{} {}",
                tr(
                    "Tar bort Ljud-AI (några minuter)…",
                    "Removing the audio AI (a few minutes)…"
                ),
                format_duration(since.elapsed().as_secs_f64())
            ));
        });
    } else {
        let mut install = false;
        let mut remove = false;
        let mut confirm = app.audio_pack.confirm_remove;
        ui.horizontal_wrapped(|ui| {
            if !installed {
                let label = if on_disk {
                    tr("Fortsätt installera", "Continue installing").to_string()
                } else {
                    format!(
                        "{} {})",
                        tr("Installera (cirka", "Install (about"),
                        format_bytes(PACK_SIZE_BYTES)
                    )
                };
                install = primary_button(ui, label, !exporting)
                    .on_hover_text(tr(
                        "Laddar ner och installerar. Tar runt 10 minuter; ett avbrutet försök fortsätter där det slutade.",
                        "Downloads and installs. Takes around 10 minutes; a cancelled attempt continues where it stopped.",
                    ))
                    .clicked();
            }
            if on_disk && !confirm {
                confirm = ui
                    .add_enabled(!exporting, egui::Button::new(tr("Ta bort…", "Remove…")))
                    .clicked();
            }
            if ui
                .add_enabled(
                    !app.audio_pack.checking,
                    egui::Button::new(tr("Kontrollera igen", "Check again")),
                )
                .clicked()
            {
                app.audio_pack.refresh(&app.bus);
            }
        });
        if confirm {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(tr(
                        "Ta bort Ljud-AI från disken? En ny installation tar runt 10 minuter.",
                        "Remove the audio AI from the disk? Installing again takes around 10 minutes.",
                    ))
                    .color(WARN),
                );
                if ui.button(tr("Ja, ta bort", "Yes, remove")).clicked() {
                    remove = true;
                    confirm = false;
                }
                if ui.button(tr("Nej", "No")).clicked() {
                    confirm = false;
                }
            });
        }
        app.audio_pack.confirm_remove = confirm;
        if exporting {
            ui.label(
                egui::RichText::new(tr(
                    "Vänta tills exporten är klar.",
                    "Wait for the export to finish.",
                ))
                .small()
                .weak(),
            );
        }
        if install {
            let cancel = Arc::new(AtomicBool::new(false));
            app.audio_pack.message = None;
            app.audio_pack.installing = Some((String::new(), 0.0, cancel.clone()));
            worker::install_pack(app.bus.clone(), cancel);
        }
        if remove {
            app.audio_pack.message = None;
            app.audio_pack.removing = Some(Instant::now());
            worker::remove_pack(app.bus.clone());
        }
    }
    if let Some(m) = &app.audio_pack.message {
        match m {
            Ok(t) => ui.label(egui::RichText::new(t).color(GOOD)),
            Err(e) => ui.label(egui::RichText::new(e).color(BAD)),
        };
    }

    if installed && !app.audio_pack.busy() {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !app.audio_pack.probing && !exporting,
                    egui::Button::new(tr("Testa", "Test")),
                )
                .on_disabled_hover_text(tr(
                    "Vänta tills exporten är klar.",
                    "Wait for the export to finish.",
                ))
                .on_hover_text(tr(
                    "Startar ljud-AI:n och frågar vilken enhet den använder (tar några sekunder).",
                    "Starts the audio AI and asks which device it uses (takes a few seconds).",
                ))
                .clicked()
            {
                app.audio_pack.probing = true;
                app.audio_pack.probe = None;
                worker::probe_device(app.bus.clone());
            }
            if app.audio_pack.probing {
                ui.spinner();
                ui.label(tr("Startar ljud-AI…", "Starting the audio AI…"));
            }
        });
        match &app.audio_pack.probe {
            Some(Ok(d)) => {
                let color = if matches!(d, Device::Gpu(_)) {
                    GOOD
                } else {
                    WARN
                };
                ui.label(
                    egui::RichText::new(format!(
                        "✔ {} {}",
                        tr(
                            "Ljud-AI svarar och använder",
                            "The audio AI answers and uses"
                        ),
                        device_name(d)
                    ))
                    .color(color),
                );
            }
            Some(Err(e)) => {
                ui.label(
                    egui::RichText::new(format!(
                        "✖ {} {e}",
                        tr("Testet misslyckades:", "The test failed:")
                    ))
                    .color(BAD),
                );
            }
            None => {}
        }
    }
}

// ------------------------------------------------------------ export texts

pub fn stage_name(s: AudioStage) -> &'static str {
    match s {
        AudioStage::Extracting => tr("Plockar ut ljudet…", "Extracting the audio…"),
        AudioStage::Separating => tr(
            "Delar upp ljudet i tal, musik och effekter (AI)…",
            "Separating speech, music and effects (AI)…",
        ),
        AudioStage::Enhancing => tr(
            "Förbättrar ljudkvaliteten (AI)…",
            "Improving the audio quality (AI)…",
        ),
        AudioStage::Placing => tr("Placerar ljudet i 5.1…", "Placing the sound in 5.1…"),
        AudioStage::Joining => tr(
            "Fogar ihop delarna och kodar ljudspåren…",
            "Joining the sections and encoding the tracks…",
        ),
        AudioStage::Checking => tr(
            "Kontrollerar ljudstyrka, toppar och synk…",
            "Checking loudness, peaks and sync…",
        ),
        AudioStage::Encoding => tr(
            "Kodar ljudspåren igen med justerad nivå…",
            "Encoding the tracks again at an adjusted level…",
        ),
    }
}

/// The audio job's English warnings in the UI language; unknown ones as they are, prefixed.
pub fn warning_text(w: &str) -> String {
    if english() {
        return if w == audio_job::CPU_SLOW_SEPARATION
            || w == audio_job::CPU_SLOW_ENHANCE
            || w.starts_with("The new audio")
            || w.starts_with("The loudness check")
            || w.starts_with("The sync check")
        {
            w.to_string()
        } else {
            format!("Audio warning: {w}")
        };
    }
    if w == audio_job::CPU_SLOW_SEPARATION {
        return "Ljud-AI körs på processorn: ljudseparationen blir långsam.".into();
    }
    if w == audio_job::CPU_SLOW_ENHANCE {
        return "Ljud-AI körs på processorn: ljudförbättringen blir långsam.".into();
    }
    if let Some(v) = w
        .strip_prefix("The new audio's loudness differs from the source by ")
        .and_then(|r| r.strip_suffix(" LU."))
    {
        return format!("Det nya ljudets ljudstyrka skiljer sig från originalets med {v} LU.");
    }
    if let Some(v) = w
        .strip_prefix("The new audio peaks at ")
        .and_then(|r| r.strip_suffix(" dBTP, above -1 dBTP."))
    {
        return format!("Det nya ljudet toppar på {v} dBTP, över -1 dBTP.");
    }
    if let Some(v) = w
        .strip_prefix("The new audio may be out of sync with the source: it is ")
        .and_then(|r| r.strip_suffix(" off."))
    {
        return format!(
            "Det nya ljudet kan vara osynkat mot originalet: det ligger {} fel.",
            v.replace(" samples (", " sampel (")
        );
    }
    if let Some(e) = w.strip_prefix("The loudness check could not run: ") {
        return format!("Kontrollen av ljudstyrkan kunde inte köras: {e}");
    }
    if let Some(e) = w.strip_prefix("The sync check could not run: ") {
        return format!("Synkkontrollen kunde inte köras: {e}");
    }
    format!("Ljudvarning: {w}")
}

/// `plan_job`'s refusals (English from core) in the UI language; others as they are.
pub fn plan_error_text(e: &str) -> String {
    if e.contains("mono source cannot be turned into 5.1") {
        return tr(
            "Källan är mono, och 5.1 kan inte göras av en enda kanal.",
            "The source is mono, and 5.1 cannot be made from a single channel.",
        )
        .into();
    }
    if e.contains("the video has no audio track") {
        return tr("Videon har inget ljud.", "The video has no audio.").into();
    }
    if let Some(n) = e
        .split_once("-channel audio is not supported")
        .and_then(|(n, _)| n.rsplit(' ').next())
    {
        return if english() {
            format!("{n}-channel audio is not supported (mono, stereo and 5.1 are).")
        } else {
            format!("Ljud med {n} kanaler stöds inte (mono, stereo och 5.1 gör det).")
        };
    }
    if e.contains("keep the original audio") {
        return tr(
            "Inställningarna behåller originalljudet.",
            "The settings keep the original audio.",
        )
        .into();
    }
    if e.contains(audio_job::ALREADY_51) {
        return tr(
            "Källan är redan 5.1 – ljudet behålls som det är.",
            "The source is already 5.1 – the audio is kept as is.",
        )
        .into();
    }
    error_text(e)
}

/// An export error in the UI language when it is a known audio one.
pub fn error_text(e: &str) -> String {
    const INSTALL: &str =
        "Ljud-AI är inte installerat (eller inte komplett). Installera det under Inställningar ➡ Ljud-AI";
    for (en, sv_end) in [
        (
            PACK_MISSING_BOTH,
            ", eller välj den klassiska 5.1-motorn och stäng av Förbättra ljudkvaliteten.",
        ),
        (
            PACK_MISSING_SEPARATION,
            ", eller välj den klassiska 5.1-motorn.",
        ),
        (
            PACK_MISSING_ENHANCE,
            ", eller stäng av Förbättra ljudkvaliteten.",
        ),
        (PACK_MISSING, "."),
    ] {
        if e.contains(en) {
            return if english() {
                en.to_string()
            } else {
                format!("{INSTALL}{sv_end}")
            };
        }
    }
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_known_warnings() {
        let _lang = crate::i18n::TEST_LANGUAGE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::i18n::set_english(false);
        assert_eq!(
            warning_text("The new audio's loudness differs from the source by +1.3 LU."),
            "Det nya ljudets ljudstyrka skiljer sig från originalets med +1.3 LU."
        );
        assert_eq!(
            warning_text("The new audio peaks at -0.4 dBTP, above -1 dBTP."),
            "Det nya ljudet toppar på -0.4 dBTP, över -1 dBTP."
        );
        assert_eq!(
            warning_text(
                "The new audio may be out of sync with the source: it is 12 samples (0.3 ms) off."
            ),
            "Det nya ljudet kan vara osynkat mot originalet: det ligger 12 sampel (0.3 ms) fel."
        );
        assert_eq!(
            warning_text("The sync check could not run: boom"),
            "Synkkontrollen kunde inte köras: boom"
        );
        assert_eq!(warning_text("Something new"), "Ljudvarning: Something new");
    }

    #[test]
    fn translates_plan_errors() {
        let _lang = crate::i18n::TEST_LANGUAGE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::i18n::set_english(false);
        // Built from the real refusals, so a changed core text shows up here.
        let s = upscaler_core::settings::Settings {
            audio_mode: AudioMode::Surround,
            ..Default::default()
        };
        let src = |channels| SourceAudio {
            channels,
            codec: "ac3".into(),
            dolby_surround: false,
            dialnorm: None,
        };
        let err = |a: &SourceAudio| {
            audio_job::plan_job(&s, Some(a), true)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            plan_error_text(&err(&src(1))),
            "Källan är mono, och 5.1 kan inte göras av en enda kanal."
        );
        assert_eq!(
            plan_error_text(&err(&src(4))),
            "Ljud med 4 kanaler stöds inte (mono, stereo och 5.1 gör det)."
        );
        let none = audio_job::plan_job(&s, None, true).unwrap_err().to_string();
        assert_eq!(plan_error_text(&none), "Videon har inget ljud.");
    }

    #[test]
    fn cpu_estimate_scales_with_duration() {
        let _lang = crate::i18n::TEST_LANGUAGE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::i18n::set_english(false);
        let sep = cpu_seconds_per_minute((true, 0));
        assert_eq!(sep, 250.0);
        // 45 min × 250 s = 3.1 h.
        assert_eq!(
            cpu_estimate(Some(45.0 * 60.0), sep),
            "processorn – cirka 3 tim per avsnitt"
        );
        // 5 min × 250 s ≈ 21 min.
        assert_eq!(
            cpu_estimate(Some(300.0), sep),
            "processorn – cirka 21 min för den här videon"
        );
        assert_eq!(
            cpu_estimate(None, sep),
            "processorn – cirka 4 tim per timme ljud"
        );
        // Enhancement counts: one pass for stereo, three for 5.1 (525 s each).
        assert_eq!(cpu_seconds_per_minute((false, 1)), 525.0);
        let all = cpu_seconds_per_minute((true, 3));
        assert_eq!(all, 250.0 + 3.0 * 525.0);
        // 45 min × 1825 s ≈ 22.8 h.
        assert_eq!(
            cpu_estimate(Some(45.0 * 60.0), all),
            "processorn – cirka 23 tim per avsnitt"
        );
    }

    #[test]
    fn pack_missing_and_cpu_texts_fit_the_job() {
        let _lang = crate::i18n::TEST_LANGUAGE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::i18n::set_english(false);
        let src = SourceAudio {
            channels: 2,
            codec: "ac3".into(),
            dolby_surround: false,
            dialnorm: None,
        };
        let refusal = |mode, enhance| {
            let s = upscaler_core::settings::Settings {
                audio_mode: mode,
                surround_engine: SurroundEngine::LocalAi,
                audio_enhance: enhance,
                ..Default::default()
            };
            plan_error_text(
                &audio_job::plan_job(&s, Some(&src), false)
                    .unwrap_err()
                    .to_string(),
            )
        };
        // Only the enhancement needs the pack: no "choose Classic".
        let t = refusal(AudioMode::Original, true);
        assert!(
            t.ends_with("eller stäng av Förbättra ljudkvaliteten."),
            "{t}"
        );
        assert!(!t.contains("klassiska"), "{t}");
        let t = refusal(AudioMode::Surround, false);
        assert!(t.ends_with("eller välj den klassiska 5.1-motorn."), "{t}");
        let t = refusal(AudioMode::Surround, true);
        assert!(
            t.contains("klassiska") && t.contains("Förbättra ljudkvaliteten"),
            "{t}"
        );
        assert!(error_text(PACK_MISSING).ends_with("Ljud-AI."));
        assert_eq!(
            warning_text(audio_job::CPU_SLOW_ENHANCE),
            "Ljud-AI körs på processorn: ljudförbättringen blir långsam."
        );
        let mut five = src.clone();
        five.channels = 6;
        let s = upscaler_core::settings::Settings {
            audio_mode: AudioMode::Surround,
            ..Default::default()
        };
        assert_eq!(
            plan_error_text(
                &audio_job::plan_job(&s, Some(&five), true)
                    .unwrap_err()
                    .to_string()
            ),
            "Källan är redan 5.1 – ljudet behålls som det är."
        );
    }
}
