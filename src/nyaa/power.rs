//! Battery + tablet-mode policy (PLAN.md → Nyaa → "Power policy").
//!
//! Event driven, no polling: UPower's `OnBattery` (system bus) and KWin's `tabletMode`
//! (session bus) are watched through GDBus property-change signals. Torrent downloads are
//! paused while the laptop is on battery **and** folded into tablet mode.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::gio::{self, prelude::*, BusType, DBusProxy, DBusProxyFlags};

/// Pause only when on battery and in tablet mode; plugging in or leaving tablet mode resumes.
pub fn should_pause(on_battery: bool, tablet_mode: bool) -> bool {
    on_battery && tablet_mode
}

async fn proxy(bus: BusType, name: &str, path: &str, iface: &str) -> Option<DBusProxy> {
    DBusProxy::for_bus_future(bus, DBusProxyFlags::DO_NOT_AUTO_START, None, name, path, iface).await.ok()
}

fn bool_prop(p: &DBusProxy, name: &str) -> Option<bool> {
    p.cached_property(name).and_then(|v| v.get::<bool>())
}

/// Calls `on_change(paused)` once with the initial state and again whenever it flips.
/// Without UPower or KWin (another desktop) the missing input counts as `false`.
pub fn watch(on_change: impl Fn(bool) + 'static) {
    let on_change = Rc::new(on_change);
    gtk::glib::spawn_future_local(async move {
        let upower = proxy(BusType::System, "org.freedesktop.UPower", "/org/freedesktop/UPower", "org.freedesktop.UPower").await;
        let kwin = proxy(BusType::Session, "org.kde.KWin", "/org/kde/KWin", "org.kde.KWin.TabletModeManager").await;
        let last: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
        let update = {
            let (upower, kwin, last, on_change) = (upower.clone(), kwin.clone(), last.clone(), on_change.clone());
            Rc::new(move || {
                let battery = upower.as_ref().and_then(|p| bool_prop(p, "OnBattery")).unwrap_or(false);
                let tablet = kwin.as_ref().and_then(|p| bool_prop(p, "tabletMode")).unwrap_or(false);
                let paused = should_pause(battery, tablet);
                if last.replace(Some(paused)) != Some(paused) {
                    on_change(paused);
                }
            })
        };
        // GDBus signal handlers must be Send + Sync, so they only poke a channel; the policy
        // itself is evaluated on the main thread.
        let (tx, rx) = async_channel::unbounded::<()>();
        // Keep the proxies (and so the signal connections) alive for the whole run of the app.
        thread_local! { static KEEP: RefCell<Vec<DBusProxy>> = const { RefCell::new(Vec::new()) }; }
        for p in [upower, kwin].into_iter().flatten() {
            let tx = tx.clone();
            p.connect_g_properties_changed(move |_, _, _| {
                let _ = tx.try_send(());
            });
            KEEP.with(|k| k.borrow_mut().push(p));
        }
        update();
        while rx.recv().await.is_ok() {
            update();
        }
    });
    let _ = gio::ListStore::new::<gio::File>(); // (keeps the `gio` import used on every feature set)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_only_on_battery_and_in_tablet_mode() {
        assert!(should_pause(true, true));
        assert!(!should_pause(true, false), "laptop mode on battery keeps downloading");
        assert!(!should_pause(false, true), "plugged in resumes even in tablet mode");
        assert!(!should_pause(false, false));
    }
}
