//! Docker's logs, caches, old builds, images, stopped containers and
//! volumes, in a panel over the card like the Apps one.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Disableable, Sizable, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::scroll::Scroll;
use super::tip::Tip;
use super::{BG, Card, LINE, MUTED, Phase, WARN};
use crate::docker::{self, Chosen, Docker, State};
use crate::icons::{self, Logo, Source};
use crate::ui::z;

/// Docker is read once in the background and kept while the panel is
/// closed, so opening it again is instant. The refresh button reads it again.
pub(super) struct DockerPanel {
    /// Whether the panel is open.
    pub(super) shown: bool,
    bin: PathBuf,
    demo: bool,
    icon: Option<Arc<Image>>,
    /// None until Docker is first read.
    state: Option<State>,
    /// Whether Docker is being read, after a clear or on its own. Clearing
    /// and picking wait for it, so picks always match what is shown.
    loading: bool,
    /// Picked log containers by ID, old builds by image name, and the
    /// other parts.
    logs: HashSet<String>,
    builds: HashSet<String>,
    cache: bool,
    unused: bool,
    stopped: bool,
    volumes: bool,
    /// What is running: clearing, restarting or starting Docker.
    busy: Option<&'static str>,
    note: Option<String>,
    /// Asking before "Keep logs small" restarts Docker.
    asking: bool,
    /// Whether anything was cleared, so closing rescans.
    changed: bool,
    scroll: Scroll,
}

impl DockerPanel {
    pub(super) fn new(bin: PathBuf, demo: bool) -> Self {
        Self {
            shown: false,
            bin,
            demo,
            icon: None,
            state: None,
            loading: false,
            logs: HashSet::new(),
            builds: HashSet::new(),
            cache: false,
            unused: false,
            stopped: false,
            volumes: false,
            busy: None,
            note: None,
            asking: false,
            changed: false,
            scroll: Scroll::default(),
        }
    }

    pub(super) fn tick(&mut self, dt: f32) -> bool {
        self.scroll.tick(dt)
    }

    fn ready(&self) -> Option<&Docker> {
        match &self.state {
            Some(State::Ready(docker)) => Some(docker),
            _ => None,
        }
    }

    /// Picks what is safe: every log, old builds, the build cache and
    /// unused images. Stopped containers and volumes wait for a click.
    fn set(&mut self, state: State) {
        if let State::Ready(docker) = &state {
            self.logs = docker.logs.iter().map(|l| l.id.clone()).collect();
            self.builds = docker.builds.iter().map(|b| b.name.clone()).collect();
            self.cache = docker.cache.bytes > 0;
            self.unused = docker.unused.bytes > 0;
            self.stopped = false;
            self.volumes = false;
        }
        self.state = Some(state);
    }

    /// The old builds, unused images and stopped containers picked.
    fn picked<'a>(&'a self, docker: &'a Docker) -> impl Iterator<Item = &'a docker::Freed> {
        let builds = docker
            .builds
            .iter()
            .filter(|b| self.builds.contains(&b.name))
            .map(|b| &b.old);
        builds
            .chain(self.unused.then_some(&docker.unused))
            .chain(self.stopped.then_some(&docker.stopped))
    }

    fn chosen(&self) -> Chosen {
        let mut chosen = Chosen {
            logs: self.logs.iter().cloned().collect(),
            cache: self.cache,
            volumes: self.volumes,
            ..Chosen::default()
        };
        if let Some(docker) = self.ready() {
            for freed in self.picked(docker) {
                chosen.containers.extend(freed.containers.iter().cloned());
                chosen.images.extend(freed.images.iter().cloned());
            }
        }
        chosen
    }

    fn chosen_bytes(&self) -> u64 {
        let Some(docker) = self.ready() else {
            return 0;
        };
        let logs: u64 = docker
            .logs
            .iter()
            .filter(|l| self.logs.contains(&l.id))
            .map(|l| l.bytes)
            .sum();
        let freed: u64 = self.picked(docker).map(|f| f.bytes).sum();
        let part = |on: bool, part: docker::Part| if on { part.bytes } else { 0 };
        logs + freed + part(self.cache, docker.cache) + part(self.volumes, docker.volumes)
    }
}

impl Card {
    pub(super) fn show_docker(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.docker else {
            return;
        };
        if panel.shown || self.phase != Phase::Idle {
            return;
        }
        panel.shown = true;
        if panel.state.is_none() {
            self.reread_docker(cx);
        }
    }

    /// Escape: backs out of the restart question, else closes the panel.
    /// Waits while Docker is busy, like the close button.
    pub(super) fn dismiss_docker(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.docker else {
            return;
        };
        if panel.busy.is_some() {
            return;
        }
        if panel.asking {
            panel.asking = false;
        } else {
            self.close_docker(cx);
        }
    }

    /// Hides the panel and keeps what was read for next time.
    fn close_docker(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.docker else {
            return;
        };
        panel.shown = false;
        panel.asking = false;
        panel.note = None;
        panel.scroll = Scroll::default();
        if std::mem::take(&mut panel.changed) {
            self.scan(cx);
        }
    }

    /// Reads Docker again in the background, after `then` runs there.
    /// One job at a time: the buttons that start one wait for the last.
    fn docker_job(
        &mut self,
        busy: Option<&'static str>,
        then: impl FnOnce(&std::path::Path) -> Option<String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = &mut self.docker else {
            return;
        };
        if panel.loading {
            return;
        }
        panel.loading = true;
        panel.busy = busy;
        panel.note = None;
        let bin = panel.bin.clone();
        let demo = panel.demo;
        cx.spawn(async move |this, cx| {
            let (note, state, icon) = cx
                .background_executor()
                .spawn(async move {
                    let note = if demo { None } else { then(&bin) };
                    let state = if demo {
                        std::thread::sleep(Duration::from_millis(400));
                        State::Ready(docker::demo())
                    } else {
                        docker::read(&bin)
                    };
                    let icon =
                        icons::load(Source::App("com.docker.docker", Some(Logo::Docker)), None);
                    (note, state, icon)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(panel) = &mut this.docker {
                    panel.loading = false;
                    panel.busy = None;
                    panel.note = note;
                    panel.icon = icon;
                    panel.set(state);
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn reread_docker(&mut self, cx: &mut Context<Self>) {
        self.docker_job(None, |_| None, cx);
    }

    fn clear_docker(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.docker else {
            return;
        };
        let chosen = panel.chosen();
        let bytes = panel.chosen_bytes();
        panel.changed = true;
        self.docker_job(
            Some("Clearing"),
            move |bin| {
                Some(if docker::clear(bin, &chosen) {
                    format!("Cleared {}.", size(bytes))
                } else {
                    "Some of it couldn't be cleared.".into()
                })
            },
            cx,
        );
    }

    fn keep_logs_small(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.docker else {
            return;
        };
        panel.asking = false;
        self.docker_job(
            Some("Restarting Docker"),
            |bin| {
                Some(if docker::keep_logs_small(bin) {
                    "Logs are now capped at 30 MB per new container.".into()
                } else {
                    "Couldn't set the log limit.".into()
                })
            },
            cx,
        );
    }

    fn start_docker(&mut self, cx: &mut Context<Self>) {
        self.docker_job(
            Some("Starting Docker"),
            |bin| (!docker::start(bin)).then(|| "Docker didn't start.".into()),
            cx,
        );
    }

    pub(super) fn render_docker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let panel = self.docker.as_ref().filter(|p| p.shown)?;
        let muted = rgb(MUTED);
        let busy = panel.busy.is_some();
        let loading = panel.loading;

        let status = match &panel.state {
            None => "",
            Some(State::Stopped) => "Stopped",
            Some(State::Ready(_)) => "Running",
        };
        let total = panel.chosen_bytes();
        let header = h_flex()
            .gap_2()
            .children(
                panel
                    .icon
                    .clone()
                    .map(|icon| img(icon).size(z(22.0)).flex_none()),
            )
            .child(
                div()
                    .text_size(z(20.0))
                    .font_weight(FontWeight::MEDIUM)
                    .child("Docker"),
            )
            .child(div().flex_1().text_color(muted).child(status))
            .children(panel.ready().map(|_| {
                Button::new("docker-clear")
                    .primary()
                    .small()
                    .label(if total > 0 {
                        format!("Clear {}", size(total))
                    } else {
                        "Clear".into()
                    })
                    .disabled(loading || total == 0)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.clear_docker(cx);
                        cx.notify();
                    }))
            }))
            // The first read shows its own spinner below.
            .children(panel.state.is_some().then(|| {
                Button::new("docker-refresh")
                    .ghost()
                    .small()
                    .icon(IconName::RefreshCw)
                    .tooltip("Refresh")
                    .loading(loading && !busy)
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reread_docker(cx);
                        cx.notify();
                    }))
            }))
            .child(
                Button::new("docker-close")
                    .ghost()
                    .small()
                    .icon(IconName::Close)
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.close_docker(cx);
                        cx.notify();
                    })),
            );

        let body = match &panel.state {
            None => h_flex()
                .py_4()
                .gap_2()
                .child(Spinner::new().small().color(muted.into()))
                .child(div().text_color(muted).child("Reading Docker"))
                .into_any_element(),
            Some(State::Stopped) => h_flex()
                .py_4()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .text_color(muted)
                        .child(if cfg!(target_os = "macos") {
                            "Docker isn't running. Start it to see what can go."
                        } else {
                            "Docker isn't running. Start its service to see what can go."
                        }),
                )
                .children(cfg!(target_os = "macos").then(|| {
                    Button::new("docker-start")
                        .small()
                        .label("Start Docker")
                        .disabled(loading)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.start_docker(cx);
                            cx.notify();
                        }))
                }))
                .into_any_element(),
            Some(State::Ready(docker)) => {
                let rows = self.docker_rows(panel, docker, cx);
                if rows.is_empty() {
                    div()
                        .py_4()
                        .text_color(muted)
                        .child("Nothing to clear.")
                        .into_any_element()
                } else {
                    panel
                        .scroll
                        .area(
                            "docker-list",
                            cx,
                            |this| this.docker.as_mut().map(|d| &mut d.scroll),
                            rows,
                        )
                        .into_any_element()
                }
            }
        };

        let footer = (busy || panel.note.is_some()).then(|| {
            h_flex()
                .gap_2()
                .children(busy.then(|| Spinner::new().small().color(muted.into())))
                .child(
                    div().text_color(muted).child(
                        panel
                            .busy
                            .map(str::to_owned)
                            .or(panel.note.clone())
                            .unwrap_or_default(),
                    ),
                )
        });

        let card = v_flex()
            .w_full()
            .max_w(z(640.0))
            .max_h_full()
            .p_5()
            .gap_3()
            .rounded(z(12.0))
            .border_1()
            .border_color(rgb(LINE))
            .bg(rgb(0x1c1c1f))
            .child(header)
            .child(body)
            .children(footer);

        Some(
            div()
                .id("docker")
                .absolute()
                .inset_0()
                .p_6()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba((BG << 8) | 0xd8))
                .occlude()
                .child(card)
                .children(panel.asking.then(|| self.ask_keep_logs_small(cx)))
                .into_any_element(),
        )
    }

    fn docker_rows(
        &self,
        panel: &DockerPanel,
        docker: &Docker,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let muted = rgb(MUTED);
        let busy = panel.loading;
        let mut rows = Vec::new();

        if !docker.logs.is_empty() {
            let total: u64 = docker.logs.iter().map(|l| l.bytes).sum();
            rows.push(heading(format!("Logs · {}", size(total)), muted));
            let biggest = docker.logs[0].bytes.max(1) as f32;
            for (i, log) in docker.logs.iter().enumerate() {
                let id = log.id.clone();
                let state = if log.running { "running" } else { "stopped" };
                rows.push(
                    row(
                        ("docker-log", i),
                        panel.logs.contains(&log.id),
                        busy,
                        log.name.clone(),
                        format!("{state}, created {}", docker::lower_first(&log.created)),
                        Some(bar(log.bytes as f32 / biggest)),
                        log.bytes,
                        cx.listener(move |this, _, _, cx| {
                            if let Some(panel) = &mut this.docker
                                && !panel.logs.remove(&id)
                            {
                                panel.logs.insert(id.clone());
                            }
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                );
            }
            // Logs grow back unless Docker limits them.
            let warning = if !docker.capped && docker.can_cap {
                Some("No size limit set, logs grow back.")
            } else if docker.capped && docker.logs.iter().any(|l| !l.capped) {
                Some("Containers made before the limit still grow. Recreate them to cap them.")
            } else {
                None
            };
            if let Some(warning) = warning {
                rows.push(
                    h_flex()
                        .pl(z(25.0))
                        .pt_3()
                        .pb_3()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .text_size(z(12.0))
                                .text_color(rgb(WARN))
                                .child(warning),
                        )
                        .children((!docker.capped && docker.can_cap).then(|| {
                            Button::new("docker-cap")
                                .small()
                                .label("Keep logs small")
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(panel) = &mut this.docker {
                                        panel.asking = true;
                                    }
                                    cx.notify();
                                }))
                        }))
                        .into_any_element(),
                );
            }
        }

        if !docker.builds.is_empty() {
            let total: u64 = docker.builds.iter().map(|b| b.old.bytes).sum();
            rows.push(heading(format!("Old builds · {}", size(total)), muted));
            let biggest = docker.builds[0].old.bytes.max(1) as f32;
            for (i, builds) in docker.builds.iter().enumerate() {
                let name = builds.name.clone();
                let tip = Tip::builds(builds);
                rows.push(
                    row(
                        ("docker-build", i),
                        panel.builds.contains(&builds.name),
                        busy,
                        builds.name.clone(),
                        format!("{}, newest kept", count(builds.count, "build")),
                        Some(bar(builds.old.bytes as f32 / biggest)),
                        builds.old.bytes,
                        cx.listener(move |this, _, _, cx| {
                            if let Some(panel) = &mut this.docker
                                && !panel.builds.remove(&name)
                            {
                                panel.builds.insert(name.clone());
                            }
                            cx.notify();
                        }),
                    )
                    .tooltip(move |_, cx| tip.clone().view(cx))
                    .into_any_element(),
                );
            }
        }

        let stopped = &docker.stopped;
        let parts = [
            (
                docker.cache.bytes,
                panel.cache,
                "Build cache",
                "Rebuilt when needed".to_owned(),
            ),
            (
                docker.unused.bytes,
                panel.unused,
                "Unused images",
                format!(
                    "{}, downloaded again when needed",
                    count(docker.unused.images.len(), "image")
                ),
            ),
            (
                stopped.bytes,
                panel.stopped,
                "Stopped containers",
                if stopped.images.is_empty() {
                    format!(
                        "{}, removed with their logs",
                        count(stopped.containers.len(), "container")
                    )
                } else {
                    format!(
                        "{}, with their images",
                        count(stopped.containers.len(), "container")
                    )
                },
            ),
        ];
        let shown: Vec<_> = parts.into_iter().filter(|p| p.0 > 0).collect();
        if !shown.is_empty() {
            let total: u64 = shown.iter().map(|p| p.0).sum();
            rows.push(heading(
                format!("Rebuilt when needed · {}", size(total)),
                muted,
            ));
            for (i, (bytes, on, name, detail)) in shown.into_iter().enumerate() {
                rows.push(
                    row(
                        ("docker-part", i),
                        on,
                        busy,
                        name.into(),
                        detail,
                        None,
                        bytes,
                        cx.listener(move |this, _, _, cx| {
                            if let Some(panel) = &mut this.docker {
                                let flag = match name {
                                    "Build cache" => &mut panel.cache,
                                    "Unused images" => &mut panel.unused,
                                    _ => &mut panel.stopped,
                                };
                                *flag = !*flag;
                            }
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                );
            }
        }

        if docker.volumes.bytes > 0 {
            rows.push(heading(
                format!("Your data · {}", size(docker.volumes.bytes)),
                rgb(WARN),
            ));
            let pill = div()
                .flex_none()
                .px_2()
                .rounded_full()
                .border_1()
                .border_color(rgb(0x4a3a1a))
                .text_size(z(12.0))
                .text_color(rgb(WARN))
                .child("Gone for good");
            rows.push(
                row(
                    ("docker-volumes", 0),
                    panel.volumes,
                    busy,
                    "Unused volumes".into(),
                    format!(
                        "{}, like database files",
                        count(docker.volumes.count, "volume")
                    ),
                    Some(pill.into_any_element()),
                    docker.volumes.bytes,
                    cx.listener(|this, _, _, cx| {
                        if let Some(panel) = &mut this.docker {
                            panel.volumes = !panel.volumes;
                        }
                        cx.notify();
                    }),
                )
                .into_any_element(),
            );
        }
        rows
    }

    fn ask_keep_logs_small(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = rgb(MUTED);
        let (file, restart) = if cfg!(target_os = "macos") {
            (
                "~/.docker/daemon.json",
                "Running containers stop for a few seconds",
            )
        } else {
            (
                "/etc/docker/daemon.json",
                "Asks for your password, running containers stop for a few seconds",
            )
        };
        let line = |what: &'static str, how: &'static str, color: Rgba| {
            h_flex()
                .gap_4()
                .justify_between()
                .child(div().flex_none().child(what))
                .child(div().text_color(color).child(how))
        };
        let dialog =
            v_flex()
                .w_full()
                .max_w(z(520.0))
                .p_5()
                .gap_3()
                .rounded(z(12.0))
                .border_1()
                .border_color(rgb(LINE))
                .bg(rgb(0x1c1c1f))
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_size(z(20.0))
                                .font_weight(FontWeight::MEDIUM)
                                .child("Keep Docker logs small?"),
                        )
                        .child(div().text_color(muted).child(
                            "Each container keeps at most 30 MB of logs: 3 files of 10 MB.",
                        )),
                )
                .child(
                    v_flex()
                        .gap_2()
                        .pt_3()
                        .border_t_1()
                        .border_color(rgb(LINE))
                        .child(line(file, "Adds 2 settings", muted))
                        .child(line("Docker restarts", restart, muted))
                        .child(line(
                            "Existing containers",
                            "Capped once recreated",
                            rgb(WARN),
                        )),
                )
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("docker-cap-cancel")
                                .ghost()
                                .small()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(panel) = &mut this.docker {
                                        panel.asking = false;
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("docker-cap-ok")
                                .primary()
                                .small()
                                .label("Restart Docker")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.keep_logs_small(cx);
                                    cx.notify();
                                })),
                        ),
                );
        div()
            .id("docker-ask")
            .absolute()
            .inset_0()
            .p_6()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba((BG << 8) | 0xd8))
            .occlude()
            .child(dialog)
            .into_any_element()
    }
}

fn heading(text: String, color: Rgba) -> AnyElement {
    div()
        .pt_4()
        .pb_1()
        .border_b_1()
        .border_color(rgb(LINE))
        .text_size(z(12.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .child(text)
        .into_any_element()
}

/// A row with a checkbox: name and detail, an optional bar or badge, and
/// the size.
#[allow(clippy::too_many_arguments)]
fn row(
    id: (&'static str, usize),
    on: bool,
    busy: bool,
    name: String,
    detail: String,
    extra: Option<AnyElement>,
    bytes: u64,
    toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let check = super::confirm::check(on, false);
    h_flex()
        .id(id)
        .gap_2p5()
        .py_2()
        .border_b_1()
        .border_color(rgb(LINE))
        .when(!busy, |r| r.cursor_pointer().on_click(toggle))
        .child(check)
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .child(div().flex_none().child(name))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(MUTED))
                        .child(detail),
                ),
        )
        .children(extra)
        .child(
            div()
                .w(z(72.0))
                .flex_none()
                .text_right()
                .font_family("Menlo")
                .font_weight(FontWeight::BOLD)
                .child(size(bytes)),
        )
}

/// A share of the largest row, red when it is most of it.
fn bar(share: f32) -> AnyElement {
    div()
        .w(z(96.0))
        .h(z(5.0))
        .flex_none()
        .rounded_full()
        .bg(rgb(LINE))
        .child(
            div()
                .h_full()
                .w(z(96.0 * share.max(0.02)))
                .rounded_full()
                .bg(rgb(if share > 0.5 { 0xef7b5c } else { MUTED })),
        )
        .into_any_element()
}

/// Sizes down to kilobytes: logs and stopped containers are often small.
pub(super) fn size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} MB", b / 1e6)
    } else {
        format!("{:.0} KB", (b / 1e3).max(1.0))
    }
}

/// "1 image", "3 images".
pub(super) fn count(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}
