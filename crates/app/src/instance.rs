//! Single GUI instance on the session bus, so the tray (and a second launch
//! from the app menu) can reach the window that is already open.

use eframe::egui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use zbus::fdo::RequestNameFlags;
use zbus::fdo::RequestNameReply;
use zbus::{interface, proxy};

pub const GUI_NAME: &str = "io.github.thinkpower.Gui";
pub const GUI_PATH: &str = "/io/github/thinkpower/Gui";

#[proxy(
    interface = "io.github.thinkpower.Gui",
    default_service = "io.github.thinkpower.Gui",
    default_path = "/io/github/thinkpower/Gui",
    gen_async = false,
    gen_blocking = true,
    blocking_name = "GuiProxy"
)]
pub trait Gui {
    fn activate(&self) -> zbus::Result<()>;
    fn toggle(&self) -> zbus::Result<bool>;
}

/// Handles the GUI shares with its D-Bus object.
#[derive(Clone, Default)]
pub struct Remote {
    pub ctx: Arc<Mutex<Option<egui::Context>>>,
    /// The window has unsaved edits; never close it from the tray.
    pub dirty: Arc<AtomicBool>,
}

impl Remote {
    fn focus(&self) {
        if let Some(ctx) = self.ctx.lock().unwrap().as_ref() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.request_repaint();
        }
    }
}

#[interface(name = "io.github.thinkpower.Gui")]
impl Remote {
    /// Bring the window to the front.
    fn activate(&self) {
        self.focus();
    }

    /// Tray click: close the window, unless it has unsaved edits (then focus
    /// it instead). Returns true if the window is closing.
    fn toggle(&self) -> bool {
        if self.dirty.load(Ordering::Relaxed) {
            self.focus();
            return false;
        }
        if let Some(ctx) = self.ctx.lock().unwrap().as_ref() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            ctx.request_repaint();
        }
        true
    }
}

pub enum Claim {
    /// We are the only GUI; keep the connection alive while the window is open.
    Primary(zbus::blocking::Connection),
    /// Another window is already open (and was asked to come forward).
    AlreadyRunning,
    /// No session bus; run without single-instance support.
    Unavailable,
}

pub fn claim(remote: Remote) -> Claim {
    let conn = match zbus::blocking::connection::Builder::session().and_then(|b| b.serve_at(GUI_PATH, remote)?.build()) {
        Ok(c) => c,
        Err(_) => return Claim::Unavailable,
    };
    match conn.request_name_with_flags(GUI_NAME, RequestNameFlags::DoNotQueue.into()) {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => Claim::Primary(conn),
        // zbus reports a taken name (with DoNotQueue) as Error::NameTaken.
        Ok(_) | Err(zbus::Error::NameTaken) => {
            if let Ok(p) = GuiProxy::new(&conn) {
                let _ = p.activate();
            }
            Claim::AlreadyRunning
        }
        Err(_) => Claim::Unavailable,
    }
}

/// From the tray: toggle an open window, or start a new one.
pub fn toggle_or_launch() {
    let running = zbus::blocking::Connection::session()
        .ok()
        .and_then(|c| GuiProxy::new(&c).ok().map(|p| p.toggle().is_ok()))
        .unwrap_or(false);
    if !running {
        launch();
    }
}

/// From the tray menu: focus an open window, or start a new one.
pub fn open() {
    let running = zbus::blocking::Connection::session()
        .ok()
        .and_then(|c| GuiProxy::new(&c).ok().map(|p| p.activate().is_ok()))
        .unwrap_or(false);
    if !running {
        launch();
    }
}

fn launch() {
    // After a reinstall our own binary may have been replaced; fall back to PATH.
    let exe = std::env::current_exe().ok().filter(|p| p.exists()).unwrap_or_else(|| "thinkpower".into());
    match std::process::Command::new(exe).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => eprintln!("cannot start the GUI: {e}"),
    }
}
