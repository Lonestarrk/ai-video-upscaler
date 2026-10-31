//! The timeline: thumbnails, playhead, in/out range for the export, markers for frames that
//! have a preview, and frame-accurate stepping.

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke};

use super::style::{ACCENT, GOOD};
use crate::app::UpscalerApp;
use crate::i18n::{format_time, tr};

pub fn show(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    ui.add_space(6.0);
    transport(ui, app);
    ui.add_space(4.0);
    strip(ui, app);
    ui.add_space(8.0);
}

fn transport(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    let Some(v) = &app.video else { return };
    let (cur, last, fps) = (
        v.current,
        v.info.frame_count.saturating_sub(1),
        v.info.fps(),
    );
    let sec = fps.round().max(1.0) as u64;
    let mut target: Option<u64> = None;
    let (mut set_in, mut set_out, mut clear) = (false, false, false);
    ui.horizontal(|ui| {
        if ui
            .button("⏮")
            .on_hover_text(tr("Början (Home)", "Start (Home)"))
            .clicked()
        {
            target = Some(0);
        }
        if ui
            .button("⏪")
            .on_hover_text(tr("1 sekund bakåt (Shift+⬅)", "1 second back (Shift+⬅)"))
            .clicked()
        {
            target = Some(cur.saturating_sub(sec));
        }
        if ui
            .button("◀")
            .on_hover_text(tr("Föregående bildruta (⬅)", "Previous frame (⬅)"))
            .clicked()
        {
            target = Some(cur.saturating_sub(1));
        }
        if ui
            .button("▶")
            .on_hover_text(tr("Nästa bildruta (➡)", "Next frame (➡)"))
            .clicked()
        {
            target = Some((cur + 1).min(last));
        }
        if ui
            .button("⏩")
            .on_hover_text(tr(
                "1 sekund framåt (Shift+➡)",
                "1 second forward (Shift+➡)",
            ))
            .clicked()
        {
            target = Some((cur + sec).min(last));
        }
        if ui
            .button("⏭")
            .on_hover_text(tr("Slutet (End)", "End (End)"))
            .clicked()
        {
            target = Some(last);
        }
        ui.separator();
        ui.label(
            egui::RichText::new(format!(
                "{} / {}",
                format_time(v.info.frame_time(cur)),
                format_time(v.info.duration)
            ))
            .monospace()
            .size(14.0),
        );
        ui.label(
            egui::RichText::new(format!(
                "{} {} / {}",
                tr("bildruta", "frame"),
                cur + 1,
                v.info.frame_count
            ))
            .weak()
            .monospace(),
        );
        // Jump to a frame number.
        let mut frame_text = (cur + 1).to_string();
        let edit = ui
            .add(egui::TextEdit::singleline(&mut frame_text).desired_width(70.0))
            .on_hover_text(tr("Gå till bildruta", "Go to frame"));
        if edit.changed() {
            if let Ok(n) = frame_text.trim().parse::<u64>() {
                target = Some(n.saturating_sub(1).min(last));
            }
        }
        ui.separator();
        if ui
            .button(tr("Sätt start [I]", "Set in [I]"))
            .on_hover_text(tr(
                "Exportera från den här bildrutan",
                "Export from this frame",
            ))
            .clicked()
        {
            set_in = true;
        }
        if ui
            .button(tr("Sätt slut [O]", "Set out [O]"))
            .on_hover_text(tr(
                "Exportera till och med den här bildrutan",
                "Export up to and including this frame",
            ))
            .clicked()
        {
            set_out = true;
        }
        if (v.in_point.is_some() || v.out_point.is_some())
            && ui.button(tr("Hela videon", "Whole video")).clicked()
        {
            clear = true;
        }
        let (s, e) = v.range();
        let n = e - s;
        let range_text = if v.in_point.is_some() || v.out_point.is_some() {
            format!(
                "{}: {} – {} ({} {}, {})",
                tr("Urval", "Range"),
                format_time(v.info.frame_time(s)),
                format_time(v.info.frame_time(e)),
                n,
                tr("bildrutor", "frames"),
                format_time(n as f64 / fps)
            )
        } else {
            tr("Exporterar hela videon", "Exporting the whole video").to_string()
        };
        ui.label(egui::RichText::new(range_text).color(ACCENT));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !v.previewed.is_empty() {
                ui.label(
                    egui::RichText::new(format!(
                        "⚫ {} {}",
                        v.previewed.len(),
                        tr("förhandsvisade", "previewed")
                    ))
                    .color(GOOD)
                    .small(),
                );
            }
        });
    });
    if let Some(t) = target {
        app.seek(t);
    }
    if let Some(v) = &mut app.video {
        if set_in {
            v.in_point = Some(v.current);
            if v.out_point.is_some_and(|o| o < v.current) {
                v.out_point = None;
            }
        }
        if set_out {
            v.out_point = Some(v.current);
            if v.in_point.is_some_and(|i| i > v.current) {
                v.in_point = None;
            }
        }
        if clear {
            v.in_point = None;
            v.out_point = None;
        }
    }
}

fn strip(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    let height = 64.0;
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height + 14.0),
        Sense::click_and_drag(),
    );
    let Some(v) = &app.video else { return };
    let painter = ui.painter_at(rect);
    let strip = Rect::from_min_size(
        rect.min + egui::vec2(0.0, 12.0),
        egui::vec2(rect.width(), height),
    );
    painter.rect_filled(strip, 4.0, Color32::from_gray(20));
    let frames = v.info.frame_count.max(1) as f32;
    let x_of = |f: f64| strip.min.x + (f as f32 / frames) * strip.width();

    // Thumbnails, each cropped to its tile's shape.
    let n = v.thumbs.len().max(1);
    let tile_w = strip.width() / n as f32;
    let aspect = v.info.width as f32 / v.info.height.max(1) as f32;
    for (i, t) in v.thumbs.iter().enumerate() {
        let tile = Rect::from_min_size(
            egui::pos2(strip.min.x + i as f32 * tile_w, strip.min.y),
            egui::vec2(tile_w, height),
        );
        if let Some(tex) = t {
            let tile_aspect = tile_w / height;
            let uv = if tile_aspect < aspect {
                let w = tile_aspect / aspect;
                Rect::from_min_max(
                    egui::pos2(0.5 - w / 2.0, 0.0),
                    egui::pos2(0.5 + w / 2.0, 1.0),
                )
            } else {
                let h = aspect / tile_aspect;
                Rect::from_min_max(
                    egui::pos2(0.0, 0.5 - h / 2.0),
                    egui::pos2(1.0, 0.5 + h / 2.0),
                )
            };
            painter.image(
                tex.id(),
                tile.shrink2(egui::vec2(0.5, 0.0)),
                uv,
                Color32::WHITE,
            );
        }
    }

    // Outside the export range is dimmed.
    let (s, e) = v.range();
    if v.in_point.is_some() || v.out_point.is_some() {
        let (xs, xe) = (x_of(s as f64), x_of(e as f64));
        let dim = Color32::from_black_alpha(170);
        painter.rect_filled(
            Rect::from_min_max(strip.min, egui::pos2(xs, strip.max.y)),
            0.0,
            dim,
        );
        painter.rect_filled(
            Rect::from_min_max(egui::pos2(xe, strip.min.y), strip.max),
            0.0,
            dim,
        );
        painter.rect_stroke(
            Rect::from_min_max(egui::pos2(xs, strip.min.y), egui::pos2(xe, strip.max.y)),
            2.0,
            Stroke::new(2.0, ACCENT),
            egui::StrokeKind::Inside,
        );
    }

    // Previewed frames.
    for &f in &v.previewed {
        let x = x_of(f as f64 + 0.5);
        let tri = vec![
            egui::pos2(x - 5.0, rect.min.y + 2.0),
            egui::pos2(x + 5.0, rect.min.y + 2.0),
            egui::pos2(x, rect.min.y + 10.0),
        ];
        painter.add(egui::Shape::convex_polygon(tri, GOOD, Stroke::NONE));
    }

    // Hover time.
    if let Some(p) = response.hover_pos() {
        let f = (((p.x - strip.min.x) / strip.width()).clamp(0.0, 1.0) * frames) as f64;
        painter.line_segment(
            [egui::pos2(p.x, strip.min.y), egui::pos2(p.x, strip.max.y)],
            Stroke::new(1.0, Color32::from_white_alpha(90)),
        );
        response
            .clone()
            .on_hover_text_at_pointer(format_time(f / v.info.fps()));
    }

    // Playhead.
    let x = x_of(v.current as f64 + 0.5);
    painter.line_segment(
        [egui::pos2(x, rect.min.y + 8.0), egui::pos2(x, strip.max.y)],
        Stroke::new(2.0, Color32::WHITE),
    );
    let head = vec![
        Pos2::new(x - 7.0, rect.min.y),
        Pos2::new(x + 7.0, rect.min.y),
        Pos2::new(x, rect.min.y + 11.0),
    ];
    painter.add(egui::Shape::convex_polygon(
        head,
        Color32::WHITE,
        Stroke::NONE,
    ));

    if response.clicked() || response.dragged() {
        if let Some(p) = response.interact_pointer_pos() {
            let f = (((p.x - strip.min.x) / strip.width()).clamp(0.0, 1.0) * frames) as u64;
            app.seek(f);
        }
    }
}
