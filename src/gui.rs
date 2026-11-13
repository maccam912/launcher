use crate::{
    catalog::{Catalog, Entry, Platform, pick_asset},
    github::{Client, Release},
    install::{Installed, Progress, State, Store},
    load_catalog, selfupdate,
};
use eframe::egui;
use std::{
    collections::BTreeMap,
    process::Child,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

const RECHECK_EVERY: Duration = Duration::from_secs(30 * 60);

enum Event {
    Catalog(Catalog),
    Release(String, Result<Option<Release>, String>),
    Progress(String, Progress),
    Installed(String, Result<Installed, String>),
    SelfUpdate(Result<Option<String>, String>),
}

enum Action {
    Install(String),
    Play(String),
    Uninstall(String),
}

#[derive(Default)]
struct Game {
    /// `None` until the first check finishes; `Some(None)` = no releases.
    release: Option<Option<Release>>,
    error: Option<String>,
    job: Option<Progress>,
    child: Option<Child>,
}

enum SelfUpdate {
    Idle,
    /// The new version is on disk and starts on the next launch.
    Ready(String),
    Failed(String),
}

struct App {
    store: Store,
    client: Arc<Client>,
    platform: Platform,
    state: State,
    catalog: Catalog,
    games: BTreeMap<String, Game>,
    sender: mpsc::Sender<Event>,
    events: mpsc::Receiver<Event>,
    self_update: SelfUpdate,
    last_check: Instant,
}

impl App {
    fn spawn(&self, context: &egui::Context, work: impl FnOnce(&dyn Fn(Event)) + Send + 'static) {
        let sender = self.sender.clone();
        let context = context.clone();
        std::thread::spawn(move || {
            work(&|event| {
                let _ = sender.send(event);
                context.request_repaint();
            })
        });
    }

    fn check_everything(&mut self, context: &egui::Context) {
        self.last_check = Instant::now();
        let client = self.client.clone();
        self.spawn(context, move |send| send(Event::Catalog(load_catalog(&client))));
        // Never replace a development build with a published one.
        if !cfg!(debug_assertions) && matches!(self.self_update, SelfUpdate::Idle | SelfUpdate::Failed(_)) {
            let (client, store, platform) = (self.client.clone(), self.store.clone(), self.platform);
            self.spawn(context, move |send| {
                let result = selfupdate::available(&client, platform).and_then(|release| match release {
                    Some(release) => selfupdate::apply(&client, &store, platform, &release)
                        .map(|()| Some(release.tag_name)),
                    None => Ok(None),
                });
                send(Event::SelfUpdate(result));
            });
        }
    }

    fn check_releases(&mut self, context: &egui::Context) {
        for entry in &self.catalog.entries {
            let (client, id, repo) = (self.client.clone(), entry.id.clone(), entry.repo.clone());
            self.spawn(context, move |send| {
                send(Event::Release(id, client.latest_release(&repo)));
            });
        }
    }

    fn entry(&self, id: &str) -> Option<&Entry> {
        self.catalog.entries.iter().find(|entry| entry.id == id)
    }

    fn installed(&self, id: &str) -> Option<&Installed> {
        self.state.installed.get(id)
    }

    fn install(&mut self, context: &egui::Context, id: &str) {
        let Some(entry) = self.entry(id).cloned() else { return };
        let game = self.games.entry(id.to_string()).or_default();
        let Some(Some(release)) = game.release.clone() else { return };
        if game.job.is_some() || game.child.is_some() {
            return;
        }
        game.job = Some(Progress::Downloading { done: 0, total: 0 });
        game.error = None;
        let (client, store, platform) = (self.client.clone(), self.store.clone(), self.platform);
        self.spawn(context, move |send| {
            let id = entry.id.clone();
            let result = store.install(&client, &entry, &release, platform, |progress| {
                send(Event::Progress(id.clone(), progress))
            });
            send(Event::Installed(id.clone(), result));
        });
    }

    fn save(&mut self) {
        if let Err(error) = self.store.save_state(&self.state) {
            self.self_update = SelfUpdate::Failed(error);
        }
    }

    fn receive(&mut self, context: &egui::Context) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Catalog(catalog) => {
                    self.catalog = catalog;
                    self.check_releases(context);
                }
                Event::Release(id, result) => {
                    let game = self.games.entry(id.clone()).or_default();
                    match result {
                        Ok(release) => {
                            game.release = Some(release);
                            game.error = None;
                        }
                        Err(error) => game.error = Some(error),
                    }
                    if self.state.auto_update_games && self.update_available(&id) {
                        self.install(context, &id);
                    }
                }
                Event::Progress(id, progress) => {
                    self.games.entry(id).or_default().job = Some(progress);
                }
                Event::Installed(id, result) => {
                    let game = self.games.entry(id.clone()).or_default();
                    game.job = None;
                    match result {
                        Ok(installed) => {
                            self.state.installed.insert(id, installed);
                            self.save();
                        }
                        Err(error) => game.error = Some(error),
                    }
                }
                Event::SelfUpdate(Ok(Some(version))) => self.self_update = SelfUpdate::Ready(version),
                Event::SelfUpdate(Ok(None)) => self.self_update = SelfUpdate::Idle,
                Event::SelfUpdate(Err(error)) => self.self_update = SelfUpdate::Failed(error),
            }
        }
        for game in self.games.values_mut() {
            if let Some(child) = &mut game.child
                && !matches!(child.try_wait(), Ok(None))
            {
                game.child = None;
            }
        }
    }

    /// The release that could be installed here, if the latest one has a build.
    fn installable(&self, id: &str) -> Option<&Release> {
        let release = self.games.get(id)?.release.as_ref()?.as_ref()?;
        let entry = self.entry(id)?;
        pick_asset(entry, &release.assets, self.platform).map(|_| release)
    }

    fn update_available(&self, id: &str) -> bool {
        match (self.installed(id), self.installable(id)) {
            (Some(installed), Some(release)) => installed.tag != release.tag_name,
            _ => false,
        }
    }

    fn busy(&self) -> bool {
        self.games.values().any(|game| game.job.is_some())
    }

    fn card(&self, ui: &mut egui::Ui, entry: &Entry, actions: &mut Vec<Action>) {
        let empty = Game::default();
        let game = self.games.get(&entry.id).unwrap_or(&empty);
        let installed = self.installed(&entry.id);
        let installable = self.installable(&entry.id);
        let weak = ui.visuals().weak_text_color();
        egui::Frame::group(ui.style()).inner_margin(14.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&entry.name).size(21.0).strong());
                if entry.kind != "game" {
                    ui.label(egui::RichText::new(&entry.kind).color(weak));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if game.job.is_some() {
                        ui.spinner();
                    } else if game.child.is_some() {
                        ui.label("Running");
                    } else if installed.is_some() {
                        let play = egui::Button::new(egui::RichText::new("  Play  ").strong())
                            .fill(egui::Color32::from_rgb(38, 120, 78));
                        if ui.add(play).clicked() {
                            actions.push(Action::Play(entry.id.clone()));
                        }
                        if self.update_available(&entry.id) && ui.button("Update").clicked() {
                            actions.push(Action::Install(entry.id.clone()));
                        }
                        if ui.small_button("Uninstall").clicked() {
                            actions.push(Action::Uninstall(entry.id.clone()));
                        }
                    } else if let Some(release) = installable {
                        let megabytes = pick_asset(entry, &release.assets, self.platform)
                            .map_or(0, |asset| asset.size / 1_000_000);
                        if ui.button(format!("Install ({megabytes} MB)")).clicked() {
                            actions.push(Action::Install(entry.id.clone()));
                        }
                    }
                });
            });
            if !entry.description.is_empty() {
                ui.label(&entry.description);
            }
            let status = match (installed, &game.release) {
                (Some(installed), _) if self.update_available(&entry.id) => format!(
                    "Installed {} · {} available",
                    installed.tag,
                    installable.map_or("", |release| &release.tag_name)
                ),
                (Some(installed), Some(_)) => format!("Installed {} · up to date", installed.tag),
                (Some(installed), None) => format!("Installed {}", installed.tag),
                (None, None) if game.error.is_none() => "Checking…".into(),
                (None, None) => String::new(),
                (None, Some(None)) => "No releases yet".into(),
                (None, Some(Some(release))) if installable.is_none() => format!(
                    "{} has no build for {} {}",
                    release.tag_name, self.platform.os, self.platform.arch
                ),
                (None, Some(Some(release))) => format!("Latest: {}", release.tag_name),
            };
            if !status.is_empty() {
                ui.label(egui::RichText::new(status).color(weak));
            }
            match &game.job {
                Some(Progress::Downloading { done, total }) if *total > 0 => {
                    let text = format!("{} / {} MB", done / 1_000_000, total / 1_000_000);
                    ui.add(egui::ProgressBar::new(*done as f32 / *total as f32).text(text));
                }
                Some(Progress::Downloading { .. }) => {
                    ui.label("Starting download…");
                }
                Some(Progress::Installing) => {
                    ui.label("Unpacking…");
                }
                None => {}
            }
            if let Some(error) = &game.error {
                ui.colored_label(ui.visuals().warn_fg_color, error);
            }
        });
    }
}

impl eframe::App for App {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive(context);
        if self.last_check.elapsed() > RECHECK_EVERY && !self.busy() {
            self.check_everything(context);
        }
        // Keep noticing finished games and the periodic re-check while idle.
        context.request_repaint_after(Duration::from_secs(2));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let mut actions = Vec::new();
        let mut refresh = false;
        let mut auto_update = self.state.auto_update_games;
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(20.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(egui::RichText::new("Games").size(28.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        refresh = ui.button("Check for updates").clicked();
                        ui.checkbox(&mut auto_update, "Update games automatically");
                    });
                });
                match &self.self_update {
                    SelfUpdate::Ready(version) => {
                        ui.horizontal(|ui| {
                            ui.label(format!("Launcher {version} is installed."));
                            if ui.add_enabled(!self.busy(), egui::Button::new("Restart now")).clicked() {
                                selfupdate::restart();
                            }
                        });
                    }
                    SelfUpdate::Failed(error) => {
                        ui.colored_label(ui.visuals().warn_fg_color, error);
                    }
                    SelfUpdate::Idle => {}
                }
                ui.add_space(10.0);
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    for entry in &self.catalog.entries {
                        self.card(ui, entry, &mut actions);
                        ui.add_space(8.0);
                    }
                    ui.label(
                        egui::RichText::new(format!("Launcher {}", selfupdate::CURRENT))
                            .color(ui.visuals().weak_text_color()),
                    );
                });
            });

        let context = ui.ctx().clone();
        if auto_update != self.state.auto_update_games {
            self.state.auto_update_games = auto_update;
            self.save();
            refresh |= auto_update;
        }
        if refresh {
            self.check_everything(&context);
        }
        for action in actions {
            match action {
                Action::Install(id) => self.install(&context, &id),
                Action::Play(id) => {
                    let (Some(entry), Some(installed)) = (self.entry(&id), self.installed(&id)) else {
                        continue;
                    };
                    let result = self.store.launch(entry, installed);
                    let game = self.games.entry(id).or_default();
                    match result {
                        Ok(child) => {
                            game.child = Some(child);
                            game.error = None;
                        }
                        Err(error) => game.error = Some(error),
                    }
                }
                Action::Uninstall(id) => {
                    let result = self.store.uninstall(&id);
                    self.state.installed.remove(&id);
                    self.save();
                    self.games.entry(id).or_default().error = result.err();
                }
            }
        }
    }
}

pub fn run(store: Store) -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 780.0])
            .with_min_inner_size([520.0, 360.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Launcher",
        options,
        Box::new(move |creation| {
            creation.egui_ctx.all_styles_mut(|style| {
                style.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(16.0));
                style.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(16.0));
                style.spacing.button_padding = egui::vec2(12.0, 7.0);
            });
            let (sender, events) = mpsc::channel();
            let mut state = store.load_state();
            // Forget installs whose files were removed behind our back.
            state.installed.retain(|id, installed| store.is_intact(id, installed));
            let mut app = App {
                client: Arc::new(Client::new(store.cache_dir())),
                platform: Platform::current(),
                state,
                catalog: Catalog::embedded(),
                games: BTreeMap::new(),
                sender,
                events,
                self_update: SelfUpdate::Idle,
                last_check: Instant::now(),
                store,
            };
            app.check_everything(&creation.egui_ctx);
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| format!("Cannot open the launcher window: {error}"))
}
