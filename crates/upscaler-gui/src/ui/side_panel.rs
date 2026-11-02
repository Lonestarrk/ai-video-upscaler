//! The right-hand panel: AI service, prompt, output, consistency and export.

use eframe::egui::{self, Color32};
use upscaler_core::job::Stage;
use upscaler_core::provider::codex::LoginState;
use upscaler_core::settings::{
    AiRate, AspectChoice, HdrMode, OutputSize, PromptPreset, ProviderKind, VideoCodec,
    DEFAULT_PROMPT,
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
            super::audio::side_section(ui, app);
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
        app.settings_ui.tab = crate::ui::settings_window::tab_for(app.settings.provider);
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
        ProviderKind::Gemini => key_status(&app.settings.gemini_api_key, &upscaler_core::provider::gemini::ENV_KEYS, &app.settings.gemini_model),
        ProviderKind::Flux => key_status(&app.settings.bfl_api_key, &upscaler_core::provider::flux::ENV_KEYS, &app.settings.bfl_model),
        ProviderKind::Grok => key_status(&app.settings.xai_api_key, &upscaler_core::provider::grok::ENV_KEYS, &app.settings.xai_model),
        ProviderKind::RealEsrgan => match upscaler_core::provider::realesrgan::RealEsrgan::locate(&app.settings.realesrgan_path) {
            Some(_) => (GOOD, tr("Real-ESRGAN hittades. Körs lokalt på grafikkortet, ingen prompt.", "Real-ESRGAN found. Runs locally on the GPU, no prompt.").into()),
            None => (BAD, tr("realesrgan-ncnn-vulkan hittades inte – ange sökvägen.", "realesrgan-ncnn-vulkan not found – set its path.").into()),
        },
        ProviderKind::Basic => (GOOD, tr("Ingen AI: snabb skalning för att testa flödet.", "No AI: quick resize to try the workflow.").into()),
    }
}

fn key_status(key: &str, env: &[&str], model: &str) -> (Color32, String) {
    if upscaler_core::provider::http::key(key, env).is_empty() {
        (
            BAD,
            tr(
                "Ingen API-nyckel angiven – öppna inställningarna.",
                "No API key set – open the settings.",
            )
            .into(),
        )
    } else {
        (
            GOOD,
            format!(
                "{} {model} · {}",
                tr("API-nyckel finns ·", "API key set ·"),
                tr("debiteras per bild", "billed per image")
            ),
        )
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
                .desired_rows(14)
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
                    let g = upscaler_core::job::geometry(&app.settings, &v.info);
                    let i = &v.info;
                    let text = match g.sar {
                        Some((n, d)) => format!(
                            "{}×{} ➡ {}×{}\n{} {n}:{d}, {} {}×{}",
                            i.coded_width,
                            i.coded_height,
                            g.stored.0,
                            g.stored.1,
                            tr("anamorft, pixlar", "anamorphic, pixels"),
                            tr("visas som", "shown as"),
                            g.work.0,
                            g.work.1
                        ),
                        None => format!("{}×{} ➡ {}×{}", i.width, i.height, g.stored.0, g.stored.1),
                    };
                    ui.label("");
                    ui.label(egui::RichText::new(text).weak());
                    if i.anamorphic() && upscaler_core::job::extension(&app.settings, i).is_none() {
                        ui.end_row();
                        ui.label("");
                        ui.checkbox(
                            &mut app.settings.keep_anamorphic,
                            tr("Behåll anamorfa pixlar", "Keep anamorphic pixels"),
                        )
                        .on_hover_text(tr(
                            "Videon har icke-kvadratiska pixlar (t.ex. DVD). Bilden sträcks till kvadratiska pixlar för förhandsvisning och AI, och kläms sedan tillbaka till originalets pixelformat i den exporterade filen. Av: exportera med kvadratiska pixlar.",
                            "The video has non-square pixels (e.g. DVD). The picture is stretched to square pixels for preview and AI, then squeezed back to the original pixel shape in the exported file. Off: export with square pixels.",
                        ));
                    }
                    ui.end_row();
                }
                ui.label(tr("Kodek", "Codec"));
                if app.settings.hdr_mode != HdrMode::Off {
                    // HDR needs 10-bit H.265; the saved choice stays untouched.
                    let mut shown = app.settings.effective_codec();
                    ui.add_enabled_ui(false, |ui| {
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut shown, VideoCodec::H264, "H.264");
                            ui.selectable_value(&mut shown, VideoCodec::H265, "H.265");
                        });
                    })
                    .response
                    .on_disabled_hover_text(tr(
                        "HDR kräver H.265 (10 bitar).",
                        "HDR requires H.265 (10-bit).",
                    ));
                } else {
                    ui.horizontal(|ui| {
                        ui.selectable_value(
                            &mut app.settings.video_codec,
                            VideoCodec::H264,
                            "H.264",
                        );
                        ui.selectable_value(
                            &mut app.settings.video_codec,
                            VideoCodec::H265,
                            "H.265",
                        );
                    });
                }
                ui.end_row();
                hdr_rows(ui, app);
                ui.label(tr("Kvalitet (CRF)", "Quality (CRF)"));
                ui.add(
                    egui::Slider::new(&mut app.settings.crf, 10..=30)
                        .text(tr("lägre = bättre", "lower = better")),
                );
                ui.end_row();
            });
        if app.settings.hdr_mode != HdrMode::Off {
            // Outside the grid so it wraps at the panel width.
            ui.label(
                egui::RichText::new(tr(
                    "Förhandsvisningen visar SDR – HDR syns på TV:n",
                    "The preview shows SDR – HDR shows on the TV",
                ))
                .weak(),
            );
        }
        source_aspect(ui, app);
        black_bars(ui, app);
        aspect_change(ui, app);
    });
}

/// HDR rows of the output grid: mode, peak brightness, SDR white, keep/delete the in-between
/// frames.
fn hdr_rows(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    ui.label("HDR");
    let mode = &mut app.settings.hdr_mode;
    egui::ComboBox::from_id_salt("hdr_mode")
        .selected_text(match *mode {
            HdrMode::Off => tr("Av", "Off").to_string(),
            HdrMode::Hdr10Plus => tr(
                "HDR10+ (fungerar även som HDR10)",
                "HDR10+ (also plays as HDR10)",
            )
            .to_string(),
            m => m.name().to_string(),
        })
        .show_ui(ui, |ui| {
            ui.selectable_value(mode, HdrMode::Off, tr("Av", "Off"));
            ui.selectable_value(
                mode,
                HdrMode::Hdr10Plus,
                tr(
                    "HDR10+ (fungerar även som HDR10)",
                    "HDR10+ (also plays as HDR10)",
                ),
            );
            ui.selectable_value(mode, HdrMode::Hdr10, "HDR10");
            ui.selectable_value(mode, HdrMode::Hlg, "HLG");
        });
    ui.end_row();
    if app.settings.hdr_mode == HdrMode::Off {
        return;
    }
    ui.label(tr("Toppljusstyrka", "Peak brightness"));
    if app.settings.hdr_mode == HdrMode::Hlg {
        ui.label(
            egui::RichText::new(tr("HLG har fast 1000 nits", "HLG uses a fixed 1000 nits")).weak(),
        );
    } else {
        egui::ComboBox::from_id_salt("hdr_peak")
            .selected_text(format!("{} nits", app.settings.hdr_peak_nits))
            .show_ui(ui, |ui| {
                for n in [600u32, 1000, 1500] {
                    ui.selectable_value(&mut app.settings.hdr_peak_nits, n, format!("{n} nits"));
                }
            });
    }
    ui.end_row();
    ui.label(tr("SDR-vitt", "SDR white"));
    egui::ComboBox::from_id_salt("hdr_sdr_white")
        .selected_text(format!("{} nits", app.settings.hdr_sdr_white_nits))
        .show_ui(ui, |ui| {
            for n in [203u32, 300, 400] {
                ui.selectable_value(&mut app.settings.hdr_sdr_white_nits, n, format!("{n} nits"));
            }
        })
        .response
        .on_hover_text(tr(
            "Där vanligt vitt hamnar i HDR-bilden. Högre ger en ljusare bild överlag, \
             bra för projektorer och mörkare skärmar. 203 är standard (BT.2408).",
            "Where ordinary white lands in the HDR picture. Higher makes the whole picture \
             brighter, good for projectors and dimmer displays. 203 is the standard (BT.2408).",
        ));
    ui.end_row();
    // Only "every 2nd/3rd frame" makes in-between frames for HDR.
    if app.settings.ai_rate.divisor() > 1 {
        ui.label("");
        ui.checkbox(
            &mut app.settings.hdr_delete_interp_after,
            tr(
                "Ta bort mellanbildrutor efter lyckad export",
                "Delete in-between frames after a successful export",
            ),
        )
        .on_hover_text(tr(
            "Mellanbildrutorna (interp\\ i jobbmappen) behövs bara för att koda videon. \
             Behålls de slipper en ny export räkna fram dem igen.",
            "The in-between frames (interp\\ in the job folder) are only needed to encode \
             the video. Kept, a new export does not have to make them again.",
        ));
        ui.end_row();
    }
}

/// Frame shapes the user can give the source instead of its flag.
const FRAME_CHOICES: [((u32, u32), &str); 5] = [
    ((4, 3), "4:3"),
    ((16, 9), "16:9"),
    ((185, 100), "1.85:1"),
    ((235, 100), "2.35:1"),
    ((239, 100), "2.39:1"),
];

fn ratio_name(r: f64) -> String {
    upscaler_core::aspect::standard_name(r)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{r:.2}:1"))
}

/// "Source aspect ratio": what the file declares, a warning with suggestions when that looks
/// wrong, and a menu to set it by hand.
fn source_aspect(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    let Some(v) = &mut app.video else { return };
    let declared = v.info.declared_frame_aspect();
    let declared_name = ratio_name(declared);
    let square = (v.info.source_width, v.info.source_height);
    let label = |choice: Option<(u32, u32)>| -> String {
        match choice {
            None => format!("{} ({declared_name})", tr("Från filen", "From the file")),
            Some(c) if c == square => format!(
                "{} ({}×{})",
                tr("Kvadratiska pixlar", "Square pixels"),
                square.0,
                square.1
            ),
            Some(c) => FRAME_CHOICES
                .iter()
                .find(|(f, _)| *f == c)
                .map(|(_, n)| n.to_string())
                .unwrap_or_default(),
        }
    };
    ui.add_space(4.0);
    let mut chosen = v.frame_aspect;
    ui.horizontal(|ui| {
        ui.label(tr("Källans bildformat", "Source aspect ratio")).on_hover_text(tr(
            "Hur hela bildrutan i filen ska visas. Ändra bara om filen är felflaggad: personer ser smala eller breda ut, runda saker är ovala.",
            "How the whole frame in the file is shown. Only change it if the file is flagged wrongly: people look thin or wide, round things are oval.",
        ));
        egui::ComboBox::from_id_salt("source_aspect").selected_text(label(chosen)).show_ui(ui, |ui| {
            ui.selectable_value(&mut chosen, None, label(None));
            for (f, _) in FRAME_CHOICES {
                ui.selectable_value(&mut chosen, Some(f), label(Some(f)));
            }
            ui.selectable_value(&mut chosen, Some(square), label(Some(square)));
        });
    });
    if chosen != v.frame_aspect {
        v.frame_aspect = chosen;
        v.aspect_hint_done = true;
    }

    let check = v.aspect_check.clone();
    let picture_now = v.info.width as f64 / v.info.height.max(1) as f64;
    if v.frame_aspect.is_none() && !v.aspect_hint_done && check.standard.is_none() {
        egui::Frame::new()
            .fill(WARN.linear_multiply(0.12))
            .stroke(egui::Stroke::new(1.0, WARN.linear_multiply(0.6)))
            .corner_radius(4.0)
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.label(egui::RichText::new(format!("⚠ {}", tr("Bildformatet kan vara felflaggat", "The aspect ratio may be flagged wrongly"))).color(WARN).strong());
                ui.label(format!(
                    "{} {:.2}:1 {} ({declared_name}). {}",
                    tr("Bilden blir", "The picture comes out at"),
                    check.picture,
                    tr("med filens flagga", "with the file's flag"),
                    tr("Det är inget vanligt film- eller TV-format.", "That is no usual film or TV shape."),
                ));
                if check.suggestions.is_empty() {
                    ui.label(egui::RichText::new(tr(
                        "Kontrollera i svepläget att runda saker ser runda ut, och välj annars format ovan.",
                        "Check in swipe mode that round things look round, otherwise choose a shape above.",
                    )).small());
                }
                ui.horizontal_wrapped(|ui| {
                    for sug in &check.suggestions {
                        let text = format!(
                            "{} {} ({} {})",
                            tr("Använd", "Use"),
                            sug.frame_name,
                            tr("bilden blir", "picture becomes"),
                            sug.picture_name
                        );
                        if ui.button(text).clicked() {
                            v.frame_aspect = Some(sug.frame);
                            v.aspect_hint_done = true;
                        }
                    }
                    if ui.button(tr("Behåll filens", "Keep the file's")).clicked() {
                        v.aspect_hint_done = true;
                    }
                });
            });
    } else {
        let note = match v.frame_aspect {
            Some(_) => format!(
                "{} {declared_name} · {} {}",
                tr("Filen säger", "The file says"),
                tr("bilden visas nu som", "the picture is now shown as"),
                ratio_name(picture_now)
            ),
            None => format!(
                "{} {}",
                tr("Bilden:", "Picture:"),
                ratio_name(check.picture)
            ),
        };
        ui.label(egui::RichText::new(note).weak().small());
    }
}

/// "Remove black bars": letterbox/pillarbox found when the video was opened.
fn black_bars(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    ui.add_space(4.0);
    ui.checkbox(&mut app.settings.remove_black_bars, tr("Ta bort svarta kanter (letterbox)", "Remove black bars (letterbox)"))
        .on_hover_text(tr(
            "Svarta fält som är inbakade i bilden (t.ex. bredbildsfilm på en 4:3-DVD) skärs bort innan uppskalningen, så att bara själva bilden restaureras och exporteras.",
            "Black bars baked into the picture (e.g. a widescreen film on a 4:3 DVD) are cut away before upscaling, so only the picture itself is restored and exported.",
        ));
    if let Some(v) = &app.video {
        let i = &v.info;
        let text = match i.detected_crop {
            Some(c) => {
                let shown_w =
                    ((c.w as f64 * i.sar.0 as f64 / i.sar.1 as f64 / 2.0).round() as u32) * 2;
                format!(
                    "{} {}×{} ➡ {}×{} ({:.2}:1)",
                    tr("Hittade svarta kanter:", "Black bars found:"),
                    i.source_width,
                    i.source_height,
                    c.w,
                    c.h,
                    shown_w as f64 / c.h as f64
                )
            }
            None => tr(
                "Inga svarta kanter hittades i videon.",
                "No black bars found in the video.",
            )
            .to_string(),
        };
        ui.label(egui::RichText::new(text).weak().small());
    }
}

/// "Change aspect ratio": a checkbox that turns it on and a menu with the ratios.
fn aspect_change(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    let generative = app.settings.provider.is_generative();
    ui.add_space(4.0);
    ui.add_enabled_ui(generative, |ui| {
        ui.horizontal(|ui| {
            ui.checkbox(&mut app.settings.change_aspect, tr("Ändra bildformat till", "Change aspect ratio to"))
                .on_hover_text(tr(
                    "AI:n fyller ut bilden till det nya formatet genom att fortsätta scenen åt sidorna eller uppåt och nedåt. Inget av originalbilden beskärs bort. Föregående bildruta används som referens så att de utfyllda delarna hålls stabila.",
                    "The AI extends the picture to the new shape by continuing the scene to the sides or above and below. Nothing of the original is cropped away. The previous frame is used as a reference so the added parts stay stable.",
                ));
            ui.add_enabled_ui(app.settings.change_aspect, |ui| {
                egui::ComboBox::from_id_salt("target_aspect")
                    .selected_text(aspect_label(app.settings.target_aspect))
                    .show_ui(ui, |ui| {
                        for a in AspectChoice::ALL {
                            ui.selectable_value(&mut app.settings.target_aspect, a, aspect_label(a));
                        }
                    });
            });
        });
    });
    if !generative {
        ui.label(
            egui::RichText::new(tr(
                "Byte av bildformat kräver ChatGPT eller OpenAI API.",
                "Changing the aspect ratio needs ChatGPT or the OpenAI API.",
            ))
            .weak()
            .small(),
        );
    } else if let (Some(a), Some(v)) = (app.settings.aspect_change(), &app.video) {
        let (w, h) = upscaler_core::job::geometry(&app.settings, &v.info).stored;
        let src = v.info.width as f64 / v.info.height as f64;
        let note = if (a.ratio() / src - 1.0).abs() <= 0.01 {
            tr(
                "Videon har redan det formatet.",
                "The video already has that shape.",
            )
            .to_string()
        } else if a.ratio() > src {
            format!(
                "{w}×{h} · {}",
                tr(
                    "AI:n fyller ut till vänster och höger",
                    "the AI fills in left and right"
                )
            )
        } else {
            format!(
                "{w}×{h} · {}",
                tr(
                    "AI:n fyller ut ovanför och nedanför",
                    "the AI fills in above and below"
                )
            )
        };
        ui.label(egui::RichText::new(note).weak().small());
    }
}

fn aspect_label(a: AspectChoice) -> String {
    let what = match a {
        AspectChoice::Wide16x9 => tr("bredbild", "widescreen"),
        AspectChoice::Tv4x3 => tr("TV", "TV"),
        AspectChoice::Scope239 => tr("bio, cinemascope", "cinema, scope"),
        AspectChoice::Scope235 => tr("bio, äldre cinemascope", "cinema, classic scope"),
        AspectChoice::Flat185 => tr("bio", "cinema"),
        AspectChoice::Photo3x2 => tr("foto", "photo"),
        AspectChoice::Square1x1 => tr("kvadrat", "square"),
        AspectChoice::Portrait4x5 => tr("stående, Instagram", "portrait, Instagram"),
        AspectChoice::Vertical9x16 => tr("stående, mobil", "vertical, phone"),
    };
    format!("{} ({what})", a.name())
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
            if app.settings.use_previous_frame && !app.settings.reference_supported() {
                ui.label(
                    egui::RichText::new(tr(
                        "Den här tjänsten tar bara en bild åt gången, så ingen referens skickas och bildrutorna kan bearbetas parallellt. Prompten håller ändå stilen.",
                        "This service takes one image at a time, so no reference is sent and frames can run in parallel. The prompt still keeps the style.",
                    ))
                    .small()
                    .color(WARN),
                );
            }
            if app.settings.use_previous_frame && app.settings.reference_supported() {
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
        quality_control(ui, app);
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

fn quality_control(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    let supported = app.settings.provider.can_review();
    ui.add_enabled_ui(supported, |ui| {
        ui.checkbox(
            &mut app.settings.review_frames,
            tr("Kvalitetskontroll av varje bildruta", "Quality control of every frame"),
        )
        .on_hover_text(tr(
            "Under exporten granskar tjänstens egen AI varje färdig bildruta en gång, i bakgrunden, mot originalet och föregående bildruta: inget får ha ändrats, lagts till eller förvrängts, och bildrutorna ska hänga ihop. Underkända bildrutor görs om med granskarens anmärkningar i prompten. Kostar ett extra anrop per bildruta, men ingen bildgenerering.",
            "During the export the service's own AI checks every finished frame once, in the background, against the original and the previous frame: nothing may be changed, added or distorted, and the frames must match. Rejected frames are made again with the reviewer's notes in the prompt. Costs one extra request per frame, but no image generation.",
        ));
        if app.settings.review_frames && supported {
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                ui.label(tr("Försök per bildruta", "Attempts per frame"));
                ui.add(egui::DragValue::new(&mut app.settings.review_max_attempts).range(1..=6))
                    .on_hover_text(tr(
                        "Efter så många underkända försök behålls den sista versionen och markeras i loggen.",
                        "After this many rejected attempts the last version is kept and noted in the log.",
                    ));
            });
        }
    });
    if !supported && app.settings.provider.is_generative() {
        ui.label(
            egui::RichText::new(tr(
                "Kvalitetskontroll finns än så länge bara med ChatGPT.",
                "Quality control is only available with ChatGPT for now.",
            ))
            .small()
            .weak(),
        );
    }
}

fn ai_rate_label(r: AiRate) -> &'static str {
    match r {
        AiRate::All => tr("Alla bildrutor", "Every frame"),
        AiRate::Half => tr("Varannan + interpolering", "Every 2nd + interpolation"),
        AiRate::Third => tr("Var tredje + interpolering", "Every 3rd + interpolation"),
    }
}

/// The audio job's warnings, translated, in yellow.
fn audio_warnings(ui: &mut egui::Ui, warnings: &[String]) {
    for w in warnings {
        ui.label(
            egui::RichText::new(format!("⚠ {}", super::audio::warning_text(w)))
                .small()
                .color(WARN),
        );
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
            Stage::Hdr => tr("Konverterar till HDR…", "Converting to HDR…"),
            Stage::Encoding if x.audio_path => tr(
                "Kodar videon (utan ljud, det görs sedan)…",
                "Encoding the video (without audio, that comes next)…",
            ),
            Stage::Encoding => tr(
                "Kodar video med originalljud…",
                "Encoding video with the original audio…",
            ),
            Stage::Audio => tr("Bearbetar ljudet…", "Processing the audio…"),
        };
        if running {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(egui::RichText::new(stage).strong());
            });
            if x.stage == Stage::Audio {
                if let Some(s) = x.audio_stage {
                    ui.label(super::audio::stage_name(s));
                }
            }
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
            if x.stage == Stage::Audio {
                // Until the job reports its sections the count is a placeholder (0 of 1).
                if x.audio_sections_known {
                    ui.label(format!(
                        "{} {} {} {}",
                        tr("Ljuddel", "Audio section"),
                        x.done,
                        tr("av", "of"),
                        x.total
                    ));
                }
            } else {
                ui.label(format!("{} / {}", x.done, x.total));
            }
            audio_warnings(ui, &x.audio_warnings);
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
            if x.spec.settings.review_active() {
                ui.label(format!(
                    "{}: {} {} · {} {}{}{}",
                    tr("Kvalitetskontroll", "Quality control"),
                    x.approved,
                    tr("godkända", "approved"),
                    x.rejected,
                    tr("gjorda om", "made again"),
                    if x.kept > 0 {
                        format!(
                            " · {} {}",
                            x.kept,
                            tr("behållna trots fel", "kept despite problems")
                        )
                    } else {
                        String::new()
                    },
                    if x.unchecked > 0 {
                        format!(" · {} {}", x.unchecked, tr("ej granskade", "unchecked"))
                    } else {
                        String::new()
                    },
                ));
                if ui
                    .button(format!("🖼 {}", tr("Visa bildrutor", "Show frames")))
                    .clicked()
                {
                    app.frame_browser.open = true;
                }
                if !x.review_notes.is_empty() {
                    egui::CollapsingHeader::new(tr("Anmärkningar", "Notes"))
                        .id_salt("review_notes")
                        .show(ui, |ui| {
                            for r in x.review_notes.iter().rev().take(8) {
                                ui.label(egui::RichText::new(r).small().color(WARN));
                            }
                        });
                }
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
                audio_warnings(ui, &x.audio_warnings);
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
                ui.label(
                    egui::RichText::new(super::audio::plan_error_text(e))
                        .small()
                        .color(BAD),
                );
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
    // The audio AI pack is being installed or removed while the export would use it.
    let pack_busy = app.audio_pack.busy() && super::audio::export_needs_ai(app);
    let ready = app.tools.is_some() && app.preview.is_none() && !pack_busy;
    let button = primary_button(
        ui,
        format!("⬇ {}", tr("Exportera video…", "Export video…")),
        ready,
    );
    let button = if pack_busy {
        button.on_disabled_hover_text(tr(
            "Ljud-AI installeras eller tas bort just nu. Vänta tills det är klart, eller välj Klassisk motor och stäng av Förbättra ljudkvaliteten.",
            "The audio AI is being installed or removed. Wait until it is done, or choose the Classic engine and turn off Improve the audio quality.",
        ))
    } else {
        button
    };
    if button.clicked() {
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
