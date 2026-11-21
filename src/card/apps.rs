//! Removing apps with their files, from a panel over the card.

use std::cell::RefCell;
use std::f32::consts::FRAC_PI_2;
use std::rc::Rc;

use dither_gpui::dither_core::ease::approach;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Disableable, Sizable, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::scroll::{Scroll, measure};
use super::{BG, Card, FG, LINE, MUTED, Phase, duo, gb, icon, tilde, tone};
use crate::disk::{self, Category};
use crate::ui::z;
use crate::uninstall::{self, App};

/// Read once in the background and kept while the panel is closed, so
/// opening it again is instant. The refresh button reads them again.
#[derive(Default)]
pub(super) struct Apps {
    /// Whether the panel is open.
    pub(super) shown: bool,
    /// None until the apps are first read.
    list: Option<Vec<App>>,
    /// Whether the apps are being read.
    loading: bool,
    /// The app showing its files.
    open: Option<usize>,
    removing: bool,
    /// Why the open app wasn't removed.
    note: Option<String>,
    /// Whether anything was removed, so closing rescans.
    changed: bool,
    /// How far each app's files are shown, 0 to 1, and their full height.
    reveal: Vec<f32>,
    heights: Rc<RefCell<Vec<f32>>>,
    scroll: Scroll,
}

impl Apps {
    /// Eases the open and close and the scroll. Returns whether to keep going.
    pub(super) fn tick(&mut self, dt: f32) -> bool {
        let mut moving = false;
        for (i, reveal) in self.reveal.iter_mut().enumerate() {
            let to = if self.open == Some(i) { 1.0 } else { 0.0 };
            *reveal = approach(*reveal, to, dt, 0.05);
            if (*reveal - to).abs() < 0.002 {
                *reveal = to;
            } else {
                moving = true;
            }
        }
        moving |= self.scroll.tick(dt);
        // Heights are measured in a frame's layout, so look again next frame.
        moving || self.list.is_none()
    }
}

const REMOVE: &str = if cfg!(target_os = "macos") {
    "Move to Trash"
} else {
    "Uninstall"
};

impl Card {
    pub(super) fn show_apps(&mut self, cx: &mut Context<Self>) {
        if self.apps.shown || self.phase != Phase::Idle {
            return;
        }
        self.apps.shown = true;
        if self.apps.list.is_none() {
            self.load_apps(cx);
        }
    }

    /// Reads the apps in the background, keeping the old list on screen
    /// until the new one is in. Removing waits for it, so the list can't
    /// change under a removal.
    pub(super) fn load_apps(&mut self, cx: &mut Context<Self>) {
        if self.apps.loading || self.apps.removing {
            return;
        }
        self.apps.loading = true;
        let demo = self.demo;
        cx.spawn(async move |this, cx| {
            let list = cx
                .background_executor()
                .spawn(async move { uninstall::list(&disk::home(), demo) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let apps = &mut this.apps;
                apps.loading = false;
                apps.open = None;
                apps.note = None;
                apps.reveal = vec![0.0; list.len()];
                *apps.heights.borrow_mut() = vec![0.0; list.len()];
                apps.list = Some(list);
                cx.notify();
            });
        })
        .detach();
    }

    /// Escape: closes the panel, unless an app is being removed.
    pub(super) fn dismiss_apps(&mut self, cx: &mut Context<Self>) {
        if !self.apps.removing {
            self.close_apps(cx);
        }
    }

    /// Hides the panel and keeps the list for next time.
    fn close_apps(&mut self, cx: &mut Context<Self>) {
        let apps = &mut self.apps;
        apps.shown = false;
        apps.open = None;
        apps.note = None;
        apps.reveal.fill(0.0);
        apps.scroll = Default::default();
        if std::mem::take(&mut apps.changed) {
            self.scan(cx);
        }
    }

    fn remove_app(&mut self, i: usize, cx: &mut Context<Self>) {
        let apps = &mut self.apps;
        if apps.loading {
            return;
        }
        let Some(app) = apps.list.as_mut().and_then(|list| list.get(i)) else {
            return;
        };
        if app.is_open() {
            apps.note = Some(format!("Quit {} first.", app.name));
            return;
        }
        apps.removing = true;
        apps.note = None;
        let app = app.clone();
        cx.spawn(async move |this, cx| {
            let name = app.name.clone();
            let ok = cx
                .background_executor()
                .spawn(async move { app.remove() })
                .await;
            let _ = this.update(cx, |this, cx| {
                let apps = &mut this.apps;
                apps.removing = false;
                apps.changed = true;
                if ok {
                    apps.open = None;
                    if let Some(list) = &mut apps.list {
                        list.remove(i);
                        apps.reveal.remove(i);
                        apps.heights.borrow_mut().remove(i);
                    }
                } else {
                    apps.note = Some(format!("Couldn't remove all of {name}."));
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_apps(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let apps = &self.apps;
        if !apps.shown {
            return None;
        }
        let muted = rgb(MUTED);

        let body = match &apps.list {
            None => h_flex()
                .py_4()
                .gap_2()
                .child(Spinner::new().small().color(muted.into()))
                .child(div().text_color(muted).child("Reading your apps"))
                .into_any_element(),
            Some(list) if list.is_empty() => div()
                .py_4()
                .text_color(muted)
                .child("No apps you can remove.")
                .into_any_element(),
            Some(list) => {
                let rows: Vec<_> = list
                    .iter()
                    .enumerate()
                    .map(|(i, app)| self.app_row(i, app, apps, cx))
                    .collect();
                apps.scroll
                    .area("apps-list", cx, |this| Some(&mut this.apps.scroll), rows)
                    .border_t_1()
                    .border_color(rgb(LINE))
                    .into_any_element()
            }
        };

        let panel = v_flex()
            .w_full()
            .max_w(z(640.0))
            .max_h_full()
            .p_5()
            .gap_3()
            .rounded(z(12.0))
            .border_1()
            .border_color(rgb(LINE))
            .bg(rgb(0x1c1c1f))
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .text_size(z(20.0))
                            .font_weight(FontWeight::MEDIUM)
                            .child("Apps"),
                    )
                    // The first read shows its own spinner below.
                    .children(apps.list.is_some().then(|| {
                        Button::new("apps-refresh")
                            .ghost()
                            .small()
                            .icon(IconName::RefreshCw)
                            .tooltip("Refresh")
                            .loading(apps.loading)
                            .disabled(apps.removing)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.load_apps(cx);
                                cx.notify();
                            }))
                    }))
                    .child(
                        Button::new("apps-close")
                            .ghost()
                            .small()
                            .icon(IconName::Close)
                            .disabled(apps.removing)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.close_apps(cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(body);

        Some(
            div()
                .id("apps")
                .absolute()
                .inset_0()
                .p_6()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba((BG << 8) | 0xd8))
                .occlude()
                .child(panel)
                .into_any_element(),
        )
    }

    fn app_row(&self, i: usize, app: &App, apps: &Apps, cx: &mut Context<Self>) -> AnyElement {
        let muted = rgb(MUTED);
        let reveal = apps.reveal.get(i).copied().unwrap_or(0.0);
        let icon = match &app.icon {
            Some(image) => img(image.clone()).size(z(20.0)).into_any_element(),
            None => duo(icon(Category::Apps), tone(Category::Apps), 16.0).into_any_element(),
        };
        let head = h_flex()
            .id(("app", i))
            .gap_2()
            .py_2()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                let apps = &mut this.apps;
                if !apps.removing {
                    apps.open = if apps.open == Some(i) { None } else { Some(i) };
                    apps.note = None;
                }
                cx.notify();
            }))
            .child(
                div()
                    .size(z(20.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon),
            )
            .child(
                div()
                    .flex_1()
                    .font_weight(FontWeight::MEDIUM)
                    .child(app.name.clone()),
            )
            .child(div().text_color(muted).child(gb(app.bytes() as f64)))
            .child(
                svg()
                    .path(IconName::ChevronRight.path())
                    .size_3p5()
                    .text_color(muted)
                    .with_transformation(Transformation::rotate(radians(reveal * FRAC_PI_2))),
            );

        let mut row = v_flex().border_b_1().border_color(rgb(LINE)).child(head);
        if reveal > 0.0 {
            let files = app.files.iter().map(|(path, bytes)| {
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_family("Menlo")
                            .text_size(z(12.0))
                            .text_color(rgb(FG))
                            .child(tilde(&path.to_string_lossy())),
                    )
                    .child(
                        div()
                            .text_size(z(12.0))
                            .text_color(muted)
                            .child(gb(*bytes as f64)),
                    )
            });
            let removing = apps.removing;
            let height = apps.heights.borrow().get(i).copied().unwrap_or(0.0);
            let heights = apps.heights.clone();
            let details = v_flex()
                .flex_none()
                .relative()
                .opacity(reveal)
                .child(measure(move |h| {
                    if let Some(slot) = heights.borrow_mut().get_mut(i) {
                        *slot = h;
                    }
                }))
                .gap_2()
                .pl(z(28.0))
                .pb_3()
                .children(files)
                .children(cfg!(not(target_os = "macos")).then(|| {
                    div()
                        .text_color(muted)
                        .child("Deleted for good, not moved to the Trash.")
                }))
                .child(
                    h_flex()
                        .pt_1()
                        .gap_3()
                        .justify_end()
                        .children(
                            apps.note
                                .clone()
                                .map(|note| div().flex_1().text_color(muted).child(note)),
                        )
                        .children(removing.then(|| Spinner::new().small().color(muted.into())))
                        .child(
                            Button::new("app-remove")
                                .primary()
                                .small()
                                .label(REMOVE)
                                .disabled(removing || apps.loading)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.remove_app(i, cx);
                                    cx.notify();
                                })),
                        ),
                );
            row = row.child(
                v_flex()
                    .h(px(height * reveal))
                    .overflow_hidden()
                    .child(details),
            );
        }
        row.into_any_element()
    }
}
