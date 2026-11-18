//! iOS HUD Helper — the window.
//!
//! Three steps, one button each:
//!   1. Show the Developer Mode switch on the device.
//!   2. Unlock Settings → Developer (where the Graphics HUD switch lives).
//!   3. Launch a game with the Metal Performance HUD turned on.

// In release builds on Windows, don't open a black console window next to the app.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod ddi;
mod device;
mod worker;

use eframe::egui::{self, Color32, RichText};

use device::{AppInfo, DeviceInfo, DeviceStatus};
use worker::{LogKind, Msg, Worker};

fn main() -> eframe::Result {
    // Both the device library and the downloader use this encryption library.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("iOS HUD Helper")
            .with_inner_size([620.0, 820.0])
            .with_min_inner_size([460.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "iOS HUD Helper",
        options,
        Box::new(|cc| Ok(Box::new(HudHelper::new(cc.egui_ctx.clone())))),
    )
}

struct HudHelper {
    worker: Worker,
    busy: bool,
    devices: Vec<DeviceInfo>,
    selected: usize,
    status: Option<DeviceStatus>,
    apps: Vec<AppInfo>,
    search: String,
    chosen_app: Option<String>,
    close_first: bool,
    log: Vec<(LogKind, String)>,
    progress: Option<(String, f32)>,
}

impl HudHelper {
    fn new(ctx: egui::Context) -> Self {
        let mut app = Self {
            worker: Worker::new(ctx),
            busy: false,
            devices: Vec::new(),
            selected: 0,
            status: None,
            apps: Vec::new(),
            search: String::new(),
            chosen_app: None,
            close_first: true,
            log: Vec::new(),
            progress: None,
        };
        app.refresh_devices();
        app
    }

    fn device(&self) -> Option<DeviceInfo> {
        self.devices.get(self.selected).cloned()
    }

    // ----- Button actions: each one starts a background job -----

    fn refresh_devices(&mut self) {
        self.busy = true;
        self.worker.spawn(|rep| async move {
            let devices = device::list_devices(&rep).await?;
            let first = devices.first().cloned();
            rep.send(Msg::Devices(devices));
            if let Some(dev) = first {
                let status = device::status(&dev).await?;
                rep.send(Msg::Status(dev.udid, status));
            }
            Ok(())
        });
    }

    /// Runs a job for the selected device, then re-checks its status.
    fn run_on_device<F, Fut>(&mut self, job: F)
    where
        F: FnOnce(DeviceInfo, worker::Reporter) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = anyhow::Result<()>> + 'static,
    {
        let Some(dev) = self.device() else { return };
        self.busy = true;
        self.worker.spawn(|rep| async move {
            let result = job(dev.clone(), rep.clone()).await;
            if let Ok(status) = device::status(&dev).await {
                rep.send(Msg::Status(dev.udid.clone(), status));
            }
            result
        });
    }

    fn check_status(&mut self) {
        self.run_on_device(|_, _| async { Ok(()) });
    }

    fn reveal_developer_mode(&mut self) {
        self.run_on_device(|dev, rep| async move { device::reveal_developer_mode(&dev, &rep).await });
    }

    fn unlock_menu(&mut self) {
        self.run_on_device(|dev, rep| async move { device::unlock_developer_menu(&dev, &rep).await });
    }

    fn load_apps(&mut self) {
        self.run_on_device(|dev, rep| async move {
            let apps = device::list_apps(&dev).await?;
            rep.log(format!("Found {} apps.", apps.len()));
            rep.send(Msg::Apps(apps));
            Ok(())
        });
    }

    fn launch(&mut self) {
        let Some(app) = self.apps.iter().find(|a| Some(&a.bundle_id) == self.chosen_app.as_ref()).cloned() else {
            return;
        };
        let close_first = self.close_first;
        self.run_on_device(move |dev, rep| async move { device::launch_with_hud(&dev, &app, close_first, &rep).await });
    }

    /// Reads messages that background jobs sent since the last frame.
    fn read_inbox(&mut self) {
        while let Ok(msg) = self.worker.inbox.try_recv() {
            match msg {
                Msg::Log(kind, text) => self.log.push((kind, text)),
                Msg::Devices(devices) => {
                    let old = self.device().map(|d| d.udid);
                    self.devices = devices;
                    self.selected = old
                        .and_then(|udid| self.devices.iter().position(|d| d.udid == udid))
                        .unwrap_or(0);
                    self.status = None;
                    if self.devices.is_empty() {
                        self.apps.clear();
                    }
                }
                Msg::Status(udid, status) => {
                    if self.device().is_some_and(|d| d.udid == udid) {
                        self.status = Some(status);
                    }
                }
                Msg::Apps(apps) => self.apps = apps,
                Msg::Progress(p) => self.progress = p,
                Msg::Finished => self.busy = false,
            }
        }
    }
}

impl eframe::App for HudHelper {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.read_inbox();

        egui::Panel::bottom("activity")
            .resizable(true)
            .default_size(130.0)
            .min_size(90.0)
            .show(ui, |ui| self.activity_log(ui));

        egui::CentralPanel::default_margins().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                ui.add_space(4.0);
                ui.heading(RichText::new("iOS HUD Helper").size(26.0).strong());
                ui.label("Turn on Apple's Metal Performance HUD on your iPhone or iPad, from Windows.");
                ui.add_space(10.0);

                self.device_card(ui);
                let ready = self.device().is_some() && !self.busy;
                self.step_one(ui, ready);
                self.step_two(ui, ready);
                self.step_three(ui, ready);
            });
        });
    }
}

// ----- Drawing -----

fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .inner_margin(14)
        .corner_radius(10)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(10.0);
}

fn step_title(ui: &mut egui::Ui, number: &str, title: &str, done: bool) {
    ui.horizontal(|ui| {
        let badge = if done { "✔".to_owned() } else { number.to_owned() };
        let color = if done { Color32::from_rgb(46, 160, 67) } else { ui.visuals().selection.bg_fill };
        ui.label(RichText::new(format!(" {badge} ")).strong().color(Color32::WHITE).background_color(color));
        ui.label(RichText::new(title).size(18.0).strong());
    });
    ui.add_space(4.0);
}

fn yes_no(value: Option<bool>, yes: &str, no: &str) -> RichText {
    match value {
        Some(true) => RichText::new(format!("✔ {yes}")).color(Color32::from_rgb(46, 160, 67)),
        Some(false) => RichText::new(format!("✖ {no}")).color(Color32::from_rgb(207, 34, 46)),
        None => RichText::new("? checking…").weak(),
    }
}

impl HudHelper {
    fn device_card(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Device").size(18.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(!self.busy, egui::Button::new("⟳ Refresh")).clicked() {
                        self.refresh_devices();
                    }
                    if self.busy {
                        ui.spinner();
                    }
                });
            });
            ui.add_space(4.0);

            if self.devices.is_empty() {
                if self.busy {
                    ui.label("Looking for devices…");
                } else {
                    ui.label(RichText::new("No iPhone or iPad found.").strong());
                    ui.label("1. Install iTunes or the \"Apple Devices\" app (they include Apple's USB drivers).");
                    ui.label("2. Connect your device with a cable and unlock it.");
                    ui.label("3. If it asks \"Trust This Computer?\", tap Trust. Then press Refresh.");
                }
                return;
            }

            let before = self.selected;
            egui::ComboBox::from_id_salt("device")
                .width(ui.available_width())
                .selected_text(self.devices[self.selected].label())
                .show_ui(ui, |ui| {
                    for (i, dev) in self.devices.iter().enumerate() {
                        ui.selectable_value(&mut self.selected, i, dev.label());
                    }
                });
            if self.selected != before {
                self.status = None;
                self.apps.clear();
                self.chosen_app = None;
                self.check_status();
            }

            ui.add_space(6.0);
            let status = self.status.clone().unwrap_or_default();
            egui::Grid::new("status").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                ui.label("Developer Mode");
                ui.label(yes_no(status.developer_mode, "On", "Off"));
                ui.end_row();
                ui.label("Developer menu");
                ui.label(yes_no(status.menu_unlocked, "Unlocked", "Locked"));
                ui.end_row();
            });

            if let Some((label, fraction)) = &self.progress {
                ui.add_space(6.0);
                ui.add(egui::ProgressBar::new(*fraction).text(format!("{label}… {:.0}%", fraction * 100.0)));
            }
        });
    }

    fn step_one(&mut self, ui: &mut egui::Ui, ready: bool) {
        let done = self.status.as_ref().and_then(|s| s.developer_mode) == Some(true);
        card(ui, |ui| {
            step_title(ui, "1", "Turn on Developer Mode", done);
            if done {
                ui.label("Developer Mode is on. You can skip this step.");
                return;
            }
            ui.label("Click the button, then on your device go to Settings → Privacy & Security → Developer Mode, \
                      turn it on and restart. After the restart, unlock it and tap \"Turn On\".");
            ui.add_space(6.0);
            if ui.add_enabled(ready, egui::Button::new("Show Developer Mode switch")).clicked() {
                self.reveal_developer_mode();
            }
        });
    }

    fn step_two(&mut self, ui: &mut egui::Ui, ready: bool) {
        let done = self.status.as_ref().and_then(|s| s.menu_unlocked) == Some(true);
        card(ui, |ui| {
            step_title(ui, "2", "Unlock the Developer menu", done);
            if !done {
                ui.label("Downloads Apple's developer files (about 20 MB, only the first time) and installs them on \
                          your device. This is the same thing a Mac with Xcode does.");
                ui.add_space(4.0);
            }
            ui.label(RichText::new("On your device: Settings → Developer → Graphics HUD → turn on \"Show Graphics HUD\".").strong());
            ui.label(RichText::new("The menu locks again when the device restarts. Just click this button again.").weak());
            ui.add_space(6.0);
            let text = if done { "Check again" } else { "Unlock Developer menu" };
            if ui.add_enabled(ready, egui::Button::new(text)).clicked() {
                self.unlock_menu();
            }
        });
    }

    fn step_three(&mut self, ui: &mut egui::Ui, ready: bool) {
        card(ui, |ui| {
            step_title(ui, "3", "Launch a game with the HUD", false);
            ui.label("Some games only show the HUD when they're started this way. Pick a game and launch it from here.");
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                let label = if self.apps.is_empty() { "Load my apps" } else { "⟳ Reload apps" };
                if ui.add_enabled(ready, egui::Button::new(label)).clicked() {
                    self.load_apps();
                }
                if !self.apps.is_empty() {
                    ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("🔍 Search…").desired_width(f32::INFINITY));
                }
            });

            if self.apps.is_empty() {
                return;
            }
            ui.add_space(6.0);
            let search = self.search.to_lowercase();
            egui::Frame::NONE.inner_margin(4).show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(220.0).auto_shrink([false, true]).show(ui, |ui| {
                    for app in self.apps.iter().filter(|a| {
                        search.is_empty() || a.name.to_lowercase().contains(&search) || a.bundle_id.to_lowercase().contains(&search)
                    }) {
                        let selected = self.chosen_app.as_ref() == Some(&app.bundle_id);
                        let text = format!("{}   {}", app.name, app.version);
                        if ui.selectable_label(selected, text).on_hover_text(&app.bundle_id).clicked() {
                            self.chosen_app = Some(app.bundle_id.clone());
                        }
                    }
                });
            });

            ui.add_space(6.0);
            ui.checkbox(&mut self.close_first, "Close the game first if it's already open (recommended)");
            ui.add_space(4.0);
            let chosen = self.apps.iter().find(|a| Some(&a.bundle_id) == self.chosen_app.as_ref()).map(|a| a.name.clone());
            let button_text = match &chosen {
                Some(name) => format!("▶ Launch {name} with HUD"),
                None => "Pick a game above".to_owned(),
            };
            if ui.add_enabled(ready && chosen.is_some(), egui::Button::new(RichText::new(button_text).strong())).clicked() {
                self.launch();
            }
        });
    }

    fn activity_log(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Activity").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Copy").clicked() {
                    let text: Vec<&str> = self.log.iter().map(|(_, t)| t.as_str()).collect();
                    ui.ctx().copy_text(text.join("\n"));
                }
                if ui.small_button("Clear").clicked() {
                    self.log.clear();
                }
            });
        });
        egui::ScrollArea::vertical().auto_shrink(false).stick_to_bottom(true).show(ui, |ui| {
            if self.log.is_empty() {
                ui.label(RichText::new("Messages about what the app is doing will show up here.").weak());
            }
            for (kind, text) in &self.log {
                let text = match kind {
                    LogKind::Info => RichText::new(format!("• {text}")),
                    LogKind::Success => RichText::new(format!("✔ {text}")).color(Color32::from_rgb(46, 160, 67)),
                    LogKind::Error => RichText::new(format!("✖ {text}")).color(Color32::from_rgb(207, 34, 46)),
                };
                ui.label(text);
            }
        });
    }
}
