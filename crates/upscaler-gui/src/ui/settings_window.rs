//! Settings: ChatGPT sign-in through Codex, OpenAI API key, Real-ESRGAN, FFmpeg and folders,
//! language and size.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use eframe::egui;
use upscaler_core::provider::codex::{self, LoginState};

use super::style::{primary_button, BAD, GOOD, WARN};
use crate::app::UpscalerApp;
use crate::i18n::{self, format_bytes, tr};
use crate::worker::{self, CodexOp};

pub fn show(ctx: &egui::Context, app: &mut UpscalerApp) {
    if !app.settings_ui.open {
        return;
    }
    let mut open = true;
    egui::Window::new(tr("Inställningar", "Settings"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size([640.0, 520.0])
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                let tabs = [
                    "ChatGPT",
                    "OpenAI API",
                    "Real-ESRGAN",
                    tr("FFmpeg och mappar", "FFmpeg and folders"),
                    tr("Allmänt", "General"),
                ];
                for (i, t) in tabs.iter().enumerate() {
                    ui.selectable_value(&mut app.settings_ui.tab, i, *t);
                }
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .show(ui, |ui| match app.settings_ui.tab {
                    0 => chatgpt(ui, app),
                    1 => openai(ui, app),
                    2 => realesrgan(ui, app),
                    3 => ffmpeg(ui, app),
                    _ => general(ui, app),
                });
        });
    if !open {
        app.settings_ui.open = false;
    }
}

fn chatgpt(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    ui.label(
        egui::RichText::new(tr(
            "ChatGPT med din prenumeration",
            "ChatGPT with your subscription",
        ))
        .strong()
        .size(16.0),
    );
    ui.label(tr(
        "Appen använder OpenAI:s officiella Codex CLI, som loggar in med ditt ChatGPT-konto (Plus, Pro eller Business). Bildgenereringen räknas då mot planens gränser i stället för att debiteras per anrop via API. Varje bildruta tar ungefär en till två minuter.",
        "The app uses OpenAI's official Codex CLI, which signs in with your ChatGPT account (Plus, Pro or Business). Image generation then counts against your plan's limits instead of being billed per call through the API. Each frame takes roughly one to two minutes.",
    ));
    ui.add_space(8.0);

    let busy = app.settings_ui.codex_busy.is_some();
    let state = app.settings_ui.codex.clone();
    let installed = codex::locate(&app.settings.codex_path);
    egui::Grid::new("codex_status")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            ui.label(tr("Codex CLI", "Codex CLI"));
            match &installed {
                Some(p) => ui.label(egui::RichText::new(format!("✔ {}", p.display())).color(GOOD)),
                None => ui.label(
                    egui::RichText::new(tr("✖ Inte installerat", "✖ Not installed")).color(BAD),
                ),
            };
            ui.end_row();
            ui.label(tr("Konto", "Account"));
            match &state {
                None => ui.label(tr("Kontrollerar…", "Checking…")),
                Some(LoginState::NotInstalled) => ui.label("–"),
                Some(LoginState::LoggedInChatGpt(t)) => {
                    ui.label(egui::RichText::new(format!("✔ {}", first_line(t))).color(GOOD))
                }
                Some(LoginState::LoggedInApiKey(t)) => {
                    ui.label(egui::RichText::new(format!("⚠ {}", first_line(t))).color(WARN))
                }
                Some(LoginState::LoggedOut(t)) => ui.label(
                    egui::RichText::new(format!(
                        "✖ {}",
                        if t.is_empty() {
                            tr("Inte inloggad", "Not signed in")
                        } else {
                            first_line(t)
                        }
                    ))
                    .color(BAD),
                ),
            };
            ui.end_row();
        });
    ui.add_space(6.0);

    if let Some((what, cancel)) = &app.settings_ui.codex_busy {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(what.as_str());
            if ui.button(tr("Avbryt", "Cancel")).clicked() {
                cancel.store(true, Ordering::Relaxed);
            }
        });
    }
    ui.horizontal_wrapped(|ui| {
        if installed.is_none() {
            if codex::npm_available() {
                if primary_button(ui, tr("Installera Codex CLI", "Install Codex CLI"), !busy).on_hover_text("npm install -g @openai/codex").clicked() {
                    start(app, tr("Installerar Codex CLI med npm…", "Installing Codex CLI with npm…"), CodexOp::Install);
                }
            } else {
                ui.label(tr("Installera först Node.js (LTS), starta om appen och klicka sedan Installera här.", "Install Node.js (LTS) first, restart the app and then click Install here."));
                ui.hyperlink_to("nodejs.org", "https://nodejs.org/");
            }
        } else {
            let logged_in = matches!(state, Some(LoginState::LoggedInChatGpt(_)));
            if primary_button(ui, if logged_in { tr("Logga in igen", "Sign in again") } else { tr("Logga in med ChatGPT", "Sign in with ChatGPT") }, !busy).clicked() {
                start(app, tr("Väntar på inloggningen i webbläsaren… (välj \"Sign in with ChatGPT\")", "Waiting for the sign-in in the browser… (choose \"Sign in with ChatGPT\")"), CodexOp::Login);
            }
            if logged_in && ui.add_enabled(!busy, egui::Button::new(tr("Logga ut", "Sign out"))).clicked() {
                start(app, tr("Loggar ut…", "Signing out…"), CodexOp::Logout);
            }
        }
        if ui.add_enabled(!busy, egui::Button::new(tr("Kontrollera igen", "Check again"))).clicked() {
            app.settings_ui.codex = None;
            worker::codex_status(app.bus.clone(), app.settings.codex_path.clone());
        }
        ui.hyperlink_to(tr("Om Codex CLI", "About Codex CLI"), "https://github.com/openai/codex");
    });
    if let Some(m) = &app.settings_ui.codex_message {
        match m {
            Ok(t) if !t.is_empty() => {
                ui.label(egui::RichText::new(t).color(GOOD).small());
            }
            Ok(_) => {}
            Err(e) => {
                ui.label(egui::RichText::new(e).color(BAD).small());
            }
        }
    }

    ui.add_space(10.0);
    egui::CollapsingHeader::new(tr("Avancerat", "Advanced")).show(ui, |ui| {
        egui::Grid::new("codex_adv").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label(tr("Sökväg till codex", "Path to codex"));
            path_field(ui, &mut app.settings.codex_path, tr("tom = hitta automatiskt", "empty = find automatically"), false);
            ui.end_row();
            ui.label(tr("Modell (agent)", "Model (agent)"));
            ui.add(egui::TextEdit::singleline(&mut app.settings.codex_model).hint_text(tr("tom = Codex standard", "empty = Codex default")));
            ui.end_row();
            ui.label(tr("Resonemang", "Reasoning"));
            egui::ComboBox::from_id_salt("reasoning")
                .selected_text(if app.settings.codex_reasoning.is_empty() { tr("standard", "default") } else { app.settings.codex_reasoning.as_str() })
                .show_ui(ui, |ui| {
                    for (v, l) in [("low", "low"), ("medium", "medium"), ("high", "high"), ("", tr("standard", "default"))] {
                        ui.selectable_value(&mut app.settings.codex_reasoning, v.to_string(), l);
                    }
                });
            ui.end_row();
            ui.label(tr("Sandlåda", "Sandbox"));
            egui::ComboBox::from_id_salt("sandbox").selected_text(app.settings.codex_sandbox.clone()).show_ui(ui, |ui| {
                for v in ["workspace-write", "danger-full-access"] {
                    ui.selectable_value(&mut app.settings.codex_sandbox, v.to_string(), v);
                }
            });
            ui.end_row();
            ui.label(tr("Tidsgräns per bild (s)", "Timeout per image (s)"));
            ui.add(egui::DragValue::new(&mut app.settings.codex_timeout_secs).range(60..=3600));
            ui.end_row();
        });
        ui.label(
            egui::RichText::new(tr(
                "\"low\" resonemang sparar kvot: agenten behöver bara anropa bildverktyget. Byt sandlåda till danger-full-access om Codex inte kan spara output.png på din dator. Codex arbetar bara i en tillfällig mapp per bildruta.",
                "\"low\" reasoning saves quota: the agent only has to call the image tool. Switch the sandbox to danger-full-access if Codex cannot save output.png on your computer. Codex only works in a temporary folder per frame.",
            ))
            .weak()
            .small(),
        );
    });
}

fn first_line(s: &str) -> &str {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or(s)
}

fn start(app: &mut UpscalerApp, what: &str, op: CodexOp) {
    let cancel = Arc::new(AtomicBool::new(false));
    app.settings_ui.codex_message = None;
    app.settings_ui.codex_busy = Some((what.to_string(), cancel.clone()));
    worker::codex_action(app.bus.clone(), app.settings.codex_path.clone(), op, cancel);
}

fn openai(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    ui.label(
        egui::RichText::new(tr(
            "OpenAI API (betala per bild)",
            "OpenAI API (pay per image)",
        ))
        .strong()
        .size(16.0),
    );
    ui.label(tr(
        "Ett alternativ till prenumerationen: snabbare och kan köra flera bildrutor parallellt, men varje bild debiteras på ditt API-konto (platform.openai.com).",
        "An alternative to the subscription: faster and can run several frames in parallel, but every image is billed to your API account (platform.openai.com).",
    ));
    ui.add_space(8.0);
    egui::Grid::new("openai").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label(tr("API-nyckel", "API key"));
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut app.settings.openai_api_key).password(!app.settings_ui.show_key).desired_width(320.0).hint_text("sk-…"));
            ui.checkbox(&mut app.settings_ui.show_key, tr("Visa", "Show"));
        });
        ui.end_row();
        ui.label(tr("Modell", "Model"));
        ui.add(egui::TextEdit::singleline(&mut app.settings.openai_model).hint_text("gpt-image-2"));
        ui.end_row();
        ui.label(tr("Storlek", "Size"));
        egui::ComboBox::from_id_salt("oa_size").selected_text(app.settings.openai_size.clone()).show_ui(ui, |ui| {
            for v in ["auto", "1536x1024", "1024x1536", "1024x1024", "2048x1152", "2048x1365"] {
                ui.selectable_value(&mut app.settings.openai_size, v.to_string(), v);
            }
        });
        ui.end_row();
        ui.label(tr("Kvalitet", "Quality"));
        egui::ComboBox::from_id_salt("oa_q").selected_text(app.settings.openai_quality.clone()).show_ui(ui, |ui| {
            for v in ["high", "medium", "low", "auto"] {
                ui.selectable_value(&mut app.settings.openai_quality, v.to_string(), v);
            }
        });
        ui.end_row();
        ui.label("input_fidelity");
        ui.checkbox(&mut app.settings.openai_input_fidelity, tr("high (håller ansikten och detaljer närmare originalet, om modellen stöder det)", "high (keeps faces and details closer to the original, if the model supports it)"));
        ui.end_row();
    });
    ui.label(egui::RichText::new(tr("Nyckeln sparas i inställningsfilen på den här datorn. Lämnas fältet tomt används miljövariabeln OPENAI_API_KEY.", "The key is stored in the settings file on this computer. If left empty, the OPENAI_API_KEY environment variable is used.")).weak().small());
    ui.hyperlink_to(
        tr("Skapa en API-nyckel", "Create an API key"),
        "https://platform.openai.com/api-keys",
    );
}

fn realesrgan(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    ui.label(egui::RichText::new("Real-ESRGAN").strong().size(16.0));
    ui.label(tr(
        "Gratis och lokalt, sekunder per bild på ett vanligt grafikkort. Ingen prompt och inte lika naturligt som ChatGPT, men bra som jämförelse. Ladda ner realesrgan-ncnn-vulkan, packa upp och peka ut programfilen.",
        "Free and local, seconds per frame on an ordinary GPU. No prompt and not as natural as ChatGPT, but good for comparison. Download realesrgan-ncnn-vulkan, unpack it and point to the program file.",
    ));
    ui.add_space(8.0);
    egui::Grid::new("esr")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            ui.label(tr("Programfil", "Program file"));
            path_field(
                ui,
                &mut app.settings.realesrgan_path,
                "realesrgan-ncnn-vulkan(.exe)",
                false,
            );
            ui.end_row();
            ui.label(tr("Modell", "Model"));
            egui::ComboBox::from_id_salt("esr_model")
                .selected_text(app.settings.realesrgan_model.clone())
                .show_ui(ui, |ui| {
                    for v in [
                        "realesrgan-x4plus",
                        "realesrnet-x4plus",
                        "realesr-animevideov3",
                        "realesrgan-x4plus-anime",
                    ] {
                        ui.selectable_value(&mut app.settings.realesrgan_model, v.to_string(), v);
                    }
                });
            ui.end_row();
        });
    ui.hyperlink_to(
        tr("Ladda ner Real-ESRGAN", "Download Real-ESRGAN"),
        "https://github.com/xinntao/Real-ESRGAN/releases",
    );
}

fn ffmpeg(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    ui.label(egui::RichText::new("FFmpeg").strong().size(16.0));
    ui.label(tr(
        "FFmpeg läser videon, plockar ut bildrutor och sätter ihop resultatet med originalljudet.",
        "FFmpeg reads the video, extracts frames and assembles the result with the original audio.",
    ));
    ui.add_space(6.0);
    match &app.tools {
        Some(t) => {
            ui.label(egui::RichText::new(format!("✔ {}", t.ffmpeg.display())).color(GOOD));
        }
        None => {
            ui.label(
                egui::RichText::new(tr("✖ FFmpeg hittades inte", "✖ FFmpeg was not found"))
                    .color(BAD),
            );
        }
    }
    if let Some((done, total, cancel)) = &app.settings_ui.ffmpeg_download {
        let frac = total.map(|t| *done as f32 / t.max(1) as f32).unwrap_or(0.0);
        ui.add(egui::ProgressBar::new(frac).text(format!(
            "{} / {}",
            format_bytes(*done),
            total.map(format_bytes).unwrap_or_else(|| "?".into())
        )));
        if ui.button(tr("Avbryt", "Cancel")).clicked() {
            cancel.store(true, Ordering::Relaxed);
        }
    } else {
        ui.horizontal(|ui| {
            if upscaler_core::ffmpeg_download::supported() {
                let label = if app.tools.is_some() {
                    tr("Hämta senaste FFmpeg", "Download latest FFmpeg")
                } else {
                    tr("Hämta FFmpeg (~100 MB)", "Download FFmpeg (~100 MB)")
                };
                if primary_button(ui, label, true).clicked() {
                    let cancel = Arc::new(AtomicBool::new(false));
                    app.settings_ui.ffmpeg_message = None;
                    app.settings_ui.ffmpeg_download = Some((0, None, cancel.clone()));
                    worker::download_ffmpeg(app.bus.clone(), cancel);
                }
            } else {
                ui.label(tr(
                    "Installera ffmpeg med pakethanteraren.",
                    "Install ffmpeg with your package manager.",
                ));
            }
            if ui.button(tr("Sök igen", "Search again")).clicked() {
                app.reload_tools();
            }
        });
    }
    if let Some(m) = &app.settings_ui.ffmpeg_message {
        match m {
            Ok(t) => ui.label(egui::RichText::new(t).color(GOOD)),
            Err(e) => ui.label(egui::RichText::new(e).color(BAD)),
        };
    }
    ui.horizontal(|ui| {
        ui.label(tr(
            "Egen mapp med ffmpeg och ffprobe",
            "Own folder with ffmpeg and ffprobe",
        ));
        if path_field(
            ui,
            &mut app.settings.ffmpeg_dir,
            tr("tom = automatiskt", "empty = automatic"),
            true,
        ) {
            app.reload_tools();
        }
    });

    ui.separator();
    ui.label(
        egui::RichText::new(tr("Arbetsmapp", "Work folder"))
            .strong()
            .size(16.0),
    );
    ui.label(tr(
        "Här sparas förhandsvisningar och exportjobb (alla bildrutor som PNG – det kan bli många GB för långa videor). Jobb som avbryts fortsätter härifrån.",
        "Previews and export jobs are stored here (every frame as PNG – it can be many GB for long videos). Cancelled jobs resume from here.",
    ));
    ui.horizontal(|ui| {
        let hint = app.settings.work_dir().display().to_string();
        path_field(ui, &mut app.settings.work_dir, &hint, true);
    });
    ui.horizontal(|ui| {
        match app.settings_ui.work_dir_size {
            Some(b) => ui.label(format!("{}: {}", tr("Använder", "Uses"), format_bytes(b))),
            None => ui.label(""),
        };
        if ui.button(tr("Beräkna storlek", "Compute size")).clicked() {
            worker::work_dir_size(app.bus.clone(), app.settings.work_dir());
        }
        if ui.button(tr("Öppna", "Open")).clicked() {
            let dir = app.settings.work_dir();
            let _ = std::fs::create_dir_all(&dir);
            worker::open(&dir);
        }
        let can_clear = !app.exporting() && app.preview.is_none();
        if ui
            .add_enabled(
                can_clear,
                egui::Button::new(tr(
                    "Töm (förhandsvisningar och jobb)",
                    "Empty (previews and jobs)",
                )),
            )
            .clicked()
        {
            let _ = upscaler_core::job::clean_work_dir(&app.settings, true);
            app.settings_ui.work_dir_size = None;
            if let Some(v) = &mut app.video {
                v.previewed.clear();
                v.result = None;
            }
        }
    });
}

fn general(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    egui::Grid::new("general")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
            ui.label(tr("Språk", "Language"));
            ui.horizontal(|ui| {
                let mut changed = false;
                changed |= ui
                    .selectable_value(&mut app.settings.language, "sv".into(), "Svenska")
                    .changed();
                changed |= ui
                    .selectable_value(&mut app.settings.language, "en".into(), "English")
                    .changed();
                changed |= ui
                    .selectable_value(
                        &mut app.settings.language,
                        String::new(),
                        tr("Systemets", "System"),
                    )
                    .changed();
                if changed {
                    i18n::apply(&app.settings.language);
                }
            });
            ui.end_row();
            ui.label(tr("Storlek på gränssnittet", "Interface size"));
            ui.add(
                egui::Slider::new(&mut app.settings.ui_scale, 0.75..=2.0)
                    .step_by(0.05)
                    .custom_formatter(|v, _| format!("{:.0} %", v * 100.0)),
            );
            ui.end_row();
        });
    ui.add_space(10.0);
    ui.label(egui::RichText::new(tr("Kortkommandon", "Keyboard shortcuts")).strong());
    for (k, d) in [
        (
            "⬅ / ➡",
            tr("föregående / nästa bildruta", "previous / next frame"),
        ),
        (
            "Shift + ⬅ / ➡",
            tr("en sekund bakåt / framåt", "one second back / forward"),
        ),
        ("Home / End", tr("början / slutet", "start / end")),
        (
            "I / O",
            tr(
                "sätt start / slut för exporten",
                "set in / out for the export",
            ),
        ),
        ("P", tr("skapa förhandsvisning", "create preview")),
        (
            tr("Scrollhjul / dra", "Wheel / drag"),
            tr("zooma / panorera bilden", "zoom / pan the picture"),
        ),
    ] {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(k).monospace());
            ui.label(d);
        });
    }
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.label(format!("AI Video Upscaler {}", upscaler_core::version()));
        if ui
            .link(tr("Öppna loggfilen", "Open the log file"))
            .clicked()
        {
            worker::open(&upscaler_core::paths::log_file());
        }
    });
}

/// A text field with a browse button; returns true when the value changed.
fn path_field(ui: &mut egui::Ui, value: &mut String, hint: &str, folder: bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        changed |= ui
            .add(
                egui::TextEdit::singleline(value)
                    .desired_width(300.0)
                    .hint_text(hint),
            )
            .changed();
        if ui.button("…").clicked() {
            let picked = if folder {
                rfd::FileDialog::new().pick_folder()
            } else {
                rfd::FileDialog::new().pick_file()
            };
            if let Some(p) = picked {
                *value = p.display().to_string();
                changed = true;
            }
        }
    });
    changed
}
