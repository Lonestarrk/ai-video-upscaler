use eframe::egui::{self, Color32};

use crate::app::UpscalerApp;
use crate::i18n::tr;

/// Error messages as a small modal window.
pub fn show(ctx: &egui::Context, app: &mut UpscalerApp) {
    let Some(msg) = app.error.clone() else { return };
    let modal = egui::Modal::new(egui::Id::unique("error_modal")).show(ctx, |ui| {
        ui.set_max_width(460.0);
        ui.label(
            egui::RichText::new(tr("Något gick fel", "Something went wrong"))
                .strong()
                .size(16.0),
        );
        ui.add_space(6.0);
        ui.label(msg);
        ui.add_space(10.0);
        ui.button("OK").clicked()
    });
    if modal.inner || modal.should_close() {
        app.error = None;
    }
}

/// Asks before an export replaces a file the save dialog did not ask about: new audio is
/// written as `.mkv`, so `x.mp4` typed in the dialog becomes `x.mkv`, which may exist.
pub fn confirm_overwrite(ctx: &egui::Context, app: &mut UpscalerApp) {
    let Some(path) = app.confirm_overwrite.clone() else {
        return;
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let modal = egui::Modal::new(egui::Id::unique("confirm_overwrite_modal")).show(ctx, |ui| {
        ui.set_max_width(460.0);
        ui.label(
            egui::RichText::new(tr("Skriva över filen?", "Overwrite the file?"))
                .strong()
                .size(16.0),
        );
        ui.add_space(6.0);
        ui.label(format!(
            "{} {name} {}",
            tr(
                "Med nytt ljud sparas exporten som MKV:",
                "With new audio the export is saved as MKV:"
            ),
            tr(
                "finns redan i mappen. Vill du ersätta den?",
                "already exists in the folder. Do you want to replace it?"
            ),
        ));
        ui.label(
            egui::RichText::new(path.display().to_string())
                .small()
                .weak(),
        );
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button(tr("Ersätt", "Replace")).clicked() {
                Some(true)
            } else if ui.button(tr("Avbryt", "Cancel")).clicked() {
                Some(false)
            } else {
                None
            }
        })
        .inner
    });
    let answer = match modal.inner {
        Some(a) => Some(a),
        None if modal.should_close() => Some(false),
        None => None,
    };
    if let Some(replace) = answer {
        app.confirm_overwrite = None;
        if replace {
            app.launch_export(path);
        }
    }
}

/// Highlights the window while a file is dragged over it.
pub fn drop_overlay(ctx: &egui::Context) {
    let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
    if !hovering {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::unique("drop"),
    ));
    let rect = ctx.content_rect();
    painter.rect_filled(rect, 0.0, Color32::from_black_alpha(170));
    painter.rect_stroke(
        rect.shrink(16.0),
        12.0,
        egui::Stroke::new(3.0, super::style::ACCENT),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        tr(
            "Släpp videon för att öppna den",
            "Drop the video to open it",
        ),
        egui::FontId::proportional(26.0),
        Color32::WHITE,
    );
}
