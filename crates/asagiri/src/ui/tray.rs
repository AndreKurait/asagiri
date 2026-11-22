//! The Linux tray (issue #405): a StatusNotifierItem over D-Bus, through
//! ksni, the same shape as the Windows tray and the Mac menu bar item: the
//! engine's state, the running containers, Start/Stop, Quit. Windows and
//! macOS already have a tray, so this is Linux-only; the `window` feature
//! owns it, and the engine and CLI stay tray-free.
//!
//! The tray lives on its own thread (ksni's blocking API spawns one): its
//! callbacks never touch GTK directly, they send `Cmd` down the engine
//! thread's channel or ask GTK's main thread to show the window. The menu
//! is built fresh from `menu_state()` each time the host opens it, as the
//! Mac's `fill_menu` does.
//!
//! No tray host (no `org.kde.StatusNotifierWatcher` on the session bus, or
//! no host registered) means no tray: `spawn` fails, `active()` stays
//! false, and closing the window quits as it always has. The watcher
//! going away later clears `active()` the same way, through
//! `watcher_offline`, so a close then quits instead of hiding the window
//! behind a tray that is gone.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{MenuItem, StandardItem};
use ksni::{Category, Icon, Status, Tray};

// `PixbufLoaderExt` (write/close/pixbuf): gtk.rs gets it from
// `gtk::prelude`, this file takes it directly. The `Pixbuf` methods used
// below (scale, rows) are inherent, not trait methods.
use gtk::gdk_pixbuf::prelude::PixbufLoaderExt;

use super::{Cmd, emit, json_str, menu_state, prefs, trf};
use crate::diag;

/// The tray service, while it runs; and whether it is showing, which the
/// window's close handler reads. Both change on the tray thread and on
/// GTK's main thread, never at once.
static HANDLE: Mutex<Option<Handle<AsagiriTray>>> = Mutex::new(None);
static UP: AtomicBool = AtomicBool::new(false);

/// Whether the tray is up: only then does closing the window hide it.
pub(super) fn active() -> bool {
    UP.load(Ordering::SeqCst)
}

/// Starts the tray when `on` and the session bus has a watcher for it, or
/// stops it when `off`. `tx` is the engine thread's channel, kept for the
/// menu's Start/Stop/Quit; the icon comes from the app icon PNG, scaled
/// here so the host gets pixels, not a theme name that may not be installed.
pub(super) fn set_active(on: bool, tx: &Sender<Cmd>, icon_png: &[u8]) {
    let mut handle = HANDLE.lock().unwrap_or_else(|e| e.into_inner());
    if !on {
        if let Some(h) = handle.take() {
            UP.store(false, Ordering::SeqCst);
            h.shutdown().wait();
        }
        return;
    }
    if handle.is_some() {
        return;
    }
    let tray = AsagiriTray { tx: tx.clone(), icon: pixmap(icon_png) };
    match tray.spawn() {
        Ok(h) => {
            *handle = Some(h);
            UP.store(true, Ordering::SeqCst);
        }
        Err(e) => diag::warn!("tray not started", error = e.to_string()),
    }
}

/// The app icon as the tray's pixels: 22 px, the size hosts draw, and 32 px
/// for the ones that ask bigger. ARGB32, as StatusNotifierItem wants it.
fn pixmap(png: &[u8]) -> Vec<Icon> {
    let loader = gtk::gdk_pixbuf::PixbufLoader::new();
    if loader.write(png).is_err() || loader.close().is_err() {
        return Vec::new();
    }
    let Some(full) = loader.pixbuf() else { return Vec::new() };
    [22, 32]
        .iter()
        .filter_map(|&size| {
            let scaled = full.scale_simple(size, size, gtk::gdk_pixbuf::InterpType::Bilinear)?;
            let bytes = scaled.read_pixel_bytes();
            let (w, h) = (scaled.width() as usize, scaled.height() as usize);
            let (stride, alpha) = (scaled.rowstride() as usize, scaled.has_alpha());
            let mut data = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                for x in 0..w {
                    // Rows can be padded past the pixels, and a pixbuf
                    // without alpha is three bytes a pixel: read by row.
                    let i = y * stride + x * if alpha { 4 } else { 3 };
                    let (r, g, b, a) = if alpha { (bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]) } else { (bytes[i], bytes[i + 1], bytes[i + 2], 0xFF) };
                    // Pixbuf is RGBA; the tray wants ARGB, alpha first.
                    data.extend_from_slice(&[a, r, g, b]);
                }
            }
            Some(Icon { width: size, height: size, data })
        })
        .collect()
}

/// Shows the window again, from the tray thread: GTK's main thread does it.
fn show_window() {
    super::window::show();
}

/// A tray menu item: a label, and what it does with the tray's channel.
fn item(label: String, enabled: bool, act: impl Fn(&mut AsagiriTray) + Send + 'static) -> MenuItem<AsagiriTray> {
    MenuItem::Standard(StandardItem { label, enabled, activate: Box::new(act), ..Default::default() })
}

struct AsagiriTray {
    tx: Sender<Cmd>,
    icon: Vec<Icon>,
}

impl Tray for AsagiriTray {
    fn id(&self) -> String {
        "asagiri".into()
    }

    fn title(&self) -> String {
        "Asagiri".into()
    }

    fn category(&self) -> Category {
        Category::ApplicationStatus
    }

    fn status(&self) -> Status {
        Status::Active
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.icon.clone()
    }

    /// Left-click reopens the window, as on Windows.
    fn activate(&mut self, _x: i32, _y: i32) {
        show_window();
    }

    /// The watcher is gone: the icon is gone with it, so a close must quit
    /// again. The service keeps running and comes back with the watcher.
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        UP.store(false, Ordering::SeqCst);
        true
    }

    fn watcher_online(&self) {
        UP.store(true, Ordering::SeqCst);
    }

    /// Built fresh on each open, as the Mac's menu is: the engine's state,
    /// the running containers (up to 12, as there), Start/Stop, Quit.
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let state = menu_state();
        let lang = super::lang::resolve(prefs()["lang"].as_str());
        let tr = |en: &str| super::lang::tr(lang, en);
        let mut items = vec![item(
            trf("Asagiri: {state}", &[("state", &tr(state.vm))]),
            false,
            |_| {},
        )];
        items.push(MenuItem::Separator);
        if state.running.is_empty() {
            items.push(item(tr("No containers running"), false, |_| {}));
        }
        for (id, name, url) in state.running.iter().take(12) {
            let title = match url {
                Some(u) => format!("{name}  ·  {}", u.trim_start_matches("http://")),
                None => name.clone(),
            };
            let id = id.clone();
            // As the Windows tray does: show the window on the container's page.
            items.push(item(title, true, move |_| {
                show_window();
                let mut ev = String::from(r#"{"type":"detail","id":"#);
                json_str(&mut ev, &id);
                ev.push('}');
                emit(ev);
            }));
        }
        if state.running.len() > 12 {
            items.push(item(trf("and {n} more…", &[("n", &(state.running.len() - 12).to_string())]), true, |_| show_window()));
        }
        items.push(MenuItem::Separator);
        items.push(item(tr("Open Asagiri"), true, |_| show_window()));
        items.push(item(tr("Start"), true, |t| {
            let _ = t.tx.send(Cmd::Start);
        }));
        items.push(item(tr("Stop"), true, |t| {
            let _ = t.tx.send(Cmd::Stop);
        }));
        items.push(MenuItem::Separator);
        items.push(item(tr("Quit Asagiri"), true, |t| {
            super::window::quit_now(&t.tx);
        }));
        items
    }
}
