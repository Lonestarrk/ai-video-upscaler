use eframe::egui;

use crate::app::UpscalerApp;
use crate::i18n::{format_bytes, format_time, tr};

pub fn show(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui
            .button(format!("📂 {}", tr("Öppna video…", "Open video…")))
            .clicked()
        {
            app.open_dialog();
        }
        ui.separator();
        if let Some(p) = &app.loading {
            ui.spinner();
            ui.label(format!("{} {}", tr("Öppnar", "Opening"), p.display()));
        } else if let Some(v) = &app.video {
            let i = &v.info;
            let name = i
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            ui.label(egui::RichText::new(name).strong())
                .on_hover_text(i.path.display().to_string());
            ui.label(
                egui::RichText::new(format!(
                    "{}×{}  ·  {:.3} fps  ·  {}  ·  {} {}  ·  {}{}  ·  {}",
                    i.width,
                    i.height,
                    i.fps(),
                    format_time(i.duration),
                    i.frame_count,
                    tr("bildrutor", "frames"),
                    i.video_codec,
                    i.audio_codec
                        .as_ref()
                        .map(|a| format!(" + {a}"))
                        .unwrap_or_else(|| format!(" ({})", tr("inget ljud", "no audio"))),
                    format_bytes(i.file_size)
                ))
                .weak(),
            );
        } else {
            ui.label(
                egui::RichText::new(tr(
                    "Ingen video öppnad – dra in en fil i fönstret",
                    "No video open – drop a file on the window",
                ))
                .weak(),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button(format!("⚙ {}", tr("Inställningar", "Settings")))
                .clicked()
            {
                app.settings_ui.open = !app.settings_ui.open;
            }
            if app.tools.is_none() {
                if ui
                    .button(
                        egui::RichText::new(format!("⚠ {}", tr("FFmpeg saknas", "FFmpeg missing")))
                            .color(super::style::WARN),
                    )
                    .clicked()
                {
                    app.settings_ui.open = true;
                    app.settings_ui.tab = 3;
                }
            }
        });
    });
    ui.add_space(2.0);
}
