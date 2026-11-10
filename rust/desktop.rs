#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod brand;
mod desktop_shell;
mod updates;

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering::Relaxed},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossbeam_channel::Receiver;
use desktop_shell::{Action, Instance, State, Tray};
use eframe::egui::{self, Color32, RichText, Stroke, Vec2, vec2};
use voiceisolator::{
    RATE,
    engine::{self, BATCHES, Config, Gains, Processing, Session, Status},
    isolation::Backend,
    latency::Latency,
    playback::Correction,
    preferences::{self, Choices, Profile, Settings},
    wasapi::{self, Devices, Event},
};

fn main() -> Result<()> {
    let arg = std::env::args_os().nth(1);
    if arg.as_deref() == Some("--engine".as_ref()) {
        return engine::run_child();
    }
    if arg.as_deref() == Some("--quit".as_ref()) {
        return Instance::request_quit();
    }
    let startup = arg.as_deref() == Some("--startup".as_ref());
    let after_update = arg.as_deref() == Some("--after-update".as_ref());
    let Some(instance) = Instance::acquire(!startup)? else {
        return Ok(());
    };
    let installed = std::env::current_exe()?
        .with_file_name("installed-release")
        .is_file();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([784.0, 754.0])
            .with_min_inner_size([580.0, 540.0])
            .with_icon(egui::IconData {
                rgba: brand::waveform(brand::STANDBY),
                width: brand::SIZE,
                height: brand::SIZE,
            })
            .with_active(!startup)
            .with_visible(!startup),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "VoiceIsolator",
        options,
        Box::new(move |cc| {
            Ok(Box::new(Desktop::new(
                &cc.egui_ctx,
                instance,
                installed,
                startup,
                after_update,
            )?))
        }),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))
}

struct Recording {
    stop: Arc<Event>,
    level: Arc<AtomicU32>,
    done: Receiver<Result<String>>,
    since: Instant,
    canceled: bool,
}

impl Recording {
    fn start(microphone: String, path: PathBuf) -> Result<Self> {
        let stop = Arc::new(Event::new(true)?);
        let level = Arc::new(AtomicU32::new(0));
        let (send, done) = crossbeam_channel::bounded(1);
        let thread_stop = stop.clone();
        let thread_level = level.clone();
        thread::spawn(move || {
            let result = (|| -> Result<String> {
                let samples = wasapi::record(&microphone, &thread_stop, |level| {
                    thread_level.fetch_max(level.to_bits(), Relaxed);
                })?;
                anyhow::ensure!(
                    wasapi::wait([&thread_stop], 0)?.is_none(),
                    "Recording canceled"
                );
                let profile = Profile {
                    microphone,
                    samples,
                };
                preferences::save_profile(&path, &profile)?;
                Ok(profile.microphone.clone())
            })();
            let _ = send.send(result);
        });
        Ok(Self {
            stop,
            level,
            done,
            since: Instant::now(),
            canceled: false,
        })
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.stop.signal();
    }
}

enum UpdateTask {
    Check(Receiver<Result<Option<updates::Release>>>),
    Download {
        cancel: Arc<AtomicBool>,
        done: Receiver<Result<updates::Setup>>,
    },
}

struct Desktop {
    instance: Instance,
    tray: Tray,
    installed: bool,
    hidden: bool,
    quitting: bool,
    settings: Settings,
    settings_dirty: bool,
    startup: bool,
    resume_until: Option<Instant>,
    last_device_refresh: Instant,
    pending_save: bool,
    status: Status,
    update_task: Option<UpdateTask>,
    update_available: Option<updates::Release>,
    update_message: Option<String>,
    devices: Option<Devices>,
    device_update: Option<Receiver<Result<Devices>>>,
    choices: Choices,
    preferred: Choices,
    profile_path: Result<PathBuf, String>,
    enrolled: Option<String>,
    dark: bool,
    processing: Processing,
    gains: Gains,
    session: Option<Session>,
    restart: Option<Config>,
    recording: Option<Recording>,
    measurement_start: Option<Instant>,
    latency: Option<Latency>,
    latency_error: Option<String>,
    notice: Option<(bool, String)>,
    input_meter: f32,
    output_meter: f32,
    meter_time: Instant,
}

impl Desktop {
    fn new(
        ctx: &egui::Context,
        instance: Instance,
        installed: bool,
        hidden: bool,
        after_update: bool,
    ) -> Result<Self> {
        let (mut settings, settings_error) = match Settings::load() {
            Ok(settings) => (settings, None),
            Err(error) => (
                Settings::default(),
                Some((true, format!("Could not read saved settings. {error:#}"))),
            ),
        };
        let preferred = settings.choices.clone();
        let profile_path = preferences::profile_path().map_err(|error| format!("{error:#}"));
        let (enrolled, mut notice) = match &profile_path {
            Ok(path) => match preferences::load_profile(path) {
                Ok(profile) => (profile.map(|profile| profile.microphone.clone()), None),
                Err(error) => (None, Some((true, format!("{error:#}")))),
            },
            Err(error) => (None, Some((true, error.clone()))),
        };
        let mut resume_until = None;
        let mut settings_dirty = false;
        if settings.resume {
            if !after_update {
                if enrolled.as_ref() == Some(&preferred.microphone) && !preferred.cable.is_empty() {
                    resume_until = Some(Instant::now() + Duration::from_secs(30));
                } else if notice.is_none() {
                    notice = Some((true, "Could not resume isolation. The saved microphone, cable or voice profile is missing.".to_owned()));
                }
            }
            settings.resume = false;
            settings_dirty = true;
        }
        let dark = settings.dark;
        theme(ctx, dark);
        let mut app = Self {
            instance,
            tray: Tray::new(ctx, installed)?,
            installed,
            hidden,
            quitting: false,
            startup: preferences::startup_enabled(),
            resume_until,
            last_device_refresh: Instant::now(),
            pending_save: false,
            status: Status::default(),
            update_task: None,
            update_available: None,
            update_message: None,
            devices: None,
            device_update: None,
            choices: preferred.clone(),
            preferred,
            profile_path,
            enrolled,
            dark,
            processing: settings.processing.clone(),
            gains: settings.gains,
            session: None,
            restart: None,
            recording: None,
            measurement_start: None,
            latency: None,
            latency_error: None,
            notice: notice.or(settings_error),
            input_meter: 0.0,
            output_meter: 0.0,
            meter_time: Instant::now(),
            settings,
            settings_dirty,
        };
        app.refresh(ctx);
        app.save_settings();
        if installed {
            thread::spawn(updates::cleanup);
        }
        Ok(app)
    }

    fn refresh(&mut self, ctx: &egui::Context) {
        self.last_device_refresh = Instant::now();
        let (send, receive) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = send.send(wasapi::enumerate());
            ctx.request_repaint();
        });
        self.device_update = Some(receive);
    }

    fn selected_profile(&self) -> bool {
        self.enrolled
            .as_ref()
            .is_some_and(|id| id == &self.choices.microphone)
    }

    fn config(&self) -> Config {
        let mut config =
            engine::default_config(self.choices.microphone.clone(), self.choices.cable.clone());
        config.processing = self.processing.clone();
        config.gains = self.gains;
        config
    }

    fn changed(&self) -> bool {
        self.session.as_ref().is_some_and(|session| {
            session.config.processing != self.processing
                || session.config.gains != self.gains
                || session.config.cable != self.choices.cable
        })
    }

    fn save_settings(&mut self) {
        if self.settings_dirty {
            self.settings_dirty = false;
            let result = self.settings.save();
            self.report(result);
        }
    }

    fn save_standby_edits(&mut self) {
        if self.session.is_some() || self.resume_until.is_some() || self.quitting {
            return;
        }
        if self.settings.choices != self.preferred
            || self.settings.processing != self.processing
            || self.settings.gains != self.gains
        {
            self.settings.choices = self.preferred.clone();
            self.settings.processing = self.processing.clone();
            self.settings.gains = self.gains;
            self.settings_dirty = true;
        }
        self.save_settings();
    }

    fn can_start(&self) -> bool {
        self.recording.is_none()
            && !self.downloading()
            && self.selected_profile()
            && self.devices.as_ref().is_some_and(|devices| {
                devices
                    .microphones
                    .iter()
                    .any(|device| device.id == self.choices.microphone)
                    && devices
                        .cables
                        .iter()
                        .any(|device| device.id == self.choices.cable)
            })
    }

    fn show(&mut self, ctx: &egui::Context) {
        self.hidden = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn restore_approved(&mut self) {
        self.preferred = self.settings.choices.clone();
        self.choices = self.preferred.clone();
        self.processing = self.settings.processing.clone();
        self.gains = self.settings.gains;
    }

    fn quit(&mut self) {
        self.quitting = true;
        self.resume_until = None;
        self.restart = None;
        if let Some(session) = &mut self.session {
            session.stop();
        }
        if let Some(recording) = &mut self.recording {
            recording.canceled = true;
            recording.stop.signal();
        }
        if let Some(UpdateTask::Download { cancel, .. }) = &self.update_task {
            cancel.store(true, Relaxed);
        }
    }

    fn downloading(&self) -> bool {
        matches!(self.update_task, Some(UpdateTask::Download { .. }))
    }

    fn check_updates(&mut self, ctx: &egui::Context) {
        if self.update_task.is_some() {
            return;
        }
        if !updates::configured() {
            self.update_message = Some("This build has no update verification key.".into());
            return;
        }
        self.settings.last_update_check = updates::now();
        self.settings_dirty = true;
        self.save_settings();
        let (send, done) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = send.send(updates::check());
            ctx.request_repaint();
        });
        self.update_task = Some(UpdateTask::Check(done));
        self.update_message = Some("Checking for updates…".into());
    }

    fn install_update(&mut self, ctx: &egui::Context) {
        if self.session.is_some()
            || self.recording.is_some()
            || self.resume_until.is_some()
            || self.update_task.is_some()
        {
            return;
        }
        let Some(release) = self.update_available.clone() else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = cancel.clone();
        let (send, done) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = send.send(updates::download(release, &thread_cancel));
            ctx.request_repaint();
        });
        self.update_task = Some(UpdateTask::Download { cancel, done });
        self.update_message = Some("Downloading and verifying the update…".into());
    }

    fn poll_updates(&mut self, ctx: &egui::Context) {
        if !self.installed || self.quitting {
            return;
        }
        match &self.update_task {
            Some(UpdateTask::Check(done)) => {
                if let Ok(result) = done.try_recv() {
                    self.update_task = None;
                    match result {
                        Ok(release) => {
                            self.update_message = Some(match &release {
                                Some(release) => {
                                    format!("VoiceIsolator {} is available.", release.version)
                                }
                                None => "VoiceIsolator is up to date.".into(),
                            });
                            self.update_available = release;
                        }
                        Err(error) => {
                            self.update_message =
                                Some(format!("Could not check for updates. {error:#}"))
                        }
                    }
                }
            }
            Some(UpdateTask::Download { cancel, done }) => {
                if let Ok(result) = done.try_recv() {
                    let canceled = cancel.load(Relaxed);
                    self.update_task = None;
                    if canceled {
                        self.update_message = Some("Update download canceled.".into());
                    } else {
                        match result.and_then(updates::Setup::launch) {
                            Ok(()) => self.quit(),
                            Err(error) => {
                                self.update_message =
                                    Some(format!("Could not install the update. {error:#}"))
                            }
                        }
                    }
                }
            }
            None if self.settings.auto_updates
                && updates::configured()
                && updates::now().saturating_sub(self.settings.last_update_check) >= 86_400 =>
            {
                self.check_updates(ctx);
            }
            None => {}
        }
    }

    fn report(&mut self, result: Result<()>) {
        if let Err(error) = result {
            self.notice = Some((true, format!("{error:#}")));
        }
    }

    fn clear_latency(&mut self) {
        self.measurement_start = None;
        self.latency = None;
        self.latency_error = None;
    }

    fn launch(&mut self, config: Config) {
        self.notice = None;
        self.clear_latency();
        match Session::launch(config) {
            Ok(session) => {
                self.session = Some(session);
                self.pending_save = true;
            }
            Err(error) => {
                self.restore_approved();
                self.settings.resume = false;
                self.settings_dirty = true;
                self.save_settings();
                self.notice = Some((true, format!("Could not start isolation. {error:#}")));
            }
        }
    }

    fn stop(&mut self) {
        self.restart = None;
        self.resume_until = None;
        self.pending_save = false;
        self.settings.resume = false;
        self.settings_dirty = true;
        self.save_settings();
        self.restore_approved();
        self.clear_latency();
        if let Some(session) = self.session.as_mut() {
            session.stop();
        }
    }

    fn isolation_action(&mut self) {
        if self.session.is_none() {
            if self.can_start() {
                self.resume_until = None;
                self.launch(self.config());
            }
        } else if !self.changed() {
            self.stop();
        } else {
            self.restart = Some(self.config());
            self.pending_save = false;
            self.session.as_mut().unwrap().stop();
            self.clear_latency();
        }
    }

    fn voice_action(&mut self) -> Result<()> {
        if let Some(recording) = self.recording.as_mut() {
            recording.canceled = true;
            recording.stop.signal();
            return Ok(());
        }
        let path = self
            .profile_path
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?
            .clone();
        if self.selected_profile() {
            preferences::delete_profile(&path)?;
            self.enrolled = None;
            self.notice = Some((false, "Voice profile removed.".to_owned()));
        } else {
            self.recording = Some(Recording::start(self.choices.microphone.clone(), path)?);
            self.notice = None;
        }
        Ok(())
    }

    fn poll(&mut self, ctx: &egui::Context) -> Status {
        if let Some(result) = self
            .device_update
            .as_ref()
            .and_then(|receive| receive.try_recv().ok())
        {
            self.device_update = None;
            match result {
                Ok(devices) => {
                    if self.resume_until.is_none() && self.session.is_none() {
                        let selected = self.preferred.resolve(&devices, self.enrolled.as_deref());
                        if self.recording.is_none() {
                            self.choices.microphone = selected.microphone;
                        }
                        self.choices.cable = selected.cable;
                    }
                    self.devices = Some(devices);
                }
                Err(error) => {
                    self.notice =
                        Some((true, format!("Could not refresh audio devices. {error:#}")))
                }
            }
        }
        if let Some(deadline) = self.resume_until {
            if self.can_start() {
                self.resume_until = None;
                self.launch(self.config());
            } else if Instant::now() >= deadline {
                self.resume_until = None;
                self.notice = Some((true, "Could not resume isolation. The saved microphone or virtual cable did not appear within 30 seconds.".to_owned()));
                self.refresh(ctx);
            } else if self.device_update.is_none()
                && self.last_device_refresh.elapsed() >= Duration::from_secs(1)
            {
                self.refresh(ctx);
            }
        }
        let mut recorded_peak = 0.0;
        if let Some(recording) = self.recording.as_ref() {
            recorded_peak = f32::from_bits(recording.level.swap(0, Relaxed));
            if let Ok(result) = recording.done.try_recv() {
                let canceled = recording.canceled;
                self.recording = None;
                match result {
                    Ok(id) => {
                        self.enrolled = Some(id);
                        self.notice = Some((false, "Voice profile saved on this PC.".to_owned()));
                    }
                    Err(_) if canceled => {
                        self.notice = Some((false, "Recording canceled.".to_owned()))
                    }
                    Err(error) => self.notice = Some((true, format!("{error:#}"))),
                }
            }
        }
        let mut status = Status::default();
        let mut approved = None;
        if let Some(session) = self.session.as_mut() {
            match session.poll() {
                Ok(false) => {
                    status = session.status();
                    if status.live && self.pending_save && !session.stopping() && !self.quitting {
                        approved = Some(session.config.clone());
                        self.pending_save = false;
                    }
                    if let Some(result) = session.take_measurement()
                        && self.measurement_start.is_some()
                    {
                        self.measurement_start = None;
                        match result {
                            Ok(latency) => self.latency = Some(latency),
                            Err(error) => self.latency_error = Some(error),
                        }
                    }
                    if self
                        .measurement_start
                        .is_some_and(|start| start.elapsed() > Duration::from_secs(12))
                    {
                        let _ = session.cancel_measurement();
                        self.measurement_start = None;
                        self.latency_error = Some(
                            "Latency measurement timed out. Wait for Live and retry.".to_owned(),
                        );
                    }
                }
                Ok(true) => {
                    self.session = None;
                    if let Some(config) = self.restart.take() {
                        self.launch(config);
                    }
                }
                Err(error) => {
                    self.session = None;
                    self.restart = None;
                    self.pending_save = false;
                    self.clear_latency();
                    self.restore_approved();
                    self.settings.resume = false;
                    self.settings_dirty = true;
                    self.save_settings();
                    self.notice = Some((true, format!("{error:#}")));
                }
            }
        }
        if let Some(config) = approved {
            self.preferred = Choices {
                microphone: config.microphone,
                cable: config.cable,
            };
            self.settings.choices = self.preferred.clone();
            self.settings.processing = config.processing;
            self.settings.gains = config.gains;
            self.settings.resume = true;
            self.settings_dirty = true;
            self.save_settings();
        }
        let now = Instant::now();
        let elapsed = (now - self.meter_time).as_secs_f32();
        self.meter_time = now;
        self.input_meter = smooth(self.input_meter, status.input.max(recorded_peak), elapsed);
        self.output_meter = smooth(self.output_meter, status.output, elapsed);
        if self.session.is_some()
            || self.recording.is_some()
            || self.input_meter > 0.001
            || self.output_meter > 0.001
        {
            ctx.request_repaint_after(Duration::from_millis(if self.hidden { 125 } else { 33 }));
        }
        status
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.menu_button("Session", |ui| {
                if ui
                    .add_enabled(self.session.is_some(), egui::Button::new("Stop isolation"))
                    .clicked()
                {
                    self.stop();
                    ui.close();
                }
                if ui.button("Quit").clicked() {
                    self.quit();
                    ui.close();
                }
            });
            ui.menu_button("Settings", |ui| {
                if ui.button("Reset processing settings").clicked() {
                    self.processing = Processing::default();
                    self.gains = Gains::default();
                    ui.close();
                }
                if ui.checkbox(&mut self.dark, "Dark mode").changed() {
                    theme(ui.ctx(), self.dark);
                    self.settings.dark = self.dark;
                    self.settings_dirty = true;
                    self.save_settings();
                }
                if self.installed {
                    let mut enabled = self.startup;
                    if ui.checkbox(&mut enabled, "Start with Windows").changed() {
                        let result = preferences::set_startup(enabled);
                        if result.is_ok() {
                            self.startup = enabled;
                        }
                        self.report(result);
                    }
                    if ui
                        .checkbox(
                            &mut self.settings.auto_updates,
                            "Automatically check for updates",
                        )
                        .changed()
                    {
                        self.settings_dirty = true;
                        self.save_settings();
                    }
                    if ui
                        .add_enabled(
                            self.update_task.is_none(),
                            egui::Button::new("Check for updates"),
                        )
                        .clicked()
                    {
                        self.check_updates(ui.ctx());
                        ui.close();
                    }
                }
            });
        });
    }

    fn routing(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong("AUDIO ROUTING");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let enabled = self.device_update.is_none() && self.recording.is_none();
                    if ui
                        .add_enabled(enabled, egui::Button::new("Refresh devices"))
                        .clicked()
                    {
                        self.refresh(ui.ctx());
                    }
                });
            });
            let mut changed = [false; 2];
            if let Some(devices) = &self.devices {
                changed[0] = device_picker(
                    ui,
                    "Microphone",
                    &devices.microphones,
                    &mut self.choices.microphone,
                    self.session.is_none() && self.recording.is_none(),
                );
                changed[1] = device_picker(
                    ui,
                    "Virtual cable",
                    &devices.cables,
                    &mut self.choices.cable,
                    self.recording.is_none()
                        && !self.session.as_ref().is_some_and(Session::stopping),
                );
            } else {
                ui.label("Looking for Windows audio devices…");
            }
            if changed[0] {
                self.resume_until = None;
                self.preferred
                    .microphone
                    .clone_from(&self.choices.microphone);
            }
            if changed[1] && self.session.is_none() {
                self.resume_until = None;
                self.preferred.cable.clone_from(&self.choices.cable);
            }
            if self
                .devices
                .as_ref()
                .is_some_and(|devices| devices.cables.is_empty())
            {
                ui.weak(
                    "No supported virtual cable found. Install VB-CABLE, then refresh devices.",
                );
                ui.hyperlink_to("Get VB-CABLE", "https://vb-audio.com/Cable/");
            }
        });
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("PROCESSING CONTROLS");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let options = [
                    (Correction::Linear, "Linear"),
                    (Correction::BandLimited, "Band-limited"),
                ];
                let selected = options
                    .iter()
                    .find(|(value, _)| *value == self.processing.correction)
                    .unwrap()
                    .1;
                egui::ComboBox::from_id_salt("correction")
                    .width(112.0)
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for (value, name) in options {
                            ui.selectable_value(&mut self.processing.correction, value, name);
                        }
                    });
                ui.weak("Clock correction");
                if !cfg!(feature = "cpu-only") {
                    egui::ComboBox::from_id_salt("backend")
                    .width(72.0)
                    .selected_text(self.processing.backend.label())
                    .show_ui(ui, |ui| {
                        for backend in [Backend::Cpu, Backend::Cuda] {
                            ui.selectable_value(&mut self.processing.backend, backend, backend.label());
                        }
                    }).response.on_hover_text("CUDA requires the optional NVIDIA runtime. Applying a backend change restarts isolation.");
                    ui.weak("Backend");
                }
            });
        });
        let mut batch = BATCHES
            .iter()
            .position(|&batch| batch == self.processing.batch_ms)
            .unwrap_or(0) as i32;
        ui.columns(4, |columns| {
            knob(
                &mut columns[0],
                "Mic gain",
                &mut self.gains.mic_db,
                -12..=12,
                |x| format!("{x:+} dB"),
            );
            knob(
                &mut columns[1],
                "Output gain",
                &mut self.gains.output_db,
                -18..=12,
                |x| format!("{x:+} dB"),
            );
            knob(&mut columns[2], "Batch size", &mut batch, 0..=3, |x| {
                format!("{} ms", BATCHES[x as usize])
            });
            knob(
                &mut columns[3],
                "CPU threads",
                &mut self.processing.threads,
                1..=4,
                |x| x.to_string(),
            );
        });
        self.processing.batch_ms = BATCHES[batch as usize];
    }

    fn actions(&mut self, ui: &mut egui::Ui) {
        let stopping = self.session.as_ref().is_some_and(Session::stopping);
        let changed = self.changed();
        let voice = if let Some(recording) = &self.recording {
            if recording.canceled {
                "Canceling…"
            } else {
                "Cancel recording"
            }
        } else if self.selected_profile() {
            "Forget voice"
        } else {
            "Record voice"
        };
        let isolation = if stopping {
            if self.restart.is_some() {
                "Applying changes…"
            } else {
                "Stopping…"
            }
        } else if self.session.is_none() {
            "Start isolation"
        } else if changed {
            "Apply changes"
        } else {
            "Stop isolation"
        };
        let record_enabled = self.session.is_none()
            && !self.downloading()
            && !self.choices.microphone.is_empty()
            && !self.recording.as_ref().is_some_and(|r| r.canceled);
        let isolate_enabled = !stopping && (self.session.is_some() && !changed || self.can_start());
        ui.columns(2, |columns| {
            let width = columns[0].available_width();
            if columns[0]
                .add_enabled(
                    record_enabled,
                    egui::Button::new(RichText::new(voice).strong()).min_size(vec2(width, 36.0)),
                )
                .clicked()
            {
                let result = self.voice_action();
                self.report(result);
            }
            let width = columns[1].available_width();
            if columns[1]
                .add_enabled(
                    isolate_enabled,
                    egui::Button::new(RichText::new(isolation).strong())
                        .min_size(vec2(width, 36.0)),
                )
                .clicked()
            {
                self.isolation_action();
            }
        });
        if let Some(recording) = &self.recording {
            ui.add(
                egui::ProgressBar::new((recording.since.elapsed().as_secs_f32() / 6.0).min(1.0))
                    .text("Speak alone at your normal distance"),
            );
        } else if changed && !stopping {
            ui.weak("Processing changes are ready to apply.");
        }
    }

    fn latency(&mut self, ui: &mut egui::Ui, status: &Status) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong("MIC TO VIRTUAL CABLE");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let active = self.measurement_start.is_some();
                    let enabled = self
                        .session
                        .as_ref()
                        .is_some_and(|session| !session.stopping())
                        && (active || status.live);
                    if ui
                        .add_enabled(
                            enabled,
                            egui::Button::new(if active { "Cancel" } else { "Measure latency" }),
                        )
                        .clicked()
                    {
                        let session = self.session.as_mut().unwrap();
                        let result = if active {
                            session.cancel_measurement()
                        } else {
                            session.measure()
                        };
                        if result.is_ok() && !active {
                            self.clear_latency();
                            self.measurement_start = Some(Instant::now());
                        }
                        self.report(result);
                    }
                });
            });
            if let Some(start) = self.measurement_start {
                ui.add(
                    egui::ProgressBar::new((start.elapsed().as_secs_f32() / 6.0).min(1.0))
                        .text("Speak alone in short phrases"),
                );
            } else if let Some(error) = &self.latency_error {
                ui.colored_label(ui.visuals().warn_fg_color, error);
            } else if let Some(result) = &self.latency {
                ui.label(format!(
                    "{}{:.1} ms median  ·  {:.1} ms p95  ·  {}/{} windows{}",
                    if result.approximate { "≈ " } else { "" },
                    result.median_ms,
                    result.p95_ms,
                    result.windows,
                    result.eligible_windows,
                    if result.gaps_skipped {
                        "  ·  gaps skipped"
                    } else {
                        ""
                    }
                ));
            } else {
                ui.weak("Measure on demand while isolation is live.");
            }
        });
    }
}

impl eframe::App for Desktop {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.hidden = true;
        }
        if self.instance.quit_requested() {
            self.quit();
        }
        if !self.quitting && self.instance.open_requested() {
            self.show(ctx);
        }
        while !self.quitting {
            match self.tray.next_action() {
                Some(Action::Open) => self.show(ctx),
                Some(Action::Updates) => {
                    self.show(ctx);
                    if self.update_available.is_none() {
                        self.check_updates(ctx);
                    }
                }
                Some(Action::Toggle) if self.session.is_some() => self.stop(),
                Some(Action::Toggle) => self.isolation_action(),
                Some(Action::Quit) => self.quit(),
                None => break,
            }
        }
        self.status = self.poll(ctx);
        self.poll_updates(ctx);
        if self.quitting && self.session.is_none() && self.recording.is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let state = if self.status.live && !self.session.as_ref().is_some_and(Session::stopping) {
            State::Isolating
        } else if self.notice.as_ref().is_some_and(|(error, _)| *error)
            || self.status.recoveries > 0
        {
            State::NeedsAttention
        } else {
            State::Standby
        };
        let can_toggle = !self.quitting
            && self
                .session
                .as_ref()
                .map_or_else(|| self.can_start(), |session| !session.stopping());
        self.tray.update(
            state,
            can_toggle,
            self.session.is_some(),
            self.update_available.is_some(),
        );
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.hidden {
            // eframe's first paint can show the root viewport regardless of its initial visibility.
            root.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Visible(false));
            return;
        }
        let status = self.status.clone();
        egui::CentralPanel::default().show(root, |ui| {
            self.toolbar(ui);
            ui.separator();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new("VoiceIsolator").size(30.0).strong());
                            ui.weak("Local, real-time speaker isolation");
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let (label, live) =
                                if self.session.as_ref().is_some_and(Session::stopping) {
                                    ("STOPPING", false)
                                } else if status.live {
                                    ("LIVE", true)
                                } else if self.session.is_some() {
                                    (
                                        if status.recoveries > 0 {
                                            "RECOVERING"
                                        } else {
                                            "STARTING"
                                        },
                                        false,
                                    )
                                } else if self.recording.is_some() {
                                    ("RECORDING", false)
                                } else if self.selected_profile() {
                                    ("VOICE READY", true)
                                } else {
                                    ("STANDBY", false)
                                };
                            let color = if live {
                                accent(self.dark)
                            } else {
                                ui.visuals().weak_text_color()
                            };
                            egui::Frame::new()
                                .inner_margin(egui::Margin::symmetric(12, 7))
                                .corner_radius(18)
                                .fill(color.gamma_multiply(0.14))
                                .stroke(Stroke::new(1.0, color.gamma_multiply(0.5)))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        let (dot, _) = ui.allocate_exact_size(
                                            vec2(8.0, 8.0),
                                            egui::Sense::hover(),
                                        );
                                        ui.painter().circle_filled(dot.center(), 3.5, color);
                                        ui.label(
                                            RichText::new(label).size(11.0).strong().color(color),
                                        );
                                    });
                                });
                        });
                    });
                    ui.add_space(8.0);
                    self.routing(ui);
                    ui.add_space(8.0);
                    self.controls(ui);
                    ui.add_space(8.0);
                    self.actions(ui);
                    ui.add_space(6.0);
                    card(ui, |ui| {
                        ui.columns(2, |columns| {
                            meter(
                                &mut columns[0],
                                "MIC LEVEL",
                                self.input_meter,
                                accent(self.dark),
                            );
                            meter(
                                &mut columns[1],
                                "ISOLATED LEVEL",
                                self.output_meter,
                                accent(self.dark),
                            );
                        });
                    });
                    ui.add_space(6.0);
                    self.latency(ui, &status);
                    if let Some(message) = self.update_message.clone() {
                        ui.horizontal_wrapped(|ui| {
                            ui.weak(message);
                            if let Some(UpdateTask::Download { cancel, .. }) = &self.update_task {
                                if ui.add_enabled(!cancel.load(Relaxed), egui::Button::new("Cancel download")).clicked() {
                                    cancel.store(true, Relaxed);
                                }
                            } else if self.update_available.is_some() && self.update_task.is_none() {
                                let available = self.session.is_none() && self.recording.is_none() && self.resume_until.is_none();
                                if ui.add_enabled(available, egui::Button::new("Install update"))
                                    .on_disabled_hover_text("Stop isolation and recording before installing an update.").clicked() {
                                    self.install_update(ui.ctx());
                                }
                            }
                        });
                    }
                    if status.model_ready
                        && let Some(session) = &self.session
                    {
                        ui.weak(format!(
                            "{}  ·  Queue peak {:.1} ms  ·  underruns {}  ·  restarts {}",
                            session.config.processing.backend.label(),
                            status.peak_frames as f32 * 1000.0 / RATE as f32,
                            status.underruns,
                            status.recoveries
                        ));
                        if let Some(p99) = status.batch_p99_ms {
                            let percentile = |p99: Option<usize>| match p99 {
                                Some(63) => "≥63".to_owned(),
                                Some(ms) => ms.to_string(),
                                None => "—".to_owned(),
                            };
                            let ms = |frames: usize| frames as f32 * 1000.0 / RATE as f32;
                            ui.weak(format!(
                                "Batch p99 {} ms  ·  misses {}  ·  input queue {:.1} ms  ·  output queue {:.1} ms",
                                percentile(Some(p99)), status.batch_deadline_misses, ms(status.input_pending_frames), ms(status.output_pending_frames)
                            )).on_hover_text(format!(
                                "Batch time runs from capture arrival until available output is queued. Input wait p99 {} ms. Inference p99 {} ms, peak {:.2} ms. Last model reset {:.2} ms. Queue sizes are shown in milliseconds of audio.",
                                percentile(status.queue_wait_p99_ms), percentile(status.inference_p99_ms),
                                status.inference_peak_us as f64 / 1000.0, status.last_reset_us as f64 / 1000.0
                            ));
                        }
                    }
                    if let Some((error, notice)) = &self.notice {
                        let color = if *error {
                            ui.visuals().error_fg_color
                        } else {
                            ui.visuals().text_color()
                        };
                        ui.colored_label(color, notice);
                    } else if self.resume_until.is_some() {
                        ui.weak("Waiting for the saved microphone and virtual cable…");
                    } else if !self.selected_profile()
                        && self.recording.is_none()
                        && self.session.is_none()
                    {
                        ui.weak("Select a microphone and record six seconds of your voice.");
                    }
                });
        });
        self.save_standby_edits();
    }
}

fn accent(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(104, 227, 196)
    } else {
        Color32::from_rgb(0, 115, 91)
    }
}

fn theme(ctx: &egui::Context, dark: bool) {
    let mut style = (*ctx.global_style()).clone();
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.panel_fill = if dark {
        Color32::from_rgb(15, 23, 36)
    } else {
        Color32::from_rgb(241, 246, 250)
    };
    visuals.window_fill = if dark {
        Color32::from_rgb(24, 37, 54)
    } else {
        Color32::WHITE
    };
    visuals.weak_text_color = Some(if dark {
        Color32::from_rgb(154, 173, 193)
    } else {
        Color32::from_rgb(81, 103, 119)
    });
    visuals.widgets.inactive.weak_bg_fill = if dark {
        Color32::from_rgb(34, 53, 72)
    } else {
        Color32::from_rgb(226, 237, 242)
    };
    visuals.widgets.hovered.weak_bg_fill = if dark {
        Color32::from_rgb(47, 77, 94)
    } else {
        Color32::from_rgb(203, 228, 224)
    };
    visuals.selection.bg_fill = if dark {
        Color32::from_rgb(22, 86, 77)
    } else {
        Color32::from_rgb(177, 222, 211)
    };
    style.visuals = visuals;
    style.spacing.item_spacing = vec2(9.0, 7.0);
    style.spacing.button_padding = vec2(11.0, 5.0);
    ctx.set_global_style(style);
}

fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .inner_margin(11)
        .corner_radius(10)
        .stroke(Stroke::new(
            1.0,
            ui.visuals().widgets.noninteractive.bg_stroke.color,
        ))
        .fill(ui.visuals().window_fill())
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            add(ui);
        });
}

fn device_picker(
    ui: &mut egui::Ui,
    label: &str,
    devices: &[wasapi::Device],
    selected: &mut String,
    enabled: bool,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.add_sized([100.0, 24.0], egui::Label::new(label));
        let name = devices
            .iter()
            .find(|device| device.id == *selected)
            .map(|device| device.name.as_str())
            .unwrap_or("Choose a device");
        ui.add_enabled_ui(enabled, |ui| {
            egui::ComboBox::from_id_salt(label)
                .width((ui.available_width() - 12.0).max(100.0))
                .truncate()
                .selected_text(name)
                .show_ui(ui, |ui| {
                    for device in devices {
                        if ui
                            .selectable_label(device.id == *selected, &device.name)
                            .clicked()
                        {
                            changed = *selected != device.id;
                            selected.clone_from(&device.id);
                        }
                    }
                });
        });
    });
    changed
}

fn knob(
    ui: &mut egui::Ui,
    name: &str,
    value: &mut i32,
    range: std::ops::RangeInclusive<i32>,
    display: impl Fn(i32) -> String,
) {
    let (min, max) = (*range.start(), *range.end());
    card(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.strong(name);
            let (rect, response) =
                ui.allocate_exact_size(vec2(62.0, 57.0), egui::Sense::click_and_drag());
            let cache = response.id.with("initial_value");
            if response.clicked() || response.drag_started() {
                response.request_focus();
            }
            if response.drag_started() {
                ui.data_mut(|d| d.insert_temp(cache, *value));
            }
            if response.dragged() {
                let start = ui.data(|d| d.get_temp::<i32>(cache)).unwrap_or(*value);
                let delta = response.drag_delta();
                *value = (start
                    + ((delta.x - delta.y) * (max - min) as f32 / 115.0).round() as i32)
                    .clamp(min, max);
            }
            let painter = ui.painter();
            let center = rect.center() + vec2(0.0, 1.0);
            let tint = accent(ui.visuals().dark_mode);
            let fraction = (*value - min) as f32 / (max - min) as f32;
            let angle = |t| std::f32::consts::PI * (0.75 + 1.5 * t);
            let point = |t, radius| center + Vec2::angled(angle(t)) * radius;
            let arc = |end: f32| {
                (0..=32)
                    .map(|i| point(end * i as f32 / 32.0, 25.0))
                    .collect()
            };
            painter.add(egui::Shape::line(
                arc(1.0),
                Stroke::new(3.8, ui.visuals().widgets.inactive.bg_fill),
            ));
            if fraction > 0.0 {
                painter.add(egui::Shape::line(arc(fraction), Stroke::new(3.8, tint)));
            }
            let fill = if response.hovered() || response.dragged() {
                ui.visuals().widgets.hovered.weak_bg_fill
            } else {
                ui.visuals().widgets.inactive.weak_bg_fill
            };
            painter.circle_filled(center, 18.0, fill);
            painter.line_segment(
                [point(fraction, 10.0), point(fraction, 15.0)],
                Stroke::new(2.6, tint),
            );
            if response.has_focus() {
                ui.input(|input| {
                    let up = input.key_pressed(egui::Key::ArrowUp)
                        || input.key_pressed(egui::Key::ArrowRight);
                    let down = input.key_pressed(egui::Key::ArrowDown)
                        || input.key_pressed(egui::Key::ArrowLeft);
                    *value = (*value + i32::from(up) - i32::from(down)).clamp(min, max);
                    if input.key_pressed(egui::Key::Home) {
                        *value = 0.clamp(min, max);
                    }
                });
            }
            if response.hovered() {
                ui.input_mut(|input| {
                    let scroll = input
                        .raw
                        .events
                        .iter()
                        .filter_map(|event| match event {
                            egui::Event::MouseWheel { delta, .. } => Some(delta.y),
                            _ => None,
                        })
                        .sum::<f32>();
                    if scroll != 0.0 {
                        *value = (*value + scroll.signum() as i32).clamp(min, max);
                        input.smooth_scroll_delta.y = 0.0;
                    }
                });
            }
            response.on_hover_text("Drag or scroll to adjust. Click the value to type.");
            let format = egui::widgets::ValueFormat::default()
                .custom_formatter(|value, _| display(value as i32))
                .custom_parser(|text| {
                    let number: i32 = text.split_whitespace().next()?.parse().ok()?;
                    range
                        .clone()
                        .find(|&value| {
                            display(value)
                                .split_whitespace()
                                .next()
                                .and_then(|x| x.parse::<i32>().ok())
                                == Some(number)
                        })
                        .map(f64::from)
                });
            ui.add(
                egui::DragValue::new(value)
                    .range(range.clone())
                    .speed(0.12)
                    .format(format),
            );
        });
    });
}

fn smooth(previous: f32, peak: f32, elapsed: f32) -> f32 {
    let target = if peak > 0.001 {
        ((peak.log10() * 20.0 + 54.0) / 54.0).clamp(0.0, 1.0)
    } else {
        0.0
    };
    if target >= previous {
        target
    } else {
        (previous - elapsed * 1.6).max(target)
    }
}

fn meter(ui: &mut egui::Ui, label: &str, level: f32, color: Color32) {
    ui.strong(label);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 13.0), egui::Sense::hover());
    let width = rect.width() / 22.0;
    for index in 0..22 {
        let min = rect.min + vec2(index as f32 * width, 0.0);
        let segment = egui::Rect::from_min_size(min, vec2((width - 2.0).max(1.0), rect.height()));
        let partial = (level * 22.0 - index as f32).clamp(0.0, 1.0);
        let empty = ui.visuals().widgets.inactive.weak_bg_fill;
        ui.painter()
            .rect_filled(segment, 2, empty.lerp_to_gamma(color, partial));
    }
}
