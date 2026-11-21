use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dither_gpui::dither_core::Tone;
use dither_gpui::dither_core::ease::approach;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Disableable, Icon, Sizable, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::bar::{self, Bar, Particle, Rng, Segment};
use crate::clutter::{self, Item, Pick};
use crate::disk::{self, Category, Progress, Report, Volume};
use crate::sound::{Sound, Ticker};
use crate::treemap::{self, Rect};
use crate::ui::{self, s, z};

mod apps;
mod confirm;
mod docker_panel;
mod onboarding;
mod scroll;
mod tip;
mod update;
mod your_files;

const BG: u32 = 0x161618;
const FG: u32 = 0xededef;
const MUTED: u32 = 0x8b8b90;
/// Caution: what wasn't cleared, Docker's log warning, data that can't come back.
const WARN: u32 = 0xe8a84a;
const LINE: u32 = 0x26262a;
const OK: u32 = 0x4ccb76;

/// Items under this share of the treemap join the "N more" tile.
const SMALL: f64 = 0.03;
/// Zoomed in, the map stretches until the "N more" tile is this share of
/// its width and height.
const ZOOM_SHARE: (f32, f32) = (0.4, 0.6);
/// But never more than this, so the tiles around it stay in view.
const ZOOM_MAX: f32 = 4.0;

fn tone(category: Category) -> Tone {
    match category {
        Category::Apps => Tone::Blue,
        Category::Developer => Tone::Green,
        Category::Photos => Tone::Pink,
        Category::Movies => Tone::Purple,
        Category::Documents => Tone::Hue(205.0),
        Category::Music => Tone::Hue(95.0),
        Category::System => Tone::Grey,
        Category::Caches => Tone::Orange,
        Category::Trash => Tone::Red,
    }
}

fn icon(category: Category) -> &'static str {
    match category {
        Category::Apps => "apps",
        Category::Developer => "developer",
        Category::Photos => "photos",
        Category::Movies => "movies",
        Category::Documents => "documents",
        Category::Music => "music",
        Category::System => "system",
        Category::Caches => "caches",
        Category::Trash => "trash",
    }
}

/// One of the app's two-layer icons from `assets/icons`: a dark body with a
/// bright detail on top, both in `tone`.
fn duo(name: &'static str, tone: Tone, size: f32) -> Div {
    let layer = |part: &str, color| {
        svg()
            .path(SharedString::from(format!("icons/{name}.{part}.svg")))
            .absolute()
            .size_full()
            .text_color(dither_gpui::hsla(color))
    };
    div()
        .relative()
        .flex_none()
        .size(z(size))
        .child(layer("base", tone.shade(0.22)))
        .child(layer("accent", tone.base()))
}

/// A big figure and a smaller note on one baseline. GPUI's flex can't align
/// text baselines, so both sit on the bottom edge and the note is lifted by
/// the difference. Figtree's ascent and descent are 0.95 and 0.25 em, and
/// GPUI centers that 1.2 em box in the line, so with lines of 1.3 em the
/// baseline sits 0.3 em above the bottom.
fn figure(
    big: impl Into<SharedString>,
    big_size: f32,
    note: impl Into<SharedString>,
    note_size: f32,
) -> Div {
    const LINE: f32 = 1.3;
    const ABOVE_BOTTOM: f32 = LINE / 2.0 - (0.95 - 0.25) / 2.0;
    h_flex()
        .items_end()
        .gap_1p5()
        .child(
            div()
                .text_size(z(big_size))
                .line_height(relative(LINE))
                .font_weight(FontWeight::MEDIUM)
                .child(big.into()),
        )
        .child(
            div()
                .text_size(z(note_size))
                .line_height(relative(LINE))
                .pb(z((big_size - note_size) * ABOVE_BOTTOM))
                .text_color(rgb(MUTED))
                .child(note.into()),
        )
}

fn gb(bytes: f64) -> String {
    // Easing can dip a hair below zero; never show "-0.0 GB".
    format!("{:.1} GB", (bytes / 1e9).max(0.0) + 0.0)
}

/// Disk capacity reads like a product spec: "512 GB", "1 TB".
fn capacity(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb >= 1000.0 {
        format!("{} TB", (gb / 100.0).round() / 10.0)
    } else {
        format!("{} GB", gb.round())
    }
}

fn tilde(path: &str) -> String {
    let home = disk::home();
    let home = home.to_string_lossy();
    match path.strip_prefix(home.as_ref()) {
        Some(rest) if home != "/" => format!("~{rest}"),
        _ => path.to_string(),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Caches,
    Files,
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Scanning,
    Idle,
    Clearing,
}

pub struct Card {
    demo: bool,
    autoplay: bool,
    volume: Volume,
    progress: Arc<Progress>,
    phase: Phase,
    report: Report,
    /// Bytes per category: where the worker says they are, and what is on screen.
    target: [f64; 9],
    shown: [f64; 9],
    glow: [f32; 9],
    edge_strength: f32,
    hover: Option<Category>,
    freed: u64,
    /// Cleanup items once detection finishes, with what the UI tracks per item.
    items: Option<Arc<Vec<Item>>>,
    selected: Vec<bool>,
    /// Bytes on screen per item, easing toward what is left of it.
    item_shown: Vec<f64>,
    item_glow: Vec<f32>,
    /// Each item's size when found.
    item_found: Vec<f64>,
    /// What fixes the treemap's shape: the size when found, or 0 for items
    /// that aren't tiles.
    item_size: Vec<f64>,
    /// Size of all items when found.
    items_total: f64,
    hover_item: Option<usize>,
    /// Items too small for a readable tile, shown as one "N more" tile.
    grouped: Vec<usize>,
    group_glow: f32,
    hover_group: bool,
    /// Whether the map is zoomed into the "N more" tile, and how far it is
    /// on screen, 0 to 1.
    zoomed: bool,
    zoom: f32,
    /// The developer caches row: its glow, and whether it is hovered.
    dev_glow: f32,
    dev_hover: bool,
    /// Items waiting for the user to confirm clearing them.
    confirm: Option<confirm::Confirm>,
    confirm_scroll: scroll::Scroll,
    /// When the items arrived, for their staggered sweep in.
    items_at: f32,
    clean_bounds: Rc<Cell<Bounds<Pixels>>>,
    particles: Vec<Particle>,
    rng: Rng,
    time: f32,
    last_frame: Instant,
    /// Whether the previous frame asked for another. If not, the time since
    /// it is idle time, not animation time.
    animating: bool,
    bar_bounds: Rc<Cell<Bounds<Pixels>>>,
    legend_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// The legend's hover highlight glides between rows: x, y, width relative
    /// to the legend, and opacity.
    pill: [f32; 3],
    pill_velocity: [f32; 3],
    pill_alpha: f32,
    sound: Sound,
    ticker: Ticker,
    /// The item being cleared last frame, to whoosh when it is done.
    was_cleaning: u64,
    /// What the last clear couldn't remove, and why.
    kept: Vec<String>,
    /// First launch on macOS: which guarded folders to scan, before the scan.
    onboarding: Option<onboarding::Onboarding>,
    /// The panel for removing apps.
    apps: apps::Apps,
    /// The Docker panel, when the docker CLI is installed.
    docker: Option<docker_panel::DockerPanel>,
    /// Whether the next scan to finish reads Docker and the apps again in
    /// the background: the first one, and every Rescan.
    reload_panels: bool,
    /// Holds keyboard focus so Escape reaches the card.
    focus: FocusHandle,
    /// Which list fills the bottom: the caches, or the user's own files.
    view: View,
    files: your_files::FilesView,
    /// Linux: a newer release, once found.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    update: Option<update::Update>,
}

/// Legend geometry, shared by the layout and the gliding highlight.
const ROW_H: f32 = 22.0;
const ROW_GAP: f32 = 2.0;
const COL_GAP: f32 = 12.0;

/// Stiffness of the highlight's glide. Settles in about 0.2s.
const PILL_OMEGA: f32 = 30.0;

/// One step of a critically damped spring, solved exactly so any frame time
/// is stable. `x` is the offset from the target. Keeping velocity across
/// steps means a new target mid-glide bends the path instead of jerking it.
fn spring(x: f32, v: f32, dt: f32) -> (f32, f32) {
    let w = PILL_OMEGA;
    let c = v + w * x;
    let e = (-w * dt).exp();
    ((x + c * dt) * e, (v - w * c * dt) * e)
}

impl Card {
    pub fn new(demo: bool, autoplay: bool, cx: &mut Context<Self>) -> Self {
        let mut card = Self {
            demo,
            autoplay,
            volume: Volume { total: 0, used: 0 },
            progress: Arc::default(),
            phase: Phase::Idle,
            report: Report::default(),
            target: [0.0; 9],
            shown: [0.0; 9],
            glow: [1.0; 9],
            edge_strength: 0.0,
            hover: None,
            freed: 0,
            items: None,
            selected: Vec::new(),
            item_shown: Vec::new(),
            item_glow: Vec::new(),
            item_found: Vec::new(),
            item_size: Vec::new(),
            items_total: 0.0,
            hover_item: None,
            grouped: Vec::new(),
            group_glow: 0.0,
            hover_group: false,
            zoomed: false,
            zoom: 0.0,
            dev_glow: 0.0,
            dev_hover: false,
            confirm: None,
            confirm_scroll: Default::default(),
            items_at: 0.0,
            clean_bounds: Rc::default(),
            particles: Vec::new(),
            rng: Rng::new(),
            time: 0.0,
            last_frame: Instant::now(),
            animating: false,
            bar_bounds: Rc::default(),
            legend_bounds: Rc::default(),
            pill: [0.0; 3],
            pill_velocity: [0.0; 3],
            pill_alpha: 0.0,
            sound: Sound::new(),
            ticker: Ticker::default(),
            was_cleaning: 0,
            kept: Vec::new(),
            onboarding: None,
            apps: Default::default(),
            docker: if demo {
                Some("docker".into())
            } else {
                crate::docker::find()
            }
            .map(|bin| docker_panel::DockerPanel::new(bin, demo)),
            reload_panels: true,
            update: None,
            view: View::Caches,
            files: Default::default(),
            focus: cx.focus_handle(),
        };
        if cfg!(not(target_os = "macos")) && !demo {
            card.check_update(cx);
        }
        // Autoplay waits a moment so a screen recording catches the scan.
        if !autoplay && !card.onboard(cx) {
            card.scan(cx);
        }
        card
    }

    fn scan(&mut self, cx: &mut Context<Self>) {
        self.volume = if self.demo {
            disk::demo::VOLUME
        } else {
            disk::volume()
        };
        self.progress = Arc::default();
        self.phase = Phase::Scanning;
        self.report = Report::default();
        self.target = [0.0; 9];
        self.shown = [0.0; 9];
        self.hover = None;
        self.hover_item = None;
        self.hover_group = false;
        self.zoomed = false;
        self.zoom = 0.0;
        self.items = None;
        self.item_shown.clear();
        self.files = Default::default();
        self.freed = 0;
        if self.demo {
            disk::demo::scan(self.progress.clone());
        } else {
            disk::scan(self.progress.clone());
        }
        self.watch(cx);
    }

    /// Redraws while a worker runs and nothing else does. Frames only run
    /// while something animates, so without this a scan that settles before
    /// its items are found would show them only on the next mouse move.
    fn watch(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let waiting = this.update(cx, |this, cx| {
                    let waiting = this.phase != Phase::Idle
                        || (this.items.is_none() && this.progress.items().is_none());
                    if !this.animating {
                        cx.notify();
                    }
                    waiting
                });
                if !matches!(waiting, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    fn clear(&mut self, chosen: Vec<clutter::Chosen>, cx: &mut Context<Self>) {
        let Some(items) = self.items.clone() else {
            return;
        };
        self.phase = Phase::Clearing;
        self.kept.clear();
        self.hover = None;
        self.hover_item = None;
        self.hover_group = false;
        self.dev_hover = false;
        clutter::clean(items, chosen, self.progress.clone(), self.demo);
        self.watch(cx);
    }

    fn clear_selected(&mut self, cx: &mut Context<Self>) {
        self.ask_clear(|this, i| this.selected[i] && !this.is_dev(i), cx);
    }

    fn is_dev(&self, i: usize) -> bool {
        self.items
            .as_ref()
            .is_some_and(|items| items[i].pick == Pick::Dev)
    }

    /// Treemap items still worth a tile: everything until cleared away.
    fn visible(&self) -> Vec<usize> {
        let Some(items) = &self.items else {
            return Vec::new();
        };
        (0..items.len())
            .filter(|&i| !self.is_dev(i) && (items[i].left() > 0 || self.item_shown[i] > 1e6))
            .collect()
    }

    /// Developer caches, largest first.
    fn dev_items(&self) -> Vec<usize> {
        (0..self.item_shown.len())
            .filter(|&i| self.is_dev(i))
            .collect()
    }

    fn selected_bytes(&self) -> f64 {
        self.visible()
            .into_iter()
            .filter(|&i| self.selected[i])
            .map(|i| self.item_shown[i])
            .sum()
    }

    /// How full the cleanup bar is on screen, 0 to 1.
    fn items_fill(&self) -> f64 {
        self.item_shown.iter().sum::<f64>() / self.items_total.max(1.0)
    }

    fn used_shown(&self) -> f64 {
        self.shown.iter().sum()
    }

    /// Seconds since the last frame, and advances the clock.
    fn frame_time(&mut self) -> f32 {
        let now = Instant::now();
        // The first frame after idle (a hover starting a glide) would see
        // seconds of elapsed time; use one display frame so it starts smooth.
        let dt = if self.animating {
            (now - self.last_frame).as_secs_f32().min(1.0 / 20.0)
        } else {
            1.0 / 120.0
        };
        self.last_frame = now;
        self.time += dt;
        dt
    }

    /// Samples the worker, eases every animated value toward its target and
    /// says whether another frame is needed.
    fn tick(&mut self, cx: &mut Context<Self>) -> bool {
        let dt = self.frame_time();
        if let Some(onboarding) = &mut self.onboarding {
            return onboarding.tick(dt);
        }

        let working = self.phase != Phase::Idle;
        if working {
            for c in Category::ALL {
                // System data is worked out when the scan ends; after that only
                // clearing (Docker) changes it.
                if c != Category::System || self.phase == Phase::Clearing {
                    self.target[c as usize] = self.progress.bytes(c) as f64;
                }
            }
            self.freed = self.progress.freed();
        }
        if self.progress.is_done() {
            match self.phase {
                Phase::Scanning => {
                    self.report = self.progress.report();
                    let known: f64 = self.target.iter().sum();
                    let system = (self.volume.used as f64 - known).max(0.0);
                    self.target[Category::System as usize] = system;
                    self.progress.set_system(system as u64);
                    self.phase = Phase::Idle;
                    // After the scan, so the reads don't compete with it.
                    if std::mem::take(&mut self.reload_panels) {
                        self.load_apps(cx);
                        self.reread_docker(cx);
                    }
                }
                Phase::Clearing => {
                    // What was cleared is gone; what was kept stays, unselected.
                    if let Some(items) = &self.items {
                        for (i, item) in items.iter().enumerate() {
                            if self.selected[i] && item.left() > 0 {
                                self.selected[i] = false;
                            }
                        }
                    }
                    self.kept = self.progress.kept();
                    self.phase = Phase::Idle;
                }
                Phase::Idle => {}
            }
        }

        let total = (self.volume.total as f64).max(1.0);
        let used_before = self.used_shown() / total;
        let items_before = self.items_fill();

        let mut moving = false;
        for i in 0..9 {
            let (from, to) = (self.shown[i], self.target[i]);
            self.shown[i] = approach(from as f32, to as f32, dt, 0.09) as f64;
            if (self.shown[i] - to).abs() < 5e6 {
                self.shown[i] = to;
            } else {
                moving = true;
            }

            let category = Category::ALL[i];
            let glow_to = match (self.phase, self.hover) {
                (Phase::Idle, Some(h)) if h == category => 1.0,
                (Phase::Idle, Some(_)) => 0.15,
                (Phase::Idle, None) => 0.45,
                _ => 1.0,
            };
            self.glow[i] = approach(self.glow[i], glow_to, dt, 0.07);
            if (self.glow[i] - glow_to).abs() > 0.005 {
                moving = true;
            } else {
                self.glow[i] = glow_to;
            }
        }

        // Gliding highlight: slide to the hovered row; when nothing is
        // hovered, fade out in place. Appearing from nothing jumps there
        // first so it never slides in from a stale spot.
        let legend_w = f32::from(self.legend_bounds.get().size.width);
        let hovered = self
            .hover
            .filter(|_| self.phase == Phase::Idle && legend_w > 0.0);
        if let Some(c) = hovered {
            let i = c as usize;
            let w = (legend_w - 2.0 * s(COL_GAP)) / 3.0;
            let to = [
                (i % 3) as f32 * (w + s(COL_GAP)),
                (i / 3) as f32 * s(ROW_H + ROW_GAP),
                w,
            ];
            for k in 0..3 {
                if self.pill_alpha < 0.02 {
                    self.pill[k] = to[k];
                    self.pill_velocity[k] = 0.0;
                    continue;
                }
                let (x, v) = spring(self.pill[k] - to[k], self.pill_velocity[k], dt);
                if x.abs() > 0.05 || v.abs() > 1.0 {
                    self.pill[k] = to[k] + x;
                    self.pill_velocity[k] = v;
                    moving = true;
                } else {
                    self.pill[k] = to[k];
                    self.pill_velocity[k] = 0.0;
                }
            }
        }
        let alpha_to = if hovered.is_some() { 1.0 } else { 0.0 };
        self.pill_alpha = approach(self.pill_alpha, alpha_to, dt, 0.06);
        if (self.pill_alpha - alpha_to).abs() > 0.005 {
            moving = true;
        } else {
            self.pill_alpha = alpha_to;
        }

        if self.items.is_none()
            && self.phase == Phase::Idle
            && let Some(items) = self.progress.items()
        {
            self.selected = items.iter().map(|i| i.pick == Pick::On).collect();
            self.item_shown = vec![0.0; items.len()];
            self.item_glow = vec![0.0; items.len()];
            self.item_found = items.iter().map(|i| i.left() as f64).collect();
            self.items_total = self.item_found.iter().sum();
            // Developer caches get their own row, not a tile.
            self.item_size = items
                .iter()
                .map(|i| match i.pick {
                    Pick::Dev => 0.0,
                    _ => i.left() as f64,
                })
                .collect();
            let tiles: f64 = self.item_size.iter().sum();
            self.grouped = (0..items.len())
                .filter(|&i| self.item_size[i] > 0.0 && self.item_size[i] < tiles * SMALL)
                .collect();
            // One small item reads fine as its own tile.
            if self.grouped.len() < 2 {
                self.grouped.clear();
            }
            self.items_at = self.time;
            self.items = Some(items);
            self.start_files();
        }
        if self.items.is_some() {
            let busy_dev = self
                .dev_items()
                .into_iter()
                .any(|i| busy(self.progress.cleaning(), i));
            let glow_to = if busy_dev || self.dev_hover {
                1.0
            } else {
                0.62
            };
            self.dev_glow = approach(self.dev_glow, glow_to, dt, 0.06);
            if (self.dev_glow - glow_to).abs() > 0.005 {
                moving = true;
            } else {
                self.dev_glow = glow_to;
            }
        }
        if !self.grouped.is_empty() {
            let on = self.grouped.iter().any(|&i| self.selected[i]);
            let glow_to = if self
                .grouped
                .iter()
                .any(|&i| busy(self.progress.cleaning(), i))
            {
                1.0
            } else if self.hover_group {
                if on { 1.0 } else { 0.6 }
            } else if !on {
                0.06
            } else if self.hover_item.is_some() {
                0.3
            } else {
                0.62
            };
            self.group_glow = approach(self.group_glow, glow_to, dt, 0.06);
            if (self.group_glow - glow_to).abs() > 0.005 {
                moving = true;
            } else {
                self.group_glow = glow_to;
            }
        }
        if self.autoplay && self.play_script(cx) {
            moving = true;
        }
        let cleaning = self.progress.cleaning();
        if let Some(items) = &self.items {
            for (i, item) in items.iter().enumerate() {
                let to = item.left() as f64;
                // Items sweep in one after another, left to right.
                let waiting = self.time - self.items_at < i as f32 * 0.035;
                if waiting {
                    moving = true;
                } else if self.item_shown[i] != to {
                    self.item_shown[i] =
                        approach(self.item_shown[i] as f32, to as f32, dt, 0.08) as f64;
                    if (self.item_shown[i] - to).abs() < 2e6 {
                        self.item_shown[i] = to;
                    } else {
                        moving = true;
                    }
                }
                let glow_to = match (self.hover_item, busy(cleaning, i)) {
                    (_, true) => 1.0,
                    (Some(h), _) if h == i && !self.selected[i] => 0.6,
                    _ if !self.selected[i] => 0.06,
                    (Some(h), _) if h == i => 1.0,
                    (Some(_), _) => 0.3,
                    _ if self.hover_group => 0.3,
                    _ => 0.62,
                };
                self.item_glow[i] = approach(self.item_glow[i], glow_to, dt, 0.06);
                if (self.item_glow[i] - glow_to).abs() > 0.005 {
                    moving = true;
                } else {
                    self.item_glow[i] = glow_to;
                }
            }
        }
        // One ticker for both bars, following whichever moved more, so
        // clearing (which shrinks both) does not tick twice.
        let used_after = self.used_shown() / total;
        let items_after = self.items_fill();
        let (delta, fill) = if (items_after - items_before).abs() > (used_after - used_before).abs()
        {
            (items_after - items_before, items_after)
        } else {
            (used_after - used_before, used_after)
        };
        self.ticker.step(&self.sound, delta as f32, dt, fill as f32);
        // An item that just finished.
        if self.was_cleaning & !cleaning != 0 {
            self.sound.whoosh();
        }
        self.was_cleaning = cleaning;

        // Keep frames coming while detection runs, so its result shows up.
        if self.items.is_none() && self.phase == Phase::Idle {
            moving = true;
        }

        let edge_to = if working { 1.0 } else { 0.0 };
        self.edge_strength = approach(self.edge_strength, edge_to, dt, 0.12);
        if (self.edge_strength - edge_to).abs() > 0.005 {
            moving = true;
        }

        // Dust rises off every tile being cleared.
        let mut emit = Vec::new();
        if cleaning != 0
            && let Some(items) = &self.items
        {
            let (tiles, group) = self.tiles();
            for (i, t) in tiles.into_iter().enumerate() {
                if busy(cleaning, i) && t.w > 2.0 && t.h > 2.0 {
                    emit.push(([t.x, t.y, t.x + t.w, t.y + t.h], items[i].tone));
                }
            }
            if let Some(t) = group
                && self.zoom == 0.0
                && self.grouped.iter().any(|&i| busy(cleaning, i))
            {
                emit.push(([t.x, t.y, t.x + t.w, t.y + t.h], Tone::Grey));
            }
        }
        // Spread the dust so many tiles at once don't flood the screen.
        let rate = 260.0 / (emit.len().max(1) as f32).sqrt();
        bar::step_particles(&mut self.particles, &mut self.rng, dt, &emit, rate);

        if self.apps.shown && self.apps.tick(dt) {
            moving = true;
        }
        if let Some(docker) = &mut self.docker
            && docker.shown
            && docker.tick(dt)
        {
            moving = true;
        }
        if self.confirm.is_some() && self.confirm_scroll.tick(dt) {
            moving = true;
        }
        if self.zoomed && self.group_members().is_empty() {
            self.zoomed = false;
        }
        let zoom_to = if self.zoomed { 1.0 } else { 0.0 };
        self.zoom = approach(self.zoom, zoom_to, dt, 0.08);
        if (self.zoom - zoom_to).abs() > 0.001 {
            moving = true;
        } else {
            self.zoom = zoom_to;
        }

        if self.files_tick(dt) {
            moving = true;
        }

        working || moving || !self.particles.is_empty()
    }

    /// `--autoplay`: scans after a pause, hovers a few items once they are
    /// in, clears, then tours "Your files". Returns whether the script is still waiting to act.
    fn play_script(&mut self, cx: &mut Context<Self>) -> bool {
        const HOVERS: [usize; 3] = [0, 3, 1];
        const HOLD: f32 = 0.6;
        if self.volume.total == 0 {
            if self.time > 2.0 {
                self.scan(cx);
            }
            return true;
        }
        if self.phase != Phase::Idle || self.items.is_none() {
            return false;
        }
        if self.freed > 0 {
            return self.play_files();
        }
        let t = self.time - self.items_at - 1.4;
        if t < 0.0 {
            return true;
        }
        match HOVERS.get((t / HOLD) as usize) {
            Some(&i) => {
                self.hover_item = Some(i);
                true
            }
            None => {
                self.clear_selected(cx);
                false
            }
        }
    }

    /// What each tile's area follows: the item's size until its intro is
    /// done, then what is on screen, so a cleared tile shrinks away.
    fn weights(&self) -> Vec<f64> {
        let Some(items) = &self.items else {
            return Vec::new();
        };
        items
            .iter()
            .zip(&self.item_shown)
            .map(|(item, &shown)| match item.pick {
                Pick::Dev => 0.0,
                _ => shown.max(item.left() as f64),
            })
            .collect()
    }

    /// Tile rectangles in the treemap's own coordinates, and the "N more"
    /// tile that stands in for the grouped items. Zooming in grows that tile
    /// and lays its items out inside it, while the map widens past its left
    /// edge so the big tiles slide partly out of view.
    fn tiles(&self) -> (Vec<Rect>, Option<Rect>) {
        let size = self.clean_bounds.get().size;
        let area = Rect {
            x: 0.0,
            y: 0.0,
            w: f32::from(size.width),
            h: f32::from(size.height),
        };
        let mut initial = self.item_size.clone();
        let mut weight = self.weights();
        if self.grouped.is_empty() {
            return (treemap::layout(&initial, &weight, area), None);
        }
        let member_initial: Vec<f64> = self.grouped.iter().map(|&i| initial[i]).collect();
        let member_weight: Vec<f64> = self.grouped.iter().map(|&i| weight[i]).collect();
        for &i in &self.grouped {
            initial[i] = 0.0;
            weight[i] = 0.0;
        }
        // Last, as the smallest: items are sorted largest first.
        initial.push(member_initial.iter().sum());
        weight.push(member_weight.iter().sum());
        let mut tiles = treemap::layout(&initial, &weight, area);
        let Some(mut group) = tiles.pop() else {
            return (tiles, None);
        };
        if self.zoom > 0.0 {
            // A camera over the unchanged layout: stretching the map keeps
            // every tile in place relative to the others, so the zoom glides.
            let z = self.zoom;
            let axis = |at: f32, len: f32, full: f32, share: f32| {
                let scale = 1.0 + ((share * full / len.max(1.0)).clamp(1.0, ZOOM_MAX) - 1.0) * z;
                // Centre the group, without showing past either end.
                let offset =
                    (full / 2.0 - (at + len / 2.0) * scale).clamp(full - full * scale, 0.0);
                move |v: f32| v * scale + offset
            };
            let x = axis(group.x, group.w, area.w, ZOOM_SHARE.0);
            let y = axis(group.y, group.h, area.h, ZOOM_SHARE.1);
            let view = |r: Rect| {
                let (x0, y0) = (x(r.x), y(r.y));
                Rect {
                    x: x0,
                    y: y0,
                    w: x(r.x + r.w) - x0,
                    h: y(r.y + r.h) - y0,
                }
            };
            group = view(group);
            for tile in &mut tiles {
                *tile = view(*tile);
            }
        }
        if self.zoom > 0.0 {
            let inner = treemap::layout(&member_initial, &member_weight, group);
            for (&i, rect) in self.grouped.iter().zip(inner) {
                tiles[i] = rect;
            }
        }
        (tiles, Some(group))
    }

    /// Grouped items not yet cleared away.
    fn group_members(&self) -> Vec<usize> {
        let visible = self.visible();
        self.grouped
            .iter()
            .copied()
            .filter(|i| visible.contains(i))
            .collect()
    }

    /// Start and end of a category's segment as fractions of the bar.
    fn span(&self, category: Category) -> (f32, f32) {
        let total = self.volume.total.max(1) as f64;
        let i = category as usize;
        let start: f64 = self.shown[..i].iter().sum();
        (
            (start / total) as f32,
            ((start + self.shown[i]) / total) as f32,
        )
    }

    fn bar(&self) -> Bar {
        let segments = Category::ALL
            .iter()
            .map(|&c| {
                let (start, end) = self.span(c);
                Segment {
                    tone: tone(c),
                    start,
                    end,
                    glow: self.glow[c as usize],
                }
            })
            .collect();
        let cleaning = self.progress.cleaning();
        let edge = match &self.items {
            Some(items) if cleaning != 0 => {
                Some(items[cleaning.trailing_zeros() as usize].category as usize)
            }
            _ => (0..9).rev().find(|&i| self.shown[i] > 0.0),
        };
        Bar {
            segments,
            edge,
            edge_strength: self.edge_strength,
            time: self.time,
        }
    }

    fn category_at(&self, x: Pixels) -> Option<Category> {
        let bounds = self.bar_bounds.get();
        let f = f32::from(x - bounds.origin.x) / f32::from(bounds.size.width).max(1.0);
        Category::ALL.into_iter().find(|&c| {
            let (start, end) = self.span(c);
            f >= start && f < end
        })
    }

    fn detail(&self, category: Category) -> Option<String> {
        let r = &self.report;
        let i = category as usize;
        let largest_file = || {
            r.largest_file[i].as_ref().map(|(name, bytes)| {
                format!("Your largest file is {name} at {}.", gb(*bytes as f64))
            })
        };
        match category {
            Category::Apps => r.largest_item[i].as_ref().map(|(name, bytes)| {
                let name = name.strip_suffix(".app").unwrap_or(name);
                format!("{name} is the largest at {}.", gb(*bytes as f64))
            }),
            Category::Developer => {
                let mut parts = Vec::new();
                if r.simulators >= 100_000_000 {
                    parts.push(format!("Simulators take {}.", gb(r.simulators as f64)));
                }
                if r.build_output >= 100_000_000 {
                    parts.push(format!("Build output takes {}.", gb(r.build_output as f64)));
                }
                if parts.is_empty() {
                    r.largest_item[i].as_ref().map(|(name, bytes)| {
                        format!("Most of it is {name} at {}.", gb(*bytes as f64))
                    })
                } else {
                    Some(parts.join(" "))
                }
            }
            Category::Photos | Category::Movies | Category::Documents | Category::Music => {
                largest_file()
            }
            Category::System => Some(if cfg!(target_os = "macos") {
                "macOS, app data and files outside your folders.".into()
            } else {
                "The system, app data and files outside your folders.".into()
            }),
            Category::Caches => Some("Apps rebuild these when they need them again.".into()),
            Category::Trash => Some("Things you've already deleted.".into()),
        }
    }

    fn status_line(&self) -> AnyElement {
        let muted = rgb(MUTED);
        let path = |label: &'static str| {
            h_flex()
                .h_full()
                .min_w_0()
                .items_center()
                .child(crate::pixel_text::status(
                    label,
                    tilde(&self.progress.current()),
                    self.time,
                    MUTED,
                ))
                .into_any_element()
        };
        match self.phase {
            Phase::Scanning => return path("Calculating"),
            Phase::Clearing => return path("Clearing"),
            Phase::Idle => {}
        }
        let available = gb(self.volume.total as f64 - self.used_shown());
        if let Some(c) = self.hover {
            return h_flex()
                .gap_2()
                .min_w_0()
                .child(duo(icon(c), tone(c), 14.0))
                .child(div().flex_none().text_color(rgb(FG)).child(c.name()))
                .child(
                    div()
                        .flex_none()
                        .text_color(muted)
                        .child(gb(self.shown[c as usize])),
                )
                .children(
                    self.detail(c)
                        .map(|d| div().min_w_0().truncate().text_color(muted).child(d)),
                )
                .into_any_element();
        }
        if self.freed > 0 {
            return h_flex()
                .gap_1p5()
                .child(Icon::new(IconName::Check).size_3p5().text_color(rgb(OK)))
                .child(
                    div()
                        .text_color(rgb(FG))
                        .font_weight(FontWeight::MEDIUM)
                        .child(format!("Freed {}.", gb(self.freed as f64))),
                )
                .child(
                    div()
                        .text_color(muted)
                        .child(format!("{available} available")),
                )
                .into_any_element();
        }
        div()
            .text_color(muted)
            .child(format!("{available} available"))
            .into_any_element()
    }

    /// The line under the cleanup headline: the hovered item, what is being
    /// cleared, or what the disk looks like afterwards.
    fn clean_status(&self, items: &[Item]) -> AnyElement {
        let muted = rgb(MUTED);
        let item_line = |i: usize, text: String| {
            let item = &items[i];
            h_flex()
                .gap_2()
                .min_w_0()
                .children(
                    item.icon
                        .clone()
                        .map(|icon| img(icon).size(z(14.0)).flex_none()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(rgb(FG))
                        .child(item.name.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(muted)
                        .child(gb(self.item_shown[i])),
                )
                .child(div().min_w_0().truncate().text_color(muted).child(text))
                .into_any_element()
        };
        let cleaning = self.progress.cleaning();
        if cleaning.count_ones() == 1 {
            return item_line(
                cleaning.trailing_zeros() as usize,
                tilde(&self.progress.current()),
            );
        }
        if cleaning != 0 {
            return h_flex()
                .gap_2()
                .min_w_0()
                .child(
                    div()
                        .flex_none()
                        .text_color(rgb(FG))
                        .child(format!("Clearing {} items", cleaning.count_ones())),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(muted)
                        .child(tilde(&self.progress.current())),
                )
                .into_any_element();
        }
        if let Some(i) = self.hover_item {
            return item_line(i, items[i].detail.clone());
        }
        if self.hover_group {
            let members = self.group_members();
            let names: Vec<&str> = members.iter().map(|&i| items[i].name.as_str()).collect();
            return h_flex()
                .gap_2()
                .min_w_0()
                .child(
                    div()
                        .flex_none()
                        .text_color(rgb(FG))
                        .child(format!("{} more", members.len())),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(muted)
                        .child(gb(members.iter().map(|&i| self.item_shown[i]).sum())),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(muted)
                        .child(names.join(", ")),
                )
                .into_any_element();
        }
        if !self.kept.is_empty() {
            return h_flex()
                .gap_1p5()
                .min_w_0()
                .child(
                    Icon::new(IconName::TriangleAlert)
                        .size_3p5()
                        .flex_none()
                        .text_color(rgb(WARN)),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(WARN))
                        .child(format!("Not cleared: {}", self.kept.join("; "))),
                )
                .into_any_element();
        }
        let free = self.volume.total as f64 - self.used_shown();
        div()
            .text_color(muted)
            .child(format!(
                "{} available after clearing",
                gb(free + self.selected_bytes())
            ))
            .into_any_element()
    }

    /// The treemap: tiles painted on one canvas, with an invisible div per
    /// tile on top for hover, clicks and the label.
    fn treemap(&self, items: &[Item], cx: &mut Context<Self>) -> impl IntoElement + use<> {
        const DUST: f32 = 60.0;
        let (tiles, group) = self.tiles();
        let cleaning = self.progress.cleaning();
        let weights = self.weights();
        let inset = |r: Rect| Rect {
            x: r.x + s(treemap::GAP) / 2.0,
            y: r.y + s(treemap::GAP) / 2.0,
            w: (r.w - s(treemap::GAP)).max(0.0),
            h: (r.h - s(treemap::GAP)).max(0.0),
        };
        let mut painted: Vec<(Rect, treemap::Tile)> = items
            .iter()
            .enumerate()
            .filter(|&(i, _)| {
                tiles[i].w >= s(treemap::GAP + 2.0) && tiles[i].h >= s(treemap::GAP + 2.0)
            })
            .map(|(i, item)| {
                let tile = treemap::Tile {
                    tone: item.tone,
                    on: self.selected[i],
                    glow: self.item_glow[i],
                    reveal: (self.item_shown[i] / weights[i].max(1.0)) as f32,
                    busy: busy(cleaning, i),
                    clear: 0.0,
                };
                (inset(tiles[i]), tile)
            })
            .collect();
        let members = self.group_members();
        // Zoomed in, the grouped items are tiles of their own instead.
        let group = group
            .map(inset)
            .filter(|r| self.zoom == 0.0 && !members.is_empty() && r.w >= 2.0 && r.h >= 2.0);
        if let Some(r) = group {
            let shown: f64 = members.iter().map(|&i| self.item_shown[i]).sum();
            let weight: f64 = members.iter().map(|&i| weights[i]).sum();
            painted.push((
                r,
                treemap::Tile {
                    tone: Tone::Grey,
                    on: members.iter().any(|&i| self.selected[i]),
                    glow: self.group_glow,
                    reveal: (shown / weight.max(1.0)) as f32,
                    busy: members.iter().any(|&i| busy(cleaning, i)),
                    clear: 0.0,
                },
            ));
        }

        let idle = self.phase == Phase::Idle;
        let all = self.items.clone();
        let mut labels = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let r = inset(tiles[i]);
            if r.w < 2.0 || r.h < 2.0 {
                continue;
            }
            let on = self.selected[i];
            let text = rgb(if on { FG } else { 0x77777c });
            let small = r.w < s(96.0) || r.h < s(56.0);
            let tiny = r.w < s(34.0) || r.h < s(30.0);
            let icon = item.icon.clone().map(|icon| {
                img(icon)
                    .size(z(if small { 14.0 } else { 17.0 }))
                    .flex_none()
                    .grayscale(!on)
                    .opacity(if on { 1.0 } else { 0.5 })
            });
            let label =
                v_flex()
                    .size_full()
                    .justify_between()
                    .p(z(if small { 6.0 } else { 10.0 }))
                    .overflow_hidden()
                    .when(!tiny, |label| {
                        label
                            .child(h_flex().gap(z(7.0)).min_w_0().children(icon).when(
                                !small,
                                |row| {
                                    row.child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(z(12.5))
                                            .text_color(text)
                                            .child(item.name.clone()),
                                    )
                                },
                            ))
                            .when(r.h >= s(44.0) && r.w >= s(58.0), |label| {
                                label.child(
                                    div()
                                        .whitespace_nowrap()
                                        .font_family("Menlo")
                                        .font_weight(FontWeight::BOLD)
                                        .text_size(z(if small { 11.5 } else { 14.0 }))
                                        .text_color(text)
                                        .child(gb(self.item_shown[i])),
                                )
                            })
                    });
            labels.push(
                div()
                    .id(("tile", i))
                    .absolute()
                    .left(px(r.x))
                    .top(px(r.y))
                    .w(px(r.w))
                    .h(px(r.h))
                    .when(idle, |tile| tile.cursor_pointer())
                    .when_some(all.clone().filter(|_| idle), |tile, all| {
                        let size = self.item_shown[i];
                        tile.tooltip(move |_, cx| tip::Tip::item(&all[i], size).view(cx))
                    })
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if this.phase != Phase::Idle || this.autoplay {
                            return;
                        }
                        if *hovered {
                            this.hover_item = Some(i);
                        } else if this.hover_item == Some(i) {
                            this.hover_item = None;
                        }
                        cx.notify();
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.phase != Phase::Idle {
                            return;
                        }
                        // Zoomed in, the tiles around the group zoom back out.
                        if this.zoomed && !this.grouped.contains(&i) {
                            this.zoomed = false;
                        } else {
                            this.selected[i] = !this.selected[i];
                        }
                        cx.notify();
                    }))
                    .child(label),
            );
        }

        if let Some(r) = group {
            let on = members.iter().any(|&i| self.selected[i]);
            let shown: f64 = members.iter().map(|&i| self.item_shown[i]).sum();
            let roomy = r.w >= s(44.0) && r.h >= s(44.0);
            labels.push(
                div()
                    .id("tile-more")
                    .absolute()
                    .left(px(r.x))
                    .top(px(r.y))
                    .w(px(r.w))
                    .h(px(r.h))
                    .p(z(6.0))
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .justify_between()
                    .text_color(rgb(if on { FG } else { 0x77777c }))
                    .when(idle, |tile| tile.cursor_pointer())
                    .when_some(all.clone().filter(|_| idle), |tile, all| {
                        let sizes: Vec<(usize, f64)> =
                            members.iter().map(|&i| (i, self.item_shown[i])).collect();
                        tile.tooltip(move |_, cx| tip::Tip::group(&all, sizes.clone()).view(cx))
                    })
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        if this.phase != Phase::Idle || this.autoplay {
                            return;
                        }
                        this.hover_group = *hovered;
                        cx.notify();
                    }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.phase == Phase::Idle {
                            this.hover_group = false;
                            this.zoomed = true;
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .whitespace_nowrap()
                            .text_size(z(12.0))
                            .child(format!("{} more", members.len())),
                    )
                    .when(roomy, |tile| {
                        tile.child(
                            div()
                                .whitespace_nowrap()
                                .font_family("Menlo")
                                .font_weight(FontWeight::BOLD)
                                .text_size(z(11.5))
                                .child(gb(shown)),
                        )
                    }),
            );
        }

        let clean_bounds = self.clean_bounds.clone();
        let particles = self.particles.clone();
        let time = self.time;
        div()
            .relative()
            .flex_1()
            .min_h(z(160.0))
            .w_full()
            .child(
                // Clips the tiles that zooming slides past the edge.
                div()
                    .absolute()
                    .size_full()
                    .overflow_hidden()
                    .child(
                        canvas(
                            move |bounds, _, _| clean_bounds.set(bounds),
                            move |bounds, _, window, _| {
                                for (r, tile) in &painted {
                                    let at = Bounds::new(
                                        bounds.origin + point(px(r.x), px(r.y)),
                                        size(px(r.w), px(r.h)),
                                    );
                                    treemap::paint(tile, at, bounds.origin, time, window);
                                }
                            },
                        )
                        .absolute()
                        .size_full(),
                    )
                    .children(labels),
            )
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        let origin = bounds.origin + point(px(0.0), z(DUST));
                        bar::paint_particles(&particles, origin, window);
                    },
                )
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .top(z(-DUST)),
            )
    }

    /// Every developer cache in one row, cleared with one button. These are
    /// downloads and build caches the tools fetch again on their own.
    fn dev_row(&self, items: &[Item], cx: &mut Context<Self>) -> Option<AnyElement> {
        let mut dev = self.dev_items();
        if dev.is_empty() {
            return None;
        }
        dev.sort_by(|&a, &b| self.item_found[b].total_cmp(&self.item_found[a]));
        let found: f64 = dev.iter().map(|&i| self.item_found[i]).sum();
        let shown: f64 = dev.iter().map(|&i| self.item_shown[i]).sum();
        let cleaning = self.progress.cleaning();
        let busy_dev = dev.iter().any(|&i| busy(cleaning, i));
        let left = dev.iter().any(|&i| items[i].left() > 0);
        let muted = rgb(MUTED);

        let names: Vec<&str> = dev
            .iter()
            .filter(|&&i| items[i].left() > 0)
            .map(|&i| items[i].name.as_str())
            .collect();
        let detail = if busy_dev {
            tilde(&self.progress.current())
        } else if names.is_empty() {
            "All cleared".into()
        } else if names.len() <= 3 {
            names.join(", ")
        } else {
            format!("{} and {} more", names[..3].join(", "), names.len() - 3)
        };

        let tone = tone(Category::Developer);
        let tile = treemap::Tile {
            tone,
            on: left,
            glow: self.dev_glow,
            reveal: (shown / found.max(1.0)) as f32,
            busy: busy_dev,
            clear: 0.62,
        };
        let time = self.time;
        // A few of the tools' icons, overlapping.
        let icons = h_flex().flex_none().children(
            dev.iter()
                .filter_map(|&i| items[i].icon.clone())
                .take(4)
                .enumerate()
                .map(|(k, icon)| {
                    div()
                        .when(k > 0, |d| d.ml(z(-5.0)))
                        .p(z(2.0))
                        .rounded_full()
                        .bg(rgb(BG))
                        .child(img(icon).size(z(18.0)))
                }),
        );
        let idle = self.phase == Phase::Idle;
        Some(
            div()
                .id("dev-caches")
                .relative()
                .flex_none()
                .h(z(64.0))
                .w_full()
                .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                    if this.phase == Phase::Idle && !this.autoplay {
                        this.dev_hover = *hovered;
                        cx.notify();
                    }
                }))
                .child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, window, _| {
                            treemap::paint(&tile, bounds, bounds.origin, time, window)
                        },
                    )
                    .absolute()
                    .size_full(),
                )
                .child(
                    h_flex()
                        .absolute()
                        .size_full()
                        .px_4()
                        .gap_3()
                        .child(icons)
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(
                                    h_flex()
                                        .items_baseline()
                                        .gap_2()
                                        .child(
                                            div()
                                                .font_weight(FontWeight::MEDIUM)
                                                .child("Developer caches"),
                                        )
                                        .child(
                                            div()
                                                .font_family("Menlo")
                                                .font_weight(FontWeight::BOLD)
                                                .child(gb(shown)),
                                        ),
                                )
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(z(12.0))
                                        .text_color(muted)
                                        .child(detail),
                                ),
                        )
                        .child(
                            Button::new("clear-dev")
                                .primary()
                                .small()
                                .label("Clear")
                                .disabled(!idle || !left)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.ask_clear(|this, i| this.is_dev(i), cx);
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The two headlines over the bottom list, each a tab: what caches can
    /// go, and what is in the user's own files.
    fn tabs(&self, cx: &mut Context<Self>) -> Div {
        let tab = |id: &'static str, view: View, figure: Div| {
            let active = self.view == view;
            div()
                .id(id)
                .flex_none()
                .opacity(if active { 1.0 } else { 0.4 })
                .when(!active, |tab| tab.cursor_pointer())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.view = view;
                    this.hover_item = None;
                    this.hover_group = false;
                    cx.notify();
                }))
                .child(figure)
        };
        h_flex()
            .gap_6()
            .child(tab(
                "tab-caches",
                View::Caches,
                figure(gb(self.selected_bytes()), 20.0, "can go", 13.0),
            ))
            .child(tab(
                "tab-files",
                View::Files,
                figure(self.files_figure(), 20.0, "in your files", 13.0),
            ))
    }

    fn cleanup(&self, cx: &mut Context<Self>) -> AnyElement {
        let row = h_flex().h(z(30.0)).gap_2().items_center();
        let Some(items) = self.items.clone() else {
            return row
                .child(Spinner::new().small().color(rgb(MUTED).into()))
                .child(
                    div()
                        .text_color(rgb(MUTED))
                        .child("Looking for things you can clear"),
                )
                .into_any_element();
        };
        let idle = self.phase == Phase::Idle;
        let tabs = self.tabs(cx);
        let heading = move |status: AnyElement, buttons: Option<AnyElement>| {
            h_flex()
                .items_start()
                .justify_between()
                .gap_4()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(tabs)
                        .child(div().h(z(18.0)).child(status)),
                )
                .children(buttons)
        };

        let body = if self.view == View::Files {
            v_flex()
                .flex_1()
                .min_h_0()
                .gap_3()
                .child(heading(self.files_status(), Some(self.files_buttons(cx))))
                .child(self.files_area(cx))
        } else if self.visible().is_empty() {
            v_flex()
                .gap_3()
                .child(heading(div().into_any_element(), None))
                .child(
                    row.child(Icon::new(IconName::Check).size_3p5().text_color(rgb(OK)))
                        .child(
                            div()
                                .text_color(rgb(MUTED))
                                .child("Nothing else is worth clearing right now."),
                        ),
                )
        } else {
            let selected = self.selected_bytes();
            let clear = Button::new("clear")
                .primary()
                .small()
                .label("Clear")
                .disabled(!idle || selected < 1e6)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.clear_selected(cx);
                    cx.notify();
                }));
            v_flex()
                .flex_1()
                .min_h_0()
                .gap_3()
                .child(heading(
                    self.clean_status(&items),
                    Some(clear.into_any_element()),
                ))
                .child(self.treemap(&items, cx))
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .gap_3()
            .children(self.dev_row(&items, cx))
            .child(body)
            .into_any_element()
    }
}

impl Render for Card {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        ui::set_scale(window.scale_factor());
        self.animating = self.tick(cx);
        if self.animating {
            window.request_animation_frame();
        }
        if self.onboarding.is_some() {
            return self.render_onboarding(cx);
        }
        let idle = self.phase == Phase::Idle;

        let header = h_flex()
            .items_start()
            .justify_between()
            .gap_4()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(figure(
                        gb(self.used_shown()),
                        26.0,
                        format!("of {} used", capacity(self.volume.total)),
                        14.0,
                    ))
                    .child(div().h(z(18.0)).child(self.status_line())),
            )
            .child(
                h_flex()
                    .gap_1()
                    .children(self.update_button(cx))
                    .children(self.docker.is_some().then(|| {
                        Button::new("docker")
                            .ghost()
                            .small()
                            .icon(IconName::Container)
                            .label("Docker")
                            .disabled(!idle)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_docker(cx);
                                cx.notify();
                            }))
                    }))
                    .child(
                        Button::new("apps")
                            .ghost()
                            .small()
                            .icon(IconName::LayoutGrid)
                            .label("Apps")
                            .disabled(!idle)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_apps(cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("rescan")
                            .ghost()
                            .small()
                            .icon(IconName::RefreshCw)
                            .label("Rescan")
                            .disabled(!idle)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.reload_panels = true;
                                this.scan(cx);
                                cx.notify();
                            })),
                    ),
            );

        let model = self.bar();
        let bar_bounds = self.bar_bounds.clone();
        let bar = div()
            .id("bar")
            .relative()
            .h(z(54.0))
            .w_full()
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                if this.phase != Phase::Idle
                    || this.autoplay
                    || !this.bar_bounds.get().contains(&e.position)
                {
                    return;
                }
                let hover = this.category_at(e.position.x);
                if hover != this.hover {
                    this.hover = hover;
                    cx.notify();
                }
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !hovered && !this.autoplay {
                    this.hover = None;
                    cx.notify();
                }
            }))
            .child(
                canvas(
                    move |bounds, _, _| bar_bounds.set(bounds),
                    move |bounds, _, window, _| bar::paint(&model, bounds, window),
                )
                .absolute()
                .size_full(),
            );

        let legend_bounds = self.legend_bounds.clone();
        let [pill_x, pill_y, pill_w] = self.pill;
        let pill = div()
            .absolute()
            .left(px(pill_x))
            .top(px(pill_y))
            .w(px(pill_w))
            .h(z(ROW_H))
            .rounded(z(6.0))
            .bg(rgb(0x232327))
            .opacity(self.pill_alpha);
        let rows = Category::ALL.chunks(3).map(|row| {
            h_flex().gap(z(COL_GAP)).children(row.iter().map(|&c| {
                h_flex()
                    .id(c.name())
                    .flex_1()
                    .h(z(ROW_H))
                    .px_1p5()
                    .gap_2()
                    .rounded(z(6.0))
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if this.phase != Phase::Idle || this.autoplay {
                            return;
                        }
                        if *hovered {
                            this.hover = Some(c);
                        } else if this.hover == Some(c) {
                            this.hover = None;
                        }
                        cx.notify();
                    }))
                    .child(duo(icon(c), tone(c), 14.0))
                    .child(div().flex_1().text_color(rgb(FG)).child(c.name()))
                    .child(
                        div()
                            .text_size(z(12.0))
                            .text_color(rgb(MUTED))
                            .child(gb(self.shown[c as usize])),
                    )
            }))
        });
        let legend = v_flex()
            .relative()
            .gap(z(ROW_GAP))
            .child(pill)
            .child(
                canvas(
                    move |bounds, _, _| legend_bounds.set(bounds),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .children(rows);

        let confirm = self.render_confirm(cx);
        let apps = self.render_apps(cx);
        let docker = self.render_docker(cx);
        if window.focused(cx).is_none() {
            self.focus.focus(window, cx);
        }
        v_flex()
            .track_focus(&self.focus)
            .key_context("Card")
            .on_action(cx.listener(|this, _: &crate::Dismiss, _, cx| {
                if this.apps.shown {
                    this.dismiss_apps(cx);
                } else if this.docker.as_ref().is_some_and(|d| d.shown) {
                    this.dismiss_docker(cx);
                } else if this.files_dismiss() {
                } else {
                    this.confirm = None;
                }
                cx.notify();
            }))
            .relative()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(FG))
            .text_size(z(13.0))
            .px_5()
            .pt_5()
            .pb_3()
            .gap_4()
            .child(header)
            .child(bar)
            .child(legend)
            .child(div().flex_none().h(px(1.0)).bg(rgb(LINE)))
            .child(self.cleanup(cx))
            .children(confirm)
            .children(apps)
            .children(docker)
            .into_any_element()
    }
}

/// Whether item `i` is set in a [`Progress::cleaning`] mask.
fn busy(mask: u64, i: usize) -> bool {
    1u64.checked_shl(i as u32)
        .is_some_and(|bit| mask & bit != 0)
}
