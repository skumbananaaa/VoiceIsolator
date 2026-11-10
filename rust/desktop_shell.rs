use anyhow::Result;
use crossbeam_channel::{Receiver, bounded};
use eframe::egui;
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, GetLastError, HANDLE,
            WAIT_OBJECT_0,
        },
        System::Threading::{
            CreateEventW, EVENT_MODIFY_STATE, OpenEventW, SetEvent, WaitForSingleObject,
        },
    },
    core::{PCWSTR, w},
};

const OPEN: PCWSTR = w!("Local\\VoiceIsolator-Open-v1");
const QUIT: PCWSTR = w!("Local\\VoiceIsolator-Quit-v1");

struct Signal(HANDLE);

impl Signal {
    fn pending(&self) -> bool {
        unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
    }
}

impl Drop for Signal {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub struct Instance {
    open: Signal,
    quit: Signal,
}

impl Instance {
    pub fn acquire(reveal_existing: bool) -> Result<Option<Self>> {
        let (open, existed) = unsafe {
            let handle = CreateEventW(None, false, false, OPEN)?;
            (Signal(handle), GetLastError() == ERROR_ALREADY_EXISTS)
        };
        if existed {
            if reveal_existing {
                unsafe { SetEvent(open.0)? };
            }
            return Ok(None);
        }
        let quit = Signal(unsafe { CreateEventW(None, false, false, QUIT)? });
        Ok(Some(Self { open, quit }))
    }

    pub fn request_quit() -> Result<()> {
        match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, QUIT) } {
            Ok(handle) => {
                let signal = Signal(handle);
                unsafe { SetEvent(signal.0)? };
                Ok(())
            }
            Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn open_requested(&self) -> bool {
        self.open.pending()
    }
    pub fn quit_requested(&self) -> bool {
        self.quit.pending()
    }
}

#[derive(Clone, Copy)]
pub enum Action {
    Open,
    Toggle,
    Updates,
    Quit,
}

#[derive(Clone, Copy, PartialEq)]
pub enum State {
    Standby,
    Isolating,
    NeedsAttention,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            Self::Standby => "Standby",
            Self::Isolating => "Isolating",
            Self::NeedsAttention => "Needs attention",
        }
    }

    fn icon(self) -> Icon {
        let color = match self {
            Self::Standby => [135, 157, 185, 255],
            Self::Isolating => [87, 220, 184, 255],
            Self::NeedsAttention => [255, 171, 94, 255],
        };
        let heights = [5, 12, 20, 12, 5];
        let mut pixels = vec![0; 32 * 32 * 4];
        for (bar, height) in heights.into_iter().enumerate() {
            for y in (32 - height) / 2..(32 + height) / 2 {
                for x in 4 + bar * 5..7 + bar * 5 {
                    let at = (y * 32 + x) * 4;
                    pixels[at..at + 4].copy_from_slice(&color);
                }
            }
        }
        Icon::from_rgba(pixels, 32, 32).expect("valid tray icon dimensions")
    }
}

pub struct Tray {
    icon: TrayIcon,
    toggle: MenuItem,
    updates: MenuItem,
    actions: Receiver<Action>,
    state: State,
    can_toggle: bool,
    running: bool,
    update_available: bool,
}

impl Tray {
    pub fn new(ctx: &egui::Context, installed: bool) -> Result<Self> {
        let open = MenuItem::new("Open", true, None);
        let toggle = MenuItem::new("Start isolation", false, None);
        let updates = MenuItem::new("Check for updates", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        menu.append_items(&[&open, &toggle])?;
        if installed {
            menu.append(&updates)?;
        }
        menu.append_items(&[&PredefinedMenuItem::separator(), &quit])?;
        let state = State::Standby;
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_icon(state.icon())
            .with_tooltip("VoiceIsolator · Standby")
            .build()?;
        let (send, actions) = bounded(16);
        let menu_sender = send.clone();
        let menu_ctx = ctx.clone();
        let ids = [
            (open.id().clone(), Action::Open),
            (toggle.id().clone(), Action::Toggle),
            (updates.id().clone(), Action::Updates),
            (quit.id().clone(), Action::Quit),
        ];
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if let Some((_, action)) = ids.iter().find(|(id, _)| *id == event.id) {
                let _ = menu_sender.try_send(*action);
                menu_ctx.request_repaint();
            }
        }));
        let ctx = ctx.clone();
        TrayIconEvent::set_event_handler(Some(move |event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                let _ = send.try_send(Action::Open);
                ctx.request_repaint();
            }
        }));
        Ok(Self {
            icon,
            toggle,
            updates,
            actions,
            state,
            can_toggle: false,
            running: false,
            update_available: false,
        })
    }

    pub fn next_action(&self) -> Option<Action> {
        self.actions.try_recv().ok()
    }

    pub fn update(
        &mut self,
        state: State,
        can_toggle: bool,
        running: bool,
        update_available: bool,
    ) {
        if state != self.state {
            self.state = state;
            let _ = self.icon.set_icon(Some(state.icon()));
            let _ = self
                .icon
                .set_tooltip(Some(format!("VoiceIsolator · {}", state.label())));
        }
        if can_toggle != self.can_toggle {
            self.can_toggle = can_toggle;
            self.toggle.set_enabled(can_toggle);
        }
        if running != self.running {
            self.running = running;
            self.toggle.set_text(if running {
                "Stop isolation"
            } else {
                "Start isolation"
            });
        }
        if update_available != self.update_available {
            self.update_available = update_available;
            self.updates.set_text(if update_available {
                "Update available…"
            } else {
                "Check for updates"
            });
        }
    }
}
