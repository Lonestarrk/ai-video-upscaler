//! A dark theme suited to judging pictures (neutral greys, one accent colour).

use eframe::egui::{self, Color32};

pub const ACCENT: Color32 = Color32::from_rgb(98, 160, 255);
pub const GOOD: Color32 = Color32::from_rgb(110, 200, 120);
pub const WARN: Color32 = Color32::from_rgb(240, 180, 80);
pub const BAD: Color32 = Color32::from_rgb(235, 95, 95);
pub const CANVAS: Color32 = Color32::from_rgb(16, 16, 18);

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut v = egui::Visuals::dark();
    v.selection.bg_fill = ACCENT.linear_multiply(0.55);
    v.hyperlink_color = ACCENT;
    v.panel_fill = Color32::from_rgb(30, 31, 34);
    v.window_fill = Color32::from_rgb(36, 37, 41);
    v.extreme_bg_color = Color32::from_rgb(22, 22, 25);
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
        // egui's small text is hard to read at 100 %; hints and notes use it a lot.
        if let Some(f) = s.text_styles.get_mut(&egui::TextStyle::Small) {
            f.size = 11.5;
        }
    });
}

/// A filled accent button for the main action of a section.
pub fn primary_button(ui: &mut egui::Ui, text: impl Into<String>, enabled: bool) -> egui::Response {
    let text = egui::RichText::new(text.into())
        .color(Color32::WHITE)
        .strong();
    ui.add_enabled(
        enabled,
        egui::Button::new(text)
            .fill(ACCENT.linear_multiply(0.8))
            .min_size(egui::vec2(0.0, 30.0)),
    )
}

pub fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(8.0);
    ui.label(egui::RichText::new(title).strong().size(15.0));
    ui.add_space(2.0);
}
