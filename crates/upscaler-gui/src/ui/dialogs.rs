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
