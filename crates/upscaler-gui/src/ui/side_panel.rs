//! The right-hand panel: AI service, prompt, output, consistency and export.

use eframe::egui::{self, Color32};
use upscaler_core::job::Stage;
use upscaler_core::provider::codex::LoginState;
use upscaler_core::settings::{
    AiRate, OutputSize, PromptPreset, ProviderKind, VideoCodec, DEFAULT_PROMPT,
};

use super::style::{self, primary_button, section, BAD, GOOD, WARN};
use super::viewer::provider_name;
use crate::app::UpscalerApp;
use crate::i18n::{format_duration, tr};
use crate::worker;

pub fn show(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(4.0);
            provider(ui, app);
            ui.separator();
            prompt(ui, app);
            ui.separator();
            output(ui, app);
            ui.separator();
            consistency(ui, app);
            ui.separator();
            export(ui, app);
            ui.add_space(12.0);
        });
}

fn provider(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    section(ui, tr("AI-tjänst", "AI service"));
    let locked = app.exporting();
    ui.add_enabled_ui(!locked, |ui| {
        egui::ComboBox::from_id_salt("provider")
            .width(ui.available_width() - 8.0)
            .selected_text(provider_name(app.settings.provider))
            .show_ui(ui, |ui| {
                for p in ProviderKind::ALL {
                    ui.selectable_value(&mut app.settings.provider, p, provider_name(p));
                }
            });
    });
    let (dot, text) = provider_status(app);
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("⚫").color(dot).size(15.0));
        ui.label(text);
    });
    if ui
        .link(tr(
            "Konto och inställningar för tjänsten…",
            "Account and service settings…",
        ))
        .clicked()
    {
        app.settings_ui.open = true;
        app.settings_ui.tab = match app.settings.provider {
            ProviderKind::ChatGptSubscription => 0,
            ProviderKind::OpenAiApi => 1,
            ProviderKind::RealEsrgan => 2,
            ProviderKind::Basic => 4,
        };
    }
}

fn provider_status(app: &UpscalerApp) -> (Color32, String) {
    match app.settings.provider {
        ProviderKind::ChatGptSubscription => match &app.settings_ui.codex {
            None => (WARN, tr("Kontrollerar inloggningen…", "Checking the sign-in…").into()),
            Some(LoginState::NotInstalled) => (BAD, tr("Codex CLI är inte installerat – öppna inställningarna.", "Codex CLI is not installed – open the settings.").into()),
            Some(LoginState::LoggedInChatGpt(_)) => (GOOD, tr("Inloggad med ChatGPT – använder din prenumeration.", "Signed in with ChatGPT – uses your subscription.").into()),
            Some(LoginState::LoggedInApiKey(_)) => (WARN, tr("Codex är inloggat med API-nyckel (kostar per anrop). Logga in med ChatGPT i stället.", "Codex is signed in with an API key (billed per call). Sign in with ChatGPT instead.").into()),
            Some(LoginState::LoggedOut(_)) => (BAD, tr("Inte inloggad – klicka nedan och logga in med ChatGPT.", "Not signed in – click below and sign in with ChatGPT.").into()),
        },
        ProviderKind::OpenAiApi => {
            if app.settings.openai_api_key.trim().is_empty() && std::env::var("OPENAI_API_KEY").is_err() {
                (BAD, tr("Ingen API-nyckel angiven.", "No API key set.").into())
            } else {
                (GOOD, format!("{} {} · {}", tr("API-nyckel finns ·", "API key set ·"), app.settings.openai_model, tr("debiteras per bild", "billed per image")))
            }
        }
        ProviderKind::RealEsrgan => match upscaler_core::provider::realesrgan::RealEsrgan::locate(&app.settings.realesrgan_path) {
            Some(_) => (GOOD, tr("Real-ESRGAN hittades. Körs lokalt på grafikkortet, ingen prompt.", "Real-ESRGAN found. Runs locally on the GPU, no prompt.").into()),
            None => (BAD, tr("realesrgan-ncnn-vulkan hittades inte – ange sökvägen.", "realesrgan-ncnn-vulkan not found – set its path.").into()),
        },
        ProviderKind::Basic => (GOOD, tr("Ingen AI: snabb skalning för att testa flödet.", "No AI: quick resize to try the workflow.").into()),
    }
}

fn prompt(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    section(ui, tr("Prompt", "Prompt"));
    let generative = app.settings.provider.is_generative();
    if !generative {
        ui.label(
            egui::RichText::new(tr(
                "Den valda tjänsten använder ingen prompt.",
                "The selected service does not use a prompt.",
            ))
            .weak()
            .small(),
        );
    }
    ui.add_enabled_ui(generative && !app.exporting(), |ui| {
        ui.horizontal(|ui| {
            let current = app.settings.presets.iter().find(|p| p.text == app.settings.prompt).map(|p| p.name.clone());
            egui::ComboBox::from_id_salt("presets")
                .width(ui.available_width() - 90.0)
                .selected_text(current.clone().unwrap_or_else(|| tr("(egen, ej sparad)", "(custom, unsaved)").into()))
                .show_ui(ui, |ui| {
                    for p in app.settings.presets.clone() {
                        if ui.selectable_label(Some(&p.name) == current.as_ref(), &p.name).clicked() {
                            app.settings.prompt = p.text.clone();
                        }
                    }
                });
            if let Some(name) = &current {
                if app.settings.presets.len() > 1 && ui.small_button("🗑").on_hover_text(tr("Ta bort mallen", "Delete the preset")).clicked() {
                    let name = name.clone();
                    app.settings.presets.retain(|p| p.name != name);
                }
            }
        });
        ui.add(
            egui::TextEdit::multiline(&mut app.settings.prompt)
                .desired_rows(9)
                .desired_width(f32::INFINITY)
                .hint_text(tr("Beskriv hur bilderna ska restaureras…", "Describe how the frames should be restored…")),
        );
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut app.new_preset_name).desired_width(140.0).hint_text(tr("Namn på mall", "Preset name")));
            let name = app.new_preset_name.trim().to_string();
            if ui.add_enabled(!name.is_empty(), egui::Button::new(tr("Spara som mall", "Save as preset"))).clicked() {
                app.settings.presets.retain(|p| p.name != name);
                app.settings.presets.push(PromptPreset { name, text: app.settings.prompt.clone() });
                app.new_preset_name.clear();
            }
            if ui.small_button(tr("Standard", "Default")).on_hover_text(tr("Återställ standardprompten", "Restore the default prompt")).clicked() {
                app.settings.prompt = DEFAULT_PROMPT.to_string();
            }
        });
        ui.label(
            egui::RichText::new(tr(
                "Appen lägger själv till tekniska regler: samma bildutsnitt, inga nya objekt och – vid export – att föregående uppskalade bildruta används som referens för konsekvens.",
                "The app adds technical rules itself: same framing, no new objects and – when exporting – that the previous upscaled frame is used as a reference for consistency.",
            ))
            .weak()
            .small(),
        );
    });
}

fn output(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    section(ui, tr("Utdata", "Output"));
    ui.add_enabled_ui(!app.exporting(), |ui| {
        egui::Grid::new("output_grid")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label(tr("Storlek", "Size"));
                egui::ComboBox::from_id_salt("outsize")
                    .selected_text(app.settings.output_size.label())
                    .show_ui(ui, |ui| {
                        for s in OutputSize::ALL {
                            ui.selectable_value(&mut app.settings.output_size, s, s.label());
                        }
                    });
                ui.end_row();
                if let Some(v) = &app.video {
                    let (w, h) = app.settings.output_size.target(v.info.width, v.info.height);
                    ui.label("");
                    ui.label(
                        egui::RichText::new(format!(
                            "{}×{} ➡ {w}×{h}",
                            v.info.width, v.info.height
                        ))
                        .weak(),
                    );
                    ui.end_row();
                }
                ui.label(tr("Kodek", "Codec"));
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut app.settings.video_codec, VideoCodec::H264, "H.264");
                    ui.selectable_value(&mut app.settings.video_codec, VideoCodec::H265, "H.265");
                });
                ui.end_row();
                ui.label(tr("Kvalitet (CRF)", "Quality (CRF)"));
                ui.add(
                    egui::Slider::new(&mut app.settings.crf, 10..=30)
                        .text(tr("lägre = bättre", "lower = better")),
                );
                ui.end_row();
            });
        ui.label(
            egui::RichText::new(tr(
                "Ljudet kopieras alltid oförändrat från originalet.",
                "The audio is always copied unchanged from the original.",
            ))
            .weak()
            .small(),
        );
    });
}

fn consistency(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    section(ui, tr("Konsekvens och kostnad", "Consistency and cost"));
    let generative = app.settings.provider.is_generative();
    ui.add_enabled_ui(!app.exporting(), |ui| {
        ui.add_enabled_ui(generative, |ui| {
            ui.checkbox(&mut app.settings.use_previous_frame, tr("Använd föregående bildruta som referens", "Use the previous frame as a reference"))
                .on_hover_text(tr(
                    "Varje bildruta skickas tillsammans med den förra, redan uppskalade bildrutan så att detaljer, ansikten och färger hålls lika. Bildrutorna bearbetas då en i taget. Vid scenbyte skickas ingen referens.",
                    "Each frame is sent together with the previous, already upscaled frame so details, faces and colours stay the same. Frames are then processed one at a time. No reference is sent after a scene cut.",
                ));
            if app.settings.use_previous_frame {
                ui.horizontal(|ui| {
                    ui.label(tr("Scenbyte vid skillnad över", "Scene cut above difference"));
                    ui.add(egui::DragValue::new(&mut app.settings.scene_cut_threshold).range(5.0..=80.0).speed(0.5));
                });
            }
            ui.checkbox(&mut app.settings.pad_to_model_aspect, tr("Anpassa bildformat för modellen (svarta kanter, beskärs bort)", "Fit the model's aspect ratio (black bars, cropped away)"))
                .on_hover_text(tr(
                    "Bildmodeller arbetar i 1:1, 3:2 eller 2:3. 16:9-bilder får svarta kanter så att modellen inte beskär eller förvränger dem; kanterna tas bort efteråt.",
                    "Image models work in 1:1, 3:2 or 2:3. 16:9 frames get black bars so the model does not crop or distort them; the bars are removed afterwards.",
                ));
        });
        ui.checkbox(&mut app.settings.skip_duplicates, tr("Återanvänd resultat för identiska bildrutor", "Reuse results for identical frames"));
        ui.horizontal(|ui| {
            ui.label(tr("Skicka till AI", "Send to AI"));
            egui::ComboBox::from_id_salt("airate").selected_text(ai_rate_label(app.settings.ai_rate)).show_ui(ui, |ui| {
                for r in AiRate::ALL {
                    ui.selectable_value(&mut app.settings.ai_rate, r, ai_rate_label(r));
                }
            });
        })
        .response
        .on_hover_text(tr(
            "Varannan/var tredje: bara de bildrutorna skalas upp, resten skapas med rörelseinterpolering. Mycket billigare, men snabba rörelser kan få artefakter.",
            "Every 2nd/3rd: only those frames are upscaled, the rest are made with motion interpolation. Much cheaper, but fast motion can get artefacts.",
        ));
        let sequential = app.settings.sequential();
        ui.add_enabled_ui(!sequential, |ui| {
            ui.horizontal(|ui| {
                ui.label(tr("Parallella anrop", "Parallel requests"));
                ui.add(egui::DragValue::new(&mut app.settings.parallel_requests).range(1..=8));
                if sequential {
                    ui.label(egui::RichText::new(tr("(en i taget med referens)", "(one at a time with reference)")).weak().small());
                }
            });
        });
    });
}

fn ai_rate_label(r: AiRate) -> &'static str {
    match r {
        AiRate::All => tr("Alla bildrutor", "Every frame"),
        AiRate::Half => tr("Varannan + interpolering", "Every 2nd + interpolation"),
        AiRate::Third => tr("Var tredje + interpolering", "Every 3rd + interpolation"),
    }
}

fn export(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    section(ui, tr("Export", "Export"));
    if let Some(x) = &app.export {
        let running = x.outcome.is_none() && !x.cancelled;
        let stage = match x.stage {
            Stage::Extracting => tr("Plockar ut bildrutor…", "Extracting frames…"),
            Stage::Analysing => tr(
                "Letar scenbyten och dubbletter…",
                "Finding scene cuts and duplicates…",
            ),
            Stage::Upscaling => tr("Skalar upp bildrutor…", "Upscaling frames…"),
            Stage::Encoding => tr(
                "Kodar video med originalljud…",
                "Encoding video with the original audio…",
            ),
        };
        if running {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(egui::RichText::new(stage).strong());
            });
            let frac = if x.total > 0 {
                x.done as f32 / x.total as f32
            } else {
                0.0
            };
            ui.add(
                egui::ProgressBar::new(frac)
                    .show_percentage()
                    .desired_width(ui.available_width()),
            );
            ui.label(format!("{} / {}", x.done, x.total));
            let elapsed = x.started.elapsed().as_secs_f64();
            if x.stage == Stage::Upscaling && x.ai_frames > 0 {
                let remaining = x.total.saturating_sub(x.done) as f64;
                let per = elapsed / x.ai_frames as f64;
                ui.label(format!(
                    "{} {} · {} ~{}",
                    tr("Tid", "Time"),
                    format_duration(elapsed),
                    tr("kvar", "left"),
                    format_duration(remaining * per)
                ));
            } else {
                ui.label(format!(
                    "{} {}",
                    tr("Tid", "Time"),
                    format_duration(elapsed)
                ));
            }
            if x.already_done > 0 {
                ui.label(
                    egui::RichText::new(format!(
                        "{} {}",
                        x.already_done,
                        tr(
                            "bildrutor fanns redan från en tidigare körning.",
                            "frames were already done by an earlier run."
                        )
                    ))
                    .weak()
                    .small(),
                );
            }
            if let Some(tex) = &x.last_frame {
                let w = ui.available_width();
                let size = tex.size_vec2();
                ui.add(
                    egui::Image::new((tex.id(), egui::vec2(w, w * size.y / size.x)))
                        .corner_radius(4.0),
                );
                ui.label(
                    egui::RichText::new(tr("Senast klara bildruta", "Latest finished frame"))
                        .weak()
                        .small(),
                );
            }
            if !x.retries.is_empty() {
                egui::CollapsingHeader::new(format!(
                    "{} ({})",
                    tr("Omförsök", "Retries"),
                    x.retries.len()
                ))
                .show(ui, |ui| {
                    for r in x.retries.iter().rev().take(5) {
                        ui.label(egui::RichText::new(r).small().color(WARN));
                    }
                });
            }
            if ui
                .button(tr("Avbryt export", "Cancel export"))
                .on_hover_text(tr(
                    "Färdiga bildrutor sparas – starta samma export igen för att fortsätta.",
                    "Finished frames are kept – start the same export again to continue.",
                ))
                .clicked()
            {
                x.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            return;
        }
        let mut close = false;
        match (&x.outcome, x.cancelled) {
            (Some(Ok(path)), _) => {
                ui.label(
                    egui::RichText::new(format!("✔ {}", tr("Klar!", "Done!")))
                        .color(GOOD)
                        .strong(),
                );
                ui.label(egui::RichText::new(path.display().to_string()).small());
                ui.horizontal(|ui| {
                    if ui.button(tr("Spela upp", "Play")).clicked() {
                        worker::open(path);
                    }
                    if ui.button(tr("Visa i mapp", "Show in folder")).clicked() {
                        worker::reveal(path);
                    }
                    if ui.button(tr("Stäng", "Close")).clicked() {
                        close = true;
                    }
                });
            }
            (Some(Err(e)), _) => {
                ui.label(
                    egui::RichText::new(tr("Exporten misslyckades", "The export failed"))
                        .color(BAD)
                        .strong(),
                );
                ui.label(egui::RichText::new(e).small().color(BAD));
                ui.label(egui::RichText::new(tr("Färdiga bildrutor är sparade. Starta exporten igen för att fortsätta där den stannade.", "Finished frames are saved. Start the export again to continue where it stopped.")).small());
                if ui.button(tr("Stäng", "Close")).clicked() {
                    close = true;
                }
            }
            _ => {
                ui.label(tr(
                    "Exporten avbröts. Färdiga bildrutor är sparade och återanvänds nästa gång.",
                    "The export was cancelled. Finished frames are kept and reused next time.",
                ));
                if ui.button(tr("Stäng", "Close")).clicked() {
                    close = true;
                }
            }
        }
        if close {
            app.export = None;
        }
        return;
    }

    let Some(v) = &app.video else {
        ui.label(
            egui::RichText::new(tr(
                "Öppna en video för att exportera.",
                "Open a video to export.",
            ))
            .weak(),
        );
        return;
    };
    let (s, e) = v.range();
    let frames = e - s;
    let ai_frames = frames.div_ceil(app.settings.ai_rate.divisor() as u64);
    let per = app.settings.estimate_seconds_per_frame() as f64;
    let parallel = if app.settings.sequential() {
        1.0
    } else {
        app.settings.parallel_requests.max(1) as f64
    };
    let estimate = ai_frames as f64 * per / parallel;
    egui::Grid::new("export_grid")
        .num_columns(2)
        .spacing([10.0, 4.0])
        .show(ui, |ui| {
            ui.label(tr("Bildrutor", "Frames"));
            ui.label(frames.to_string());
            ui.end_row();
            ui.label(tr("Till AI", "To the AI"));
            ui.label(format!("{ai_frames} {}", tr("(högst)", "(at most)")));
            ui.end_row();
            ui.label(tr("Uppskattad tid", "Estimated time"));
            ui.label(format!("~{}", format_duration(estimate)));
            ui.end_row();
        });
    if app.settings.provider == ProviderKind::ChatGptSubscription && ai_frames > 50 {
        ui.label(
            egui::RichText::new(tr(
                "Tips: ChatGPT-planer har en gräns för antal bilder per tidsperiod. Testa med ett kort urval (Sätt start/slut) först. En avbruten eller stoppad export fortsätter där den slutade.",
                "Tip: ChatGPT plans limit the number of images per period. Try a short range (Set in/out) first. A cancelled or stopped export resumes where it ended.",
            ))
            .small()
            .color(WARN),
        );
    }
    ui.add_space(4.0);
    let ready = app.tools.is_some() && app.preview.is_none();
    if primary_button(
        ui,
        format!("⬇ {}", tr("Exportera video…", "Export video…")),
        ready,
    )
    .clicked()
    {
        app.start_export();
    }
    if app.preview.is_some() {
        ui.label(
            egui::RichText::new(tr(
                "Vänta tills förhandsvisningen är klar.",
                "Wait for the preview to finish.",
            ))
            .weak()
            .small(),
        );
    }
    let _ = style::ACCENT;
}
