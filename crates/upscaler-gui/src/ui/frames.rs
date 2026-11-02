//! The frame browser: every finished frame of an export, page by page, with quality
//! control's verdicts. A frame can be rejected by hand; the export makes it again and has it
//! checked again.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use eframe::egui::{self, Color32};
use upscaler_core::job::{self, FrameStatus, JobSummary, ReviewOutcome};

use super::style::{BAD, GOOD, WARN};
use crate::app::UpscalerApp;
use crate::i18n::tr;
use crate::worker::{self, Event, HdrInfo};
use upscaler_core::hdr::convert::HdrStatus;

const PAGE: usize = 24;
const COLUMNS: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    /// Rejected at some point, kept despite problems, unchecked or waiting for a redo.
    Problems,
    /// Kept despite problems or unchecked.
    Kept,
    /// Rejected by hand.
    Manual,
    /// Not finished yet.
    Waiting,
    /// No finished HDR frame (only while the job keeps its HDR frames).
    HdrMissing,
}

impl Filter {
    const ALL: [Filter; 6] = [
        Filter::All,
        Filter::Problems,
        Filter::Kept,
        Filter::Manual,
        Filter::Waiting,
        Filter::HdrMissing,
    ];

    fn label(self) -> &'static str {
        match self {
            Filter::All => tr("Alla", "All"),
            Filter::Problems => tr("Underkända någon gång", "Rejected at some point"),
            Filter::Kept => tr("Behållna trots fel", "Kept despite problems"),
            Filter::Manual => tr("Underkända av mig", "Rejected by me"),
            Filter::Waiting => tr("Väntar", "Waiting"),
            Filter::HdrMissing => tr("HDR saknas", "HDR missing"),
        }
    }

    fn keeps(self, f: &FrameStatus, hdr: Option<&HdrInfo>) -> bool {
        let r = f.record.as_ref();
        let outcome = r.and_then(|r| r.outcome);
        match self {
            Filter::All => true,
            Filter::Problems => {
                f.pending_reject
                    || r.is_some_and(|r| r.rejections > 0)
                    || matches!(
                        outcome,
                        Some(ReviewOutcome::Kept | ReviewOutcome::Unchecked)
                    )
            }
            Filter::Kept => matches!(
                outcome,
                Some(ReviewOutcome::Kept | ReviewOutcome::Unchecked)
            ),
            Filter::Manual => f.pending_reject || r.is_some_and(|r| r.manual),
            Filter::Waiting => !f.done || f.pending_reject,
            Filter::HdrMissing => {
                hdr.is_some_and(|h| h.status.get(&f.n) != Some(&HdrStatus::Ready))
            }
        }
    }
}

pub struct FrameBrowser {
    pub open: bool,
    jobs: Vec<JobSummary>,
    dir: Option<PathBuf>,
    frames: Vec<FrameStatus>,
    loading: bool,
    refreshed: Option<Instant>,
    filter: Filter,
    page: usize,
    thumbs: HashMap<u64, (u64, Option<egui::TextureHandle>)>,
    asked: HashSet<(u64, u64)>,
    selected: Option<u64>,
    big: Option<BigView>,
    hdr: Option<HdrInfo>,
    note: String,
    history: Option<History>,
}

/// Frame number, original, result and the tone-mapped HDR frame of the detail view.
#[derive(Clone)]
struct BigView(
    u64,
    Option<egui::TextureHandle>,
    Option<egui::TextureHandle>,
    Option<egui::TextureHandle>,
    /// The worker's answer has arrived (the images that are `None` could not be read).
    bool,
);

/// The window with the rejected earlier versions of one frame.
struct History {
    n: u64,
    /// Nothing arrived yet.
    loading: bool,
    current: Option<egui::TextureHandle>,
    versions: Vec<(PathBuf, job::Rejection, Option<egui::TextureHandle>)>,
}

impl Default for FrameBrowser {
    fn default() -> Self {
        FrameBrowser {
            open: false,
            jobs: Vec::new(),
            dir: None,
            frames: Vec::new(),
            loading: false,
            refreshed: None,
            filter: Filter::All,
            page: 0,
            thumbs: HashMap::new(),
            asked: HashSet::new(),
            selected: None,
            big: None,
            hdr: None,
            note: String::new(),
            history: None,
        }
    }
}

/// Changes with every new result of a frame, so its thumbnail is loaded again.
fn stamp(f: &FrameStatus) -> u64 {
    f.record
        .as_ref()
        .map(|r| r.attempts as u64 + ((r.rejections as u64) << 16))
        .unwrap_or(0)
        + if f.done { 1 << 32 } else { 0 }
}

impl FrameBrowser {
    /// Forgets everything shown (another project was opened); stays open if it was.
    pub fn reset(&mut self) {
        *self = FrameBrowser {
            open: self.open,
            ..Default::default()
        };
    }

    pub fn show_job(&mut self, dir: PathBuf) {
        if self.dir.as_ref() != Some(&dir) {
            self.dir = Some(dir);
            self.reset_view();
        }
        self.refreshed = None;
    }

    fn reset_view(&mut self) {
        self.frames.clear();
        self.thumbs.clear();
        self.asked.clear();
        self.selected = None;
        self.big = None;
        self.history = None;
        self.page = 0;
    }

    /// Takes the browser's events; returns false for events meant for someone else.
    pub fn handle(&mut self, ctx: &egui::Context, event: &Event) -> bool {
        match event {
            Event::FrameStatuses {
                jobs,
                dir,
                frames,
                hdr,
            } => {
                self.loading = false;
                self.jobs = jobs.clone();
                if &self.dir != dir {
                    self.dir = dir.clone();
                    self.reset_view();
                }
                // A frame with a new result needs its large view again.
                if let (Some(sel), Some(BigView(n, ..))) = (self.selected, &self.big) {
                    let old = self.frames.iter().find(|f| f.n == sel).map(stamp);
                    let new = frames.iter().find(|f| f.n == sel).map(stamp);
                    let hdr_of =
                        |h: &Option<HdrInfo>| h.as_ref().and_then(|h| h.status.get(&sel).copied());
                    if *n == sel && (old != new || hdr_of(&self.hdr) != hdr_of(&hdr)) {
                        self.big = None;
                    }
                }
                self.frames = frames.clone();
                self.hdr = hdr.clone();
                true
            }
            Event::FrameThumb {
                dir,
                n,
                stamp,
                image,
            } => {
                if Some(dir) == self.dir.as_ref() {
                    let tex = image.as_ref().map(|img| {
                        ctx.load_texture(
                            format!("fthumb{n}"),
                            img.clone(),
                            egui::TextureOptions::LINEAR,
                        )
                    });
                    self.thumbs.insert(*n, (*stamp, tex));
                }
                true
            }
            Event::FramePair {
                dir,
                n,
                source,
                result,
                hdr,
            } => {
                if Some(dir) == self.dir.as_ref() && self.selected == Some(*n) {
                    let load = |name: &str, img: &Option<egui::ColorImage>| {
                        img.as_ref().map(|i| {
                            ctx.load_texture(name, i.clone(), egui::TextureOptions::LINEAR)
                        })
                    };
                    self.big = Some(BigView(
                        *n,
                        load("fsrc", source),
                        load("fout", result),
                        load("fhdr", hdr),
                        true,
                    ));
                }
                true
            }
            Event::FrameHistory {
                dir,
                n,
                current,
                versions,
            } => {
                if Some(dir) == self.dir.as_ref()
                    && self.history.as_ref().is_some_and(|h| h.n == *n)
                {
                    let load = |name: String, img: &Option<egui::ColorImage>| {
                        img.as_ref().map(|i| {
                            ctx.load_texture(name, i.clone(), egui::TextureOptions::LINEAR)
                        })
                    };
                    self.history = Some(History {
                        n: *n,
                        loading: false,
                        current: load(format!("hcur{n}"), current),
                        versions: versions
                            .iter()
                            .enumerate()
                            .map(|(i, (p, r, img))| {
                                (p.clone(), r.clone(), load(format!("hrej{n}_{i}"), img))
                            })
                            .collect(),
                    });
                }
                true
            }
            _ => false,
        }
    }
}

pub fn show(ctx: &egui::Context, app: &mut UpscalerApp) {
    if !app.frame_browser.open {
        return;
    }
    // While an export runs, follow its job.
    if let Some(x) = &app.export {
        let dir = x.spec.job_dir();
        if app.frame_browser.dir.as_ref() != Some(&dir) && dir.is_dir() {
            app.frame_browser.show_job(dir);
        }
    }
    let b = &mut app.frame_browser;
    let interval = if app.export.is_some() { 2 } else { 5 };
    if !b.loading
        && b.refreshed
            .is_none_or(|t| t.elapsed().as_secs() >= interval)
    {
        b.loading = true;
        b.refreshed = Some(Instant::now());
        worker::frame_statuses(app.bus.clone(), app.settings.work_dir(), b.dir.clone());
    }
    ctx.request_repaint_after(std::time::Duration::from_millis(500));

    let mut open = true;
    egui::Window::new(tr("Bildrutor", "Frames"))
        .open(&mut open)
        .default_size([1100.0, 760.0])
        .resizable(true)
        .show(ctx, |ui| body(ui, app));
    app.frame_browser.open = open;
    history_window(ctx, &mut app.frame_browser);
}

/// Rejected earlier versions of the double-clicked frame, next to the current result.
fn history_window(ctx: &egui::Context, b: &mut FrameBrowser) {
    let Some(h) = &b.history else { return };
    let mut open = true;
    let mut opened_png = None;
    egui::Window::new(format!(
        "{} #{}",
        tr("Tidigare versioner – bildruta", "Earlier versions – frame"),
        h.n
    ))
    .open(&mut open)
    .default_size([820.0, 560.0])
    .resizable(true)
    .show(ctx, |ui| {
        if h.loading {
            ui.add(egui::Spinner::new());
            return;
        }
        if h.versions.is_empty() {
            ui.label(tr(
                "Inga underkända versioner av den här bildrutan.",
                "No rejected versions of this frame.",
            ));
        }
        egui::ScrollArea::both().show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(tr("Nuvarande resultat", "Current result")).strong(),
                    );
                    match &h.current {
                        Some(t) => {
                            ui.add(egui::Image::new((t.id(), thumb_size(t))));
                        }
                        None => {
                            ui.label(tr("saknas ännu", "not made yet"));
                        }
                    }
                });
                for (path, r, tex) in &h.versions {
                    ui.separator();
                    ui.vertical(|ui| {
                        ui.set_width(360.0);
                        let by = match r.by {
                            job::RejectedBy::Ai => tr("AI", "AI"),
                            job::RejectedBy::User => tr("du", "you"),
                        };
                        ui.label(
                            egui::RichText::new(format!(
                                "{} {} · {} {by}",
                                tr("Försök", "Attempt"),
                                r.attempt,
                                tr("underkänd av", "rejected by"),
                            ))
                            .strong(),
                        );
                        ui.label(egui::RichText::new(&r.time).small().weak());
                        if let Some(t) = tex {
                            let resp = ui
                                .add(
                                    egui::Image::new((t.id(), thumb_size(t)))
                                        .sense(egui::Sense::click()),
                                )
                                .on_hover_text(tr(
                                    "Klicka för att öppna i full storlek",
                                    "Click to open full size",
                                ));
                            if resp.clicked() {
                                opened_png = Some(path.clone());
                            }
                        }
                        ui.label(egui::RichText::new(&r.problems).small().color(WARN));
                    });
                }
            });
        });
    });
    if let Some(p) = opened_png {
        worker::open(&p);
    }
    if !open {
        b.history = None;
    }
}

fn thumb_size(t: &egui::TextureHandle) -> egui::Vec2 {
    let s = t.size_vec2();
    egui::vec2(360.0, 360.0 * s.y / s.x.max(1.0))
}

fn body(ui: &mut egui::Ui, app: &mut UpscalerApp) {
    let bus = app.bus.clone();
    let b = &mut app.frame_browser;
    if b.jobs.is_empty() {
        ui.label(if b.loading {
            tr("Läser…", "Reading…")
        } else {
            tr(
                "Inga exporterade bildrutor i det här projektet ännu.",
                "No exported frames in this project yet.",
            )
        });
        return;
    }

    // Job, filter and counts.
    ui.horizontal(|ui| {
        if b.jobs.len() > 1 {
            let label = |j: &JobSummary| {
                format!(
                    "{} {}–{} ({} {})",
                    tr("Bildruta", "Frame"),
                    j.range.0,
                    j.range.1.saturating_sub(1),
                    j.total,
                    tr("st", "frames")
                )
            };
            let current = b
                .jobs
                .iter()
                .find(|j| Some(&j.dir) == b.dir.as_ref())
                .map(label)
                .unwrap_or_default();
            let mut pick = None;
            egui::ComboBox::from_id_salt("job")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for j in &b.jobs {
                        if ui
                            .selectable_label(Some(&j.dir) == b.dir.as_ref(), label(j))
                            .clicked()
                        {
                            pick = Some(j.dir.clone());
                        }
                    }
                });
            if let Some(d) = pick {
                b.show_job(d);
            }
            ui.separator();
        }
        if b.hdr.is_none() && b.filter == Filter::HdrMissing {
            b.filter = Filter::All;
        }
        for f in Filter::ALL {
            if f == Filter::HdrMissing && b.hdr.is_none() {
                continue;
            }
            let count = b
                .frames
                .iter()
                .filter(|s| f.keeps(s, b.hdr.as_ref()))
                .count();
            if ui
                .selectable_label(b.filter == f, format!("{} ({count})", f.label()))
                .clicked()
            {
                b.filter = f;
                b.page = 0;
            }
        }
    });
    ui.separator();

    let shown: Vec<FrameStatus> = b
        .frames
        .iter()
        .filter(|f| b.filter.keeps(f, b.hdr.as_ref()))
        .cloned()
        .collect();
    let pages = shown.len().div_ceil(PAGE).max(1);
    b.page = b.page.min(pages - 1);
    ui.horizontal(|ui| {
        if ui.add_enabled(b.page > 0, egui::Button::new("⏮")).clicked() {
            b.page = 0;
        }
        if ui.add_enabled(b.page > 0, egui::Button::new("◀")).clicked() {
            b.page -= 1;
        }
        ui.label(format!("{} {} / {pages}", tr("Sida", "Page"), b.page + 1));
        if ui
            .add_enabled(b.page + 1 < pages, egui::Button::new("▶"))
            .clicked()
        {
            b.page += 1;
        }
        if ui
            .add_enabled(b.page + 1 < pages, egui::Button::new("⏭"))
            .clicked()
        {
            b.page = pages - 1;
        }
        ui.label(
            egui::RichText::new(format!("{} {}", shown.len(), tr("bildrutor", "frames"))).weak(),
        );
    });

    let page: Vec<FrameStatus> = shown
        .iter()
        .skip(b.page * PAGE)
        .take(PAGE)
        .cloned()
        .collect();
    // Load the thumbnails this page still needs.
    if let Some(dir) = b.dir.clone() {
        let want: Vec<(u64, u64)> = page
            .iter()
            .filter(|f| f.done)
            .map(|f| (f.n, stamp(f)))
            .filter(|(n, s)| {
                b.thumbs.get(n).is_none_or(|(have, _)| have != s) && !b.asked.contains(&(*n, *s))
            })
            .collect();
        if !want.is_empty() {
            b.asked.extend(want.iter().copied());
            worker::frame_thumbs(bus.clone(), dir, want);
        }
    }

    let detail_height = if b.selected.is_some() { 330.0 } else { 0.0 };
    egui::ScrollArea::vertical()
        .id_salt("frame_grid")
        .max_height((ui.available_height() - detail_height).max(160.0))
        .show(ui, |ui| {
            let w = (ui.available_width() - 8.0 * COLUMNS as f32) / COLUMNS as f32;
            egui::Grid::new("frames_grid")
                .spacing([8.0, 8.0])
                .show(ui, |ui| {
                    for (i, f) in page.iter().enumerate() {
                        tile(ui, b, &bus, f, w);
                        if (i + 1) % COLUMNS == 0 {
                            ui.end_row();
                        }
                    }
                });
        });

    if let Some(n) = b.selected {
        ui.separator();
        detail(ui, b, &bus, n);
    }
}

fn status_text(f: &FrameStatus) -> (Color32, String) {
    if f.pending_reject {
        return (
            WARN,
            tr(
                "Underkänd av dig, görs om",
                "Rejected by you, will be made again",
            )
            .into(),
        );
    }
    if !f.done {
        return match f.record.as_ref().and_then(|r| r.outcome) {
            Some(ReviewOutcome::Rejected) => (
                WARN,
                tr("Underkänd, görs om", "Rejected, being made again").into(),
            ),
            _ => (Color32::GRAY, tr("Inte klar", "Not done").into()),
        };
    }
    let r = f.record.as_ref();
    match r.and_then(|r| r.outcome) {
        Some(ReviewOutcome::Approved) if r.is_some_and(|r| r.rejections > 0) => (
            GOOD,
            format!(
                "{} ({} {})",
                tr("Godkänd", "Approved"),
                tr("efter omgörning", "after a redo"),
                r.map(|r| r.rejections).unwrap_or(0)
            ),
        ),
        Some(ReviewOutcome::Approved) => (GOOD, tr("Godkänd", "Approved").into()),
        Some(ReviewOutcome::Kept) => (
            BAD,
            tr("Behållen trots fel", "Kept despite problems").into(),
        ),
        Some(ReviewOutcome::Unchecked) => (
            WARN,
            tr("Kunde inte granskas", "Could not be checked").into(),
        ),
        Some(ReviewOutcome::Rejected) => (WARN, tr("Underkänd", "Rejected").into()),
        None => (Color32::GRAY, tr("Inte granskad", "Not checked").into()),
    }
}

fn tile(
    ui: &mut egui::Ui,
    b: &mut FrameBrowser,
    bus: &crate::worker::Bus,
    f: &FrameStatus,
    w: f32,
) {
    let h = w * 9.0 / 16.0;
    let selected = b.selected == Some(f.n);
    let (color, text) = status_text(f);
    let frame = egui::Frame::new()
        .stroke(egui::Stroke::new(
            if selected { 2.5 } else { 1.0 },
            if selected {
                ui.visuals().selection.stroke.color
            } else {
                color
            },
        ))
        .corner_radius(4.0)
        .inner_margin(4.0);
    let resp = frame
        .show(ui, |ui| {
            ui.set_width(w - 8.0);
            let tex = b.thumbs.get(&f.n).and_then(|(_, t)| t.clone());
            match tex {
                Some(t) => {
                    let size = t.size_vec2();
                    let iw = w - 8.0;
                    ui.add(egui::Image::new((
                        t.id(),
                        egui::vec2(iw, iw * size.y / size.x),
                    )));
                }
                None => {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(w - 8.0, h), egui::Sense::hover());
                    ui.painter()
                        .rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        if f.done {
                            "…"
                        } else {
                            tr("saknas ännu", "not made yet")
                        },
                        egui::FontId::proportional(13.0),
                        Color32::GRAY,
                    );
                }
            }
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("#{}", f.n)).strong());
                ui.label(egui::RichText::new(text).small().color(color));
                if let Some(h) = &b.hdr {
                    let (badge, color, hint) = match h.status.get(&f.n) {
                        Some(HdrStatus::Ready) => (
                            "HDR",
                            GOOD,
                            tr("HDR-versionen är klar", "The HDR version is ready"),
                        ),
                        _ => (
                            "HDR …",
                            WARN,
                            tr("HDR-versionen görs", "The HDR version is being made"),
                        ),
                    };
                    ui.label(egui::RichText::new(badge).small().color(color))
                        .on_hover_text(hint);
                }
                let earlier = f.record.as_ref().map_or(0, |r| r.history.len());
                if earlier > 0 {
                    ui.label(
                        egui::RichText::new(format!("↺ {earlier}"))
                            .small()
                            .color(WARN),
                    )
                    .on_hover_text(tr(
                        "Tidigare underkända versioner – dubbelklicka för att se dem",
                        "Earlier rejected versions – double-click to see them",
                    ));
                }
            });
        })
        .response
        .interact(egui::Sense::click());
    if resp.double_clicked() {
        // The first click of the pair may have toggled the detail view off.
        if b.selected != Some(f.n) {
            b.selected = Some(f.n);
            b.big = None;
            b.note.clear();
        }
        b.history = Some(History {
            n: f.n,
            loading: true,
            current: None,
            versions: Vec::new(),
        });
        if let Some(dir) = b.dir.clone() {
            worker::frame_history(bus.clone(), dir, f.n);
        }
    } else if resp.clicked() {
        if b.selected == Some(f.n) {
            b.selected = None;
            b.big = None;
        } else {
            b.selected = Some(f.n);
            b.big = None;
            b.note.clear();
        }
    }
    if let Some(p) = f
        .record
        .as_ref()
        .map(|r| r.problems.clone())
        .filter(|p| !p.is_empty())
    {
        resp.on_hover_text(p);
    }
}

fn detail(ui: &mut egui::Ui, b: &mut FrameBrowser, bus: &crate::worker::Bus, n: u64) {
    let Some(dir) = b.dir.clone() else { return };
    let Some(f) = b.frames.iter().find(|f| f.n == n).cloned() else {
        return;
    };
    if b.big.as_ref().is_none_or(|v| v.0 != n) {
        b.big = Some(BigView(n, None, None, None, false));
        worker::frame_pair(bus.clone(), dir.clone(), n);
    }
    let (color, text) = status_text(&f);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("{} #{n}", tr("Bildruta", "Frame"))).strong());
        ui.label(egui::RichText::new(text).color(color));
        if let Some(r) = &f.record {
            ui.label(
                egui::RichText::new(format!(
                    "· {} {} · {} {}",
                    r.attempts,
                    tr("försök", "attempts"),
                    r.rejections,
                    tr("underkännanden", "rejections")
                ))
                .weak(),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("✖").on_hover_text(tr("Stäng", "Close")).clicked() {
                b.selected = None;
                b.big = None;
            }
            let (src, out) = job::job_frame_files(&dir, n);
            if f.done
                && ui
                    .button(tr("Öppna resultatet", "Open the result"))
                    .clicked()
            {
                worker::open(&out);
            }
            if ui
                .button(tr("Öppna originalet", "Open the original"))
                .clicked()
            {
                worker::open(&src);
            }
        });
    });
    if let Some(p) = f
        .record
        .as_ref()
        .map(|r| r.problems.clone())
        .filter(|p| !p.is_empty())
    {
        ui.label(egui::RichText::new(p).small().color(WARN));
    }
    let avail = ui.available_width();
    let hdr_ready = b
        .hdr
        .as_ref()
        .map(|h| h.status.get(&n) == Some(&HdrStatus::Ready));
    let columns = if hdr_ready.is_some() { 3.0 } else { 2.0 };
    let img_w = (avail - 12.0 * columns) / columns;
    ui.horizontal(|ui| {
        let BigView(_, src, out, hdr_img, loaded) =
            b.big.clone().unwrap_or(BigView(n, None, None, None, false));
        for (label, tex) in [
            (tr("Original", "Original"), src),
            (tr("Resultat", "Result"), out),
        ] {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(label).small().weak());
                match tex {
                    Some(t) => {
                        let s = t.size_vec2();
                        let mut size = egui::vec2(img_w, img_w * s.y / s.x);
                        if size.y > 220.0 {
                            size *= 220.0 / size.y;
                        }
                        ui.add(egui::Image::new((t.id(), size)).corner_radius(3.0));
                    }
                    None => {
                        ui.add_sized([img_w, 120.0], egui::Spinner::new());
                    }
                }
            });
        }
        if let Some(ready) = hdr_ready {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(tr("HDR (tonmappad)", "HDR (tone mapped)"))
                        .small()
                        .weak(),
                );
                match &hdr_img {
                    Some(t) => {
                        let s = t.size_vec2();
                        let mut size = egui::vec2(img_w, img_w * s.y / s.x);
                        if size.y > 220.0 {
                            size *= 220.0 / size.y;
                        }
                        ui.add(egui::Image::new((t.id(), size)).corner_radius(3.0));
                    }
                    None if !ready => {
                        ui.add_sized(
                            [img_w, 120.0],
                            egui::Label::new(tr(
                                "HDR-versionen är inte klar",
                                "The HDR version is not ready",
                            )),
                        );
                    }
                    None if loaded => {
                        ui.add_sized(
                            [img_w, 120.0],
                            egui::Label::new(tr(
                                "HDR-versionen kunde inte läsas",
                                "The HDR version could not be read",
                            )),
                        );
                    }
                    None => {
                        ui.add_sized([img_w, 120.0], egui::Spinner::new());
                    }
                }
                ui.label(
                    egui::RichText::new(tr(
                        "Underkänns bildrutan görs både SDR- och HDR-versionen om.",
                        "Rejecting the frame remakes both the SDR and the HDR version.",
                    ))
                    .small()
                    .weak(),
                );
            });
        }
    });
    ui.horizontal(|ui| {
        if f.pending_reject {
            ui.label(
                egui::RichText::new(tr(
                    "Underkänd av dig. Den görs om när exporten körs.",
                    "Rejected by you. It is made again while the export runs.",
                ))
                .color(WARN),
            );
            if ui.button(tr("Ångra", "Undo")).clicked() {
                job::undo_reject(&dir, n);
                b.refreshed = None;
            }
        } else if f.done {
            ui.label(tr("Vad är fel?", "What is wrong?"));
            ui.add(
                egui::TextEdit::singleline(&mut b.note)
                    .hint_text(tr(
                        "till exempel: ansiktet har ändrats, stjärnor på planetens nattsida",
                        "for example: the face changed, stars on the planet's night side",
                    ))
                    .desired_width(ui.available_width() - 200.0),
            );
            if ui
                .button(egui::RichText::new(tr("Underkänn och gör om", "Reject and make again")).color(BAD))
                .on_hover_text(tr(
                    "Bildrutan raderas och görs om av AI:n med din kommentar i prompten, och granskas sedan igen. Bildrutan efter granskas också om. Körs ingen export just nu sker det nästa gång du startar den.",
                    "The frame is deleted and made again by the AI with your comment in the prompt, then checked again. The frame after it is checked again too. If no export is running, this happens the next time you start it.",
                ))
                .clicked()
            {
                if let Err(e) = job::reject_frame(&dir, n, &b.note) {
                    log::warn!("could not reject frame {n}: {e:#}");
                }
                b.note.clear();
                b.refreshed = None;
            }
        }
    });
}
