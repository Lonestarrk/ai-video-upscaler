//! The comparison area: original and upscaled frame side by side with a divider, or one
//! picture with a swipe line. Zoom (wheel) and pan (drag) are shared by both halves so the
//! same detail is always compared.

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use upscaler_core::settings::{CompareMode, ProviderKind};

use super::style::{self, ACCENT, CANVAS};
use crate::app::{UpscalerApp, ViewTransform};
use crate::i18n::{format_duration, tr};

pub fn provider_name(p: ProviderKind) -> &'static str {
    match p {
        ProviderKind::ChatGptSubscription => {
            tr("ChatGPT (prenumeration)", "ChatGPT (subscription)")
        }
        ProviderKind::OpenAiApi => tr("OpenAI API (betala per bild)", "OpenAI API (pay per image)"),
        ProviderKind::Gemini => tr(
            "Google Gemini – Nano Banana (betala per bild)",
            "Google Gemini – Nano Banana (pay per image)",
        ),
        ProviderKind::Flux => tr(
            "FLUX – Black Forest Labs (betala per bild)",
            "FLUX – Black Forest Labs (pay per image)",
        ),
        ProviderKind::Grok => tr("Grok – xAI (betala per bild)", "Grok – xAI (pay per image)"),
        ProviderKind::RealEsrgan => tr("Real-ESRGAN (lokal, gratis)", "Real-ESRGAN (local, free)"),
        ProviderKind::Basic => tr("Enkel uppskalning (test)", "Basic upscale (test)"),
    }
}

pub fn show(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    if app.video.is_none() {
        empty_state(ui, app);
        return;
    }
    controls(ui, app);
    ui.add_space(4.0);
    let rect = ui.available_rect_before_wrap();
    let response = ui.allocate_rect(rect, Sense::click_and_drag());
    ui.painter().rect_filled(rect, 4.0, CANVAS);

    let v = app.video.as_ref().unwrap();
    // The original is always shown in its own shape. The result has the output shape: the
    // same, or the new aspect ratio the AI extends the picture to, with the original's
    // content somewhere inside it. Zoom and pan follow the original's content in both.
    let source_aspect = v.info.width as f32 / v.info.height.max(1) as f32;
    let extending = upscaler_core::job::extension(&app.settings, &v.info).is_some();
    let output_aspect = if extending {
        let (w, h) = upscaler_core::job::geometry(&app.settings, &v.info).work;
        w as f32 / h.max(1) as f32
    } else {
        source_aspect
    };
    let (ix, iy, iw, ih) =
        upscaler_core::frame::inner_fraction(source_aspect as f64, output_aspect as f64);
    let source = Layout {
        aspect: source_aspect,
        inner: (0.0, 0.0, 1.0, 1.0),
    };
    let output = Layout {
        aspect: output_aspect,
        inner: (ix as f32, iy as f32, iw as f32, ih as f32),
    };
    let mode = app.settings.compare_mode;
    let panes: Vec<(Rect, Layout)> = match mode {
        CompareMode::SideBySide => {
            let half = (rect.width() - 4.0) / 2.0;
            vec![
                (
                    Rect::from_min_size(rect.min, egui::vec2(half, rect.height())),
                    source,
                ),
                (
                    Rect::from_min_size(
                        egui::pos2(rect.min.x + half + 4.0, rect.min.y),
                        egui::vec2(half, rect.height()),
                    ),
                    output,
                ),
            ]
        }
        CompareMode::Swipe => vec![(rect, output)],
    };

    // ---- interaction: both panes share one transform, in the original's coordinates
    let mut t = app.view;
    let pane_at = |p: Pos2| {
        panes
            .iter()
            .copied()
            .find(|(r, _)| r.contains(p))
            .unwrap_or(panes[0])
    };
    let swipe_id = ui.scope_id().with("swipe_drag");
    if let Some(pos) = response.hover_pos() {
        let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
        let factor = (scroll * 0.0018).exp() * pinch;
        if (factor - 1.0).abs() > 1e-4 {
            let (pane, layout) = pane_at(pos);
            zoom_at(&mut t, pane, layout, pos, factor);
        }
    }
    if response.drag_started() {
        let near_line = mode == CompareMode::Swipe
            && response
                .interact_pointer_pos()
                .is_some_and(|p| (p.x - (rect.min.x + rect.width() * t.swipe)).abs() < 14.0);
        ui.data_mut(|d| d.insert_temp(swipe_id, near_line));
    }
    if response.dragged() {
        let swiping = ui.data(|d| d.get_temp::<bool>(swipe_id)).unwrap_or(false);
        if swiping {
            if let Some(p) = response.interact_pointer_pos() {
                t.swipe = ((p.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
            }
        } else {
            let (pane, layout) = response
                .interact_pointer_pos()
                .map(pane_at)
                .unwrap_or(panes[0]);
            t.center -= response.drag_delta() / layout.content_size(pane, t.zoom);
        }
    }
    if response.double_clicked() {
        t.zoom = 1.0;
        t.center = egui::vec2(0.5, 0.5);
    }
    clamp(&mut t);
    app.view = t;

    // ---- painting
    let v = app.video.as_ref().unwrap();
    let painter = ui.painter_at(rect);
    let original = v.frame.as_ref().map(|(_, _, tex)| tex.id());
    let result = v
        .result
        .as_ref()
        .filter(|(i, _, _)| *i == v.current)
        .map(|(_, tex, size)| (tex.id(), *size));
    let full_uv = Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0));
    let aspect_note = |p: &egui::Painter, canvas: Rect| {
        if extending {
            p.text(
                canvas.center_bottom() + egui::vec2(0.0, 6.0),
                egui::Align2::CENTER_TOP,
                format!(
                    "{} {}",
                    tr("Utfyllt av AI till", "Extended by the AI to"),
                    app.settings.target_aspect.name()
                ),
                egui::FontId::proportional(12.0),
                Color32::from_gray(150),
            );
        }
    };
    match mode {
        CompareMode::SideBySide => {
            let (l, r) = (panes[0].0, panes[1].0);
            let pl = painter.with_clip_rect(l);
            let pr = painter.with_clip_rect(r);
            if let Some(tex) = original {
                pl.image(tex, source.content_rect(l, &t), full_uv, Color32::WHITE);
            }
            if let Some((tex, _)) = result {
                let canvas = output.canvas_rect(r, &t);
                pr.image(tex, canvas, full_uv, Color32::WHITE);
                aspect_note(&pr, canvas);
            } else {
                result_placeholder(&pr, r, app);
            }
            let x = l.max.x + 2.0;
            painter.line_segment(
                [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
                Stroke::new(2.0, Color32::from_gray(90)),
            );
            badge(
                &painter,
                l.left_top() + egui::vec2(8.0, 8.0),
                &format!(
                    "{} · {}×{}",
                    tr("Original", "Original"),
                    v.info.width,
                    v.info.height
                ),
                false,
            );
            if let Some((_, (w, h))) = result {
                badge(
                    &painter,
                    r.left_top() + egui::vec2(8.0, 8.0),
                    &format!("{} · {w}×{h}", short_provider(app)),
                    true,
                );
            }
        }
        CompareMode::Swipe => {
            // One picture: the result's shape, with the original over its own area.
            let canvas = output.canvas_rect(rect, &t);
            if extending {
                painter.rect_stroke(
                    canvas,
                    0.0,
                    Stroke::new(1.0, Color32::from_gray(110)),
                    egui::StrokeKind::Inside,
                );
            }
            if let Some(tex) = original {
                painter.image(tex, output.content_rect(rect, &t), full_uv, Color32::WHITE);
            }
            // The line stays put on screen while zooming and panning.
            let split_x = rect.min.x + rect.width() * t.swipe;
            if let Some((tex, (w, h))) = result {
                let clip = Rect::from_min_max(egui::pos2(split_x, rect.min.y), rect.max);
                painter
                    .with_clip_rect(clip)
                    .image(tex, canvas, full_uv, Color32::WHITE);
                aspect_note(&painter, canvas);
                badge(
                    &painter,
                    rect.left_top() + egui::vec2(8.0, 8.0),
                    &format!("◀ {}", tr("Original", "Original")),
                    false,
                );
                let label = format!("{} · {w}×{h} ▶", short_provider(app));
                badge_right(&painter, rect.right_top() + egui::vec2(-8.0, 8.0), &label);
            } else {
                let clip = Rect::from_min_max(egui::pos2(split_x, rect.min.y), rect.max);
                painter
                    .with_clip_rect(clip)
                    .rect_filled(rect, 0.0, Color32::from_black_alpha(150));
                result_placeholder(&painter.with_clip_rect(clip), clip, app);
            }
            // The swipe line with a round handle.
            let line_x = split_x.clamp(rect.min.x, rect.max.x);
            painter.line_segment(
                [
                    egui::pos2(line_x, rect.min.y),
                    egui::pos2(line_x, rect.max.y),
                ],
                Stroke::new(2.0, Color32::WHITE),
            );
            let c = egui::pos2(line_x, rect.center().y);
            painter.circle(
                c,
                14.0,
                Color32::from_black_alpha(160),
                Stroke::new(2.0, Color32::WHITE),
            );
            painter.text(
                c,
                egui::Align2::CENTER_CENTER,
                "◀▶",
                egui::FontId::proportional(11.0),
                Color32::WHITE,
            );
            if response
                .hover_pos()
                .is_some_and(|p| (p.x - line_x).abs() < 14.0)
            {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
        }
    }
    if v.requested.is_some() || v.frame.is_none() {
        let p = rect.left_bottom() + egui::vec2(10.0, -26.0);
        painter.text(
            p,
            egui::Align2::LEFT_TOP,
            tr("Läser bildruta…", "Reading frame…"),
            egui::FontId::proportional(13.0),
            Color32::from_gray(200),
        );
    }
    if let Some(e) = &v.frame_error {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            e,
            egui::FontId::proportional(13.0),
            style::BAD,
        );
    }
    if t.zoom > 1.001 {
        let p = rect.right_bottom() + egui::vec2(-10.0, -10.0);
        painter.text(
            p,
            egui::Align2::RIGHT_BOTTOM,
            format!("{:.0} %", t.zoom * 100.0),
            egui::FontId::proportional(12.0),
            Color32::from_gray(200),
        );
    }
}

fn short_provider(app: &UpscalerApp) -> &'static str {
    match app.settings.provider {
        ProviderKind::ChatGptSubscription => "ChatGPT",
        ProviderKind::OpenAiApi => "OpenAI API",
        ProviderKind::Gemini => "Gemini",
        ProviderKind::Flux => "FLUX",
        ProviderKind::Grok => "Grok",
        ProviderKind::RealEsrgan => "Real-ESRGAN",
        ProviderKind::Basic => tr("Enkel", "Basic"),
    }
}

/// How a pane shows a picture: a canvas of `aspect` (width / height) fitted into the pane,
/// with the original's content at `inner` (x, y, width, height as fractions of the canvas).
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub aspect: f32,
    pub inner: (f32, f32, f32, f32),
}

impl Layout {
    /// On-screen size of the original's content at `zoom`.
    pub fn content_size(&self, pane: Rect, zoom: f32) -> Vec2 {
        let fit = if pane.width() / pane.height().max(1.0) > self.aspect {
            egui::vec2(pane.height() * self.aspect, pane.height())
        } else {
            egui::vec2(pane.width(), pane.width() / self.aspect)
        };
        fit * zoom * egui::vec2(self.inner.2, self.inner.3)
    }

    /// Where the original's content is drawn: its point `t.center` sits at the pane centre.
    pub fn content_rect(&self, pane: Rect, t: &ViewTransform) -> Rect {
        let size = self.content_size(pane, t.zoom);
        Rect::from_min_size(pane.center() - t.center * size, size)
    }

    /// Where the whole canvas is drawn.
    pub fn canvas_rect(&self, pane: Rect, t: &ViewTransform) -> Rect {
        let content = self.content_rect(pane, t);
        let size = content.size() / egui::vec2(self.inner.2, self.inner.3);
        Rect::from_min_size(
            content.min - egui::vec2(self.inner.0 * size.x, self.inner.1 * size.y),
            size,
        )
    }
}

/// Zooms by `factor`, keeping the picture point under `cursor` in place.
fn zoom_at(t: &mut ViewTransform, pane: Rect, layout: Layout, cursor: Pos2, factor: f32) {
    let before = layout.content_rect(pane, t);
    let p = (cursor - before.min) / before.size();
    t.zoom = (t.zoom * factor).clamp(1.0, 32.0);
    t.center = p - (cursor - pane.center()) / layout.content_size(pane, t.zoom);
}

fn clamp(t: &mut ViewTransform) {
    if t.zoom <= 1.0 {
        t.zoom = 1.0;
        t.center = egui::vec2(0.5, 0.5);
        return;
    }
    let half = 0.5 / t.zoom;
    t.center.x = t.center.x.clamp(half, 1.0 - half);
    t.center.y = t.center.y.clamp(half, 1.0 - half);
}

fn badge(painter: &egui::Painter, pos: Pos2, text: &str, accent: bool) {
    let galley = painter.layout_no_wrap(
        text.to_string(),
        egui::FontId::proportional(12.5),
        Color32::WHITE,
    );
    let r = Rect::from_min_size(pos, galley.size() + egui::vec2(12.0, 6.0));
    painter.rect_filled(
        r,
        4.0,
        if accent {
            ACCENT.linear_multiply(0.75)
        } else {
            Color32::from_black_alpha(170)
        },
    );
    painter.galley(pos + egui::vec2(6.0, 3.0), galley, Color32::WHITE);
}

fn badge_right(painter: &egui::Painter, right_top: Pos2, text: &str) {
    let galley = painter.layout_no_wrap(
        text.to_string(),
        egui::FontId::proportional(12.5),
        Color32::WHITE,
    );
    let size = galley.size() + egui::vec2(12.0, 6.0);
    let r = Rect::from_min_size(right_top - egui::vec2(size.x, 0.0), size);
    painter.rect_filled(r, 4.0, ACCENT.linear_multiply(0.75));
    painter.galley(r.min + egui::vec2(6.0, 3.0), galley, Color32::WHITE);
}

fn result_placeholder(painter: &egui::Painter, area: Rect, app: &UpscalerApp) {
    let c = area.center();
    let font = egui::FontId::proportional(15.0);
    let small = egui::FontId::proportional(12.5);
    if let Some(p) = &app.preview {
        let secs = p.started.elapsed().as_secs_f64();
        let t = painter.ctx().input(|i| i.time) as f32;
        // A simple rotating arc as a spinner.
        let r = 16.0;
        let pts: Vec<Pos2> = (0..24)
            .map(|k| {
                let a = t * 4.0 + k as f32 * 0.2;
                c + egui::vec2(a.cos(), a.sin()) * r - egui::vec2(0.0, 40.0)
            })
            .collect();
        painter.add(egui::Shape::line(pts, Stroke::new(3.0, ACCENT)));
        painter.text(
            c,
            egui::Align2::CENTER_CENTER,
            format!("{} {}", short_provider(app), tr("arbetar…", "is working…")),
            font,
            Color32::WHITE,
        );
        let est = app.settings.estimate_seconds_per_frame() as f64;
        painter.text(
            c + egui::vec2(0.0, 24.0),
            egui::Align2::CENTER_CENTER,
            format!(
                "{} · {} ~{}",
                format_duration(secs),
                tr("brukar ta", "usually takes"),
                format_duration(est)
            ),
            small,
            Color32::from_gray(190),
        );
    } else if let Some(e) = &app.preview_error {
        let msg: String = e.chars().take(600).collect();
        let galley = painter.layout(
            format!(
                "{}\n\n{msg}",
                tr("Förhandsvisningen misslyckades:", "The preview failed:")
            ),
            small,
            style::BAD,
            (area.width() - 40.0).max(100.0),
        );
        let pos = c - galley.size() / 2.0;
        painter.galley(pos, galley, style::BAD);
    } else {
        painter.text(
            c - egui::vec2(0.0, 12.0),
            egui::Align2::CENTER_CENTER,
            tr(
                "Ingen förhandsvisning för den här bildrutan",
                "No preview for this frame",
            ),
            font,
            Color32::from_gray(210),
        );
        painter.text(
            c + egui::vec2(0.0, 14.0),
            egui::Align2::CENTER_CENTER,
            tr(
                "Klicka \"Skapa förhandsvisning\" (P) för att skala upp den",
                "Click \"Create preview\" (P) to upscale it",
            ),
            small,
            Color32::from_gray(150),
        );
    }
}

fn controls(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    ui.horizontal(|ui| {
        ui.selectable_value(
            &mut app.settings.compare_mode,
            CompareMode::SideBySide,
            tr("Sida vid sida", "Side by side"),
        );
        ui.selectable_value(
            &mut app.settings.compare_mode,
            CompareMode::Swipe,
            format!("↔ {}", tr("Svep", "Swipe")),
        );
        ui.separator();
        if ui
            .button(tr("Anpassa", "Fit"))
            .on_hover_text(tr(
                "Dubbelklicka i bilden gör samma sak",
                "Double-click the picture does the same",
            ))
            .clicked()
        {
            app.view.zoom = 1.0;
            app.view.center = egui::vec2(0.5, 0.5);
        }
        for z in [2.0, 4.0] {
            if ui.button(format!("{:.0}×", z)).clicked() {
                app.view.zoom = z;
            }
        }
        ui.label(
            egui::RichText::new(tr(
                "Scrolla för att zooma, dra för att panorera",
                "Scroll to zoom, drag to pan",
            ))
            .weak()
            .small(),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let busy = app.preview.is_some();
            if busy {
                if ui.button(tr("Avbryt", "Cancel")).clicked() {
                    app.cancel_preview();
                }
                ui.spinner();
            } else {
                let v = app.video.as_ref().unwrap();
                let ready =
                    v.frame.as_ref().is_some_and(|(i, _, _)| *i == v.current) && !app.exporting();
                let has = v.result.as_ref().is_some_and(|(i, _, _)| *i == v.current);
                let text = if has {
                    format!("↻ {}", tr("Skapa ny förhandsvisning", "Create new preview"))
                } else {
                    format!("✨ {}", tr("Skapa förhandsvisning", "Create preview"))
                };
                let hint = format!(
                    "{}: {}\n{} ~{}",
                    tr(
                        "Skalar upp bildrutan under spelhuvudet med",
                        "Upscales the frame under the playhead with"
                    ),
                    provider_name(app.settings.provider),
                    tr("Tar ungefär", "Takes about"),
                    format_duration(app.settings.estimate_seconds_per_frame() as f64)
                );
                if style::primary_button(ui, text, ready)
                    .on_hover_text(hint)
                    .clicked()
                {
                    app.start_preview();
                }
            }
        });
    });
}

fn empty_state(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    let rect = ui.available_rect_before_wrap();
    ui.painter().rect_filled(rect, 8.0, CANVAS);
    let inner = Rect::from_center_size(rect.center(), Vec2::new(rect.width().min(560.0), 260.0));
    ui.painter().rect_stroke(
        inner,
        12.0,
        Stroke::new(2.0, Color32::from_gray(70)),
        egui::StrokeKind::Inside,
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(inner.shrink(24.0)), |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.label(egui::RichText::new("🎞").size(46.0));
            ui.label(
                egui::RichText::new(tr(
                    "Dra och släpp en video här",
                    "Drag and drop a video here",
                ))
                .size(20.0)
                .strong(),
            );
            ui.label(
                egui::RichText::new(tr(
                    "MP4, MOV, MKV, AVI, WebM …",
                    "MP4, MOV, MKV, AVI, WebM …",
                ))
                .weak(),
            );
            ui.add_space(12.0);
            if style::primary_button(ui, tr("Öppna video…", "Open video…"), true).clicked() {
                app.open_dialog();
            }
        });
    });
}
