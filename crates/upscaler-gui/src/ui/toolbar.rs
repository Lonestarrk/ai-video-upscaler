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
        project_menu(ui, app);
        if ui
            .add_enabled(
                app.project.is_some(),
                egui::Button::new(format!("🖼 {}", tr("Bildrutor", "Frames"))),
            )
            .on_hover_text(tr(
                "Bläddra bland exportens färdiga bildrutor, se kvalitetskontrollens omdömen och underkänn bildrutor själv.",
                "Browse the export's finished frames, see quality control's verdicts and reject frames yourself.",
            ))
            .clicked()
        {
            app.frame_browser.open = !app.frame_browser.open;
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
                    "{}{}  ·  {:.3} fps{}  ·  {}  ·  {} {}  ·  {}{}  ·  {}",
                    if (i.source_width, i.source_height) != (i.width, i.height) {
                        format!(
                            "{}×{} ({} {}×{})",
                            i.source_width,
                            i.source_height,
                            tr("visas som", "shown as"),
                            i.width,
                            i.height
                        )
                    } else {
                        format!("{}×{}", i.width, i.height)
                    },
                    "",
                    i.fps(),
                    if i.interlaced {
                        format!(
                            " {}",
                            tr("interlaced, avflätas", "interlaced, deinterlaced")
                        )
                    } else {
                        String::new()
                    },
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
            if app.tools.is_none()
                && ui
                    .button(
                        egui::RichText::new(format!("⚠ {}", tr("FFmpeg saknas", "FFmpeg missing")))
                            .color(super::style::WARN),
                    )
                    .clicked()
                {
                    app.settings_ui.open = true;
                    app.settings_ui.tab = crate::ui::settings_window::TAB_FFMPEG;
                }
        });
    });
    ui.add_space(2.0);
}

fn project_menu(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    let busy = app.exporting();
    ui.menu_button(format!("📁 {}", tr("Projekt", "Project")), |ui| {
        if ui
            .add_enabled(
                !busy,
                egui::Button::new(tr("Öppna projekt…", "Open project…")),
            )
            .on_hover_text(tr(
                "Välj en projektmapp (med project.json) för att fortsätta där du slutade.",
                "Pick a project folder (with project.json) to continue where you left off.",
            ))
            .clicked()
        {
            ui.close();
            app.open_project_dialog();
        }
        let current = app.project.as_ref().map(|p| p.dir.clone());
        let recent: Vec<_> = app
            .settings
            .recent_projects
            .iter()
            .filter(|d| Some(*d) != current.as_ref())
            .cloned()
            .collect();
        ui.menu_button(tr("Senaste projekt", "Recent projects"), |ui| {
            if recent.is_empty() {
                ui.label(egui::RichText::new(tr("Inga ännu", "None yet")).weak());
            }
            for dir in recent {
                let name = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if ui
                    .add_enabled(!busy, egui::Button::new(name))
                    .on_hover_text(dir.display().to_string())
                    .clicked()
                {
                    ui.close();
                    app.open_project(dir);
                }
            }
        });
        ui.separator();
        let has_project = app.project.is_some();
        if ui
            .add_enabled(
                has_project,
                egui::Button::new(tr("Öppna projektmappen", "Open the project folder")),
            )
            .clicked()
        {
            ui.close();
            if let Some(p) = &app.project {
                crate::worker::open(&p.dir);
            }
        }
        if ui
            .add_enabled(
                has_project && !busy && !app.project_moving,
                egui::Button::new(tr("Flytta projektmappen…", "Move the project folder…")),
            )
            .on_hover_text(tr(
                "Flytta projektet med alla bildrutor till en annan disk eller mapp.",
                "Move the project with all its frames to another drive or folder.",
            ))
            .clicked()
        {
            ui.close();
            app.move_project_dialog();
        }
        if let Some(p) = &app.project {
            ui.separator();
            ui.label(
                egui::RichText::new(p.dir.display().to_string())
                    .small()
                    .weak(),
            );
        }
    });
}
