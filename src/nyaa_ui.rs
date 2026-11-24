//! The "Nyaa" place: downloaded series, recent releases, linked series and settings, plus the
//! "Link to Nyaa…" window. Retro theme, touch-sized controls. Blocking work goes through
//! [`crate::app::background`]; nothing here touches the disk or the network on the main thread.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gtk::prelude::*;

use crate::app::background;
use crate::browse::BrowseView;
use crate::covers::Covers;
use crate::db::Db;
use crate::nyaa::service::Service;
use crate::nyaa::store::{self, Link, Release};
use crate::nyaa::sync::{Report, Settings};
use crate::nyaa::{qbit, secret, DEFAULT_UPLOADERS};
use crate::source::Source;

fn human(bytes: i64) -> String {
    let b = bytes.max(0) as f64;
    if b >= 1024.0_f64.powi(3) { format!("{:.1} GiB", b / 1024.0_f64.powi(3)) } else { format!("{:.0} MiB", b / 1024.0 / 1024.0) }
}

fn clear(list: &gtk::ListBox) {
    while let Some(c) = list.first_child() {
        list.remove(&c);
    }
}

fn label(text: &str, class: &str) -> gtk::Label {
    gtk::Label::builder().label(text).xalign(0.0).wrap(true).css_classes([class]).build()
}

fn button(text: &str) -> gtk::Button {
    gtk::Button::builder().label(text).css_classes(["retro", "retro-small"]).valign(gtk::Align::Center).build()
}

fn state_text(state: &str) -> &'static str {
    match state {
        store::SEEN => "not linked",
        store::SKIPPED => "skipped",
        store::UNPARSED => "not understood",
        store::MATCHED => "waiting",
        store::CONFIRM => "needs confirmation",
        store::DOWNLOADING => "downloading",
        store::DONE => "downloaded",
        store::FAILED => "failed",
        _ => "?",
    }
}

pub struct NyaaPage {
    pub root: gtk::Box,
    pub series: Rc<BrowseView>,
    nyaa_source: Arc<dyn Source>,
    db: Arc<Db>,
    service: Arc<Service>,
    banner: gtk::Label,
    banner_box: gtk::Box,
    tabs: gtk::Stack,
    tab_buttons: Vec<(gtk::ToggleButton, &'static str)>,
    tab: RefCell<String>,
    releases: gtk::ListBox,
    show_skipped: gtk::CheckButton,
    links: gtk::ListBox,
    uploaders: gtk::ListBox,
    new_uploader: gtk::Entry,
    port: gtk::Entry,
    user: gtk::Entry,
    password: gtk::PasswordEntry,
    interval: gtk::Entry,
    cap_gb: gtk::Entry,
    settings_msg: gtk::Label,
    /// Minutes between cycles, shared with the app's timer.
    pub interval_min: Rc<Cell<u32>>,
}

impl NyaaPage {
    pub fn new(covers: Rc<Covers>, sources: Vec<Arc<dyn Source>>, db: Arc<Db>, service: Arc<Service>, interval_min: Rc<Cell<u32>>) -> Rc<Self> {
        let nyaa_source = sources.iter().find(|s| s.id() == "nyaa").cloned().expect("the nyaa source exists");
        let series = BrowseView::new(covers, sources);

        // ---- banner (qBittorrent status, last check) ----------------------------------------
        let banner = label("", "retro-banner");
        banner.set_hexpand(true);
        let retry = button("Check now");
        let start = button("Start qBittorrent");
        let banner_box = gtk::Box::builder().spacing(8).css_classes(["retro-banner-box"]).visible(false).build();
        banner_box.append(&banner);
        banner_box.append(&start);
        banner_box.append(&retry);

        // ---- tab buttons ----------------------------------------------------------------------
        let tabs = gtk::Stack::builder().vexpand(true).hexpand(true).build();
        let tab_bar = gtk::Box::builder().spacing(8).css_classes(["retro-toolbar"]).build();
        let mut first: Option<gtk::ToggleButton> = None;
        let mut tab_buttons = Vec::new();
        for (name, title) in [("series", "Series"), ("releases", "Releases"), ("links", "Links"), ("settings", "Settings")] {
            let b = gtk::ToggleButton::builder().label(title).css_classes(["retro"]).build();
            if let Some(f) = &first {
                b.set_group(Some(f));
            } else {
                b.set_active(true);
                first = Some(b.clone());
            }
            tab_bar.append(&b);
            tab_buttons.push((b, name));
        }

        // ---- releases tab ---------------------------------------------------------------------
        let releases = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["nyaa-list"]).build();
        let show_skipped = gtk::CheckButton::builder().label("Show skipped").build();
        let check_now = button("Check now");
        let rel_bar = gtk::Box::builder().spacing(8).css_classes(["retro-toolbar"]).build();
        rel_bar.append(&check_now);
        rel_bar.append(&show_skipped);
        let rel_scroll = gtk::ScrolledWindow::builder().child(&releases).vexpand(true).css_classes(["retro-well"]).build();
        let rel_page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
        rel_page.append(&rel_bar);
        rel_page.append(&rel_scroll);

        // ---- links tab ------------------------------------------------------------------------
        let links = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["nyaa-list"]).build();
        let hint = label("Link a series from its page (\"Link to Nyaa…\"): choose an uploader and a search phrase once, and new volumes are found automatically.", "retro-text");
        let links_page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
        links_page.append(&hint);
        links_page.append(&gtk::ScrolledWindow::builder().child(&links).vexpand(true).css_classes(["retro-well"]).build());

        // ---- settings tab ---------------------------------------------------------------------
        let uploaders = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["nyaa-list"]).build();
        let new_uploader = gtk::Entry::builder().placeholder_text("Uploader name").hexpand(true).css_classes(["retro"]).build();
        let add_uploader = button("Add");
        let add_row = gtk::Box::builder().spacing(8).build();
        add_row.append(&new_uploader);
        add_row.append(&add_uploader);
        let entry = |ph: &str, w: i32| gtk::Entry::builder().placeholder_text(ph).width_chars(w).css_classes(["retro"]).build();
        let port = entry("8080", 6);
        let user = entry("(none: localhost bypass)", 22);
        let password = gtk::PasswordEntry::builder().placeholder_text("password (kept in the keyring)").show_peek_icon(true).hexpand(true).css_classes(["retro"]).build();
        let interval = entry("60", 6);
        let cap_gb = entry("4", 6);
        let settings_msg = label("", "retro-text");
        let save = button("Save");
        let save_pw = button("Save password");
        let forget_pw = button("Forget password");
        let test = button("Test connection");
        let launch = button("Start qBittorrent");
        let form_row = |title: &str, w: &gtk::Widget| {
            let r = gtk::Box::builder().spacing(8).build();
            let l = gtk::Label::builder().label(title).xalign(0.0).width_chars(24).build();
            r.append(&l);
            r.append(w);
            r
        };
        let settings_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).css_classes(["retro-body"]).build();
        settings_box.append(&label("Followed uploaders (Literature - English-translated)", "retro-heading"));
        settings_box.append(&uploaders);
        settings_box.append(&add_row);
        settings_box.append(&label("qBittorrent (Web UI on this computer)", "retro-heading"));
        settings_box.append(&form_row("Port", port.upcast_ref()));
        settings_box.append(&form_row("User name (optional)", user.upcast_ref()));
        settings_box.append(&form_row("Password", password.upcast_ref()));
        let pw_row = gtk::Box::builder().spacing(8).build();
        pw_row.append(&save_pw);
        pw_row.append(&forget_pw);
        pw_row.append(&test);
        pw_row.append(&launch);
        settings_box.append(&pw_row);
        settings_box.append(&label("Checking", "retro-heading"));
        settings_box.append(&form_row("Check every (minutes, min 15)", interval.upcast_ref()));
        settings_box.append(&form_row("Ask before more than (GiB)", cap_gb.upcast_ref()));
        settings_box.append(&save);
        settings_box.append(&settings_msg);
        let settings_scroll = gtk::ScrolledWindow::builder().child(&settings_box).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).css_classes(["retro-well"]).build();

        tabs.add_named(&series.root, Some("series"));
        tabs.add_named(&rel_page, Some("releases"));
        tabs.add_named(&links_page, Some("links"));
        tabs.add_named(&settings_scroll, Some("settings"));

        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
        root.append(&tab_bar);
        root.append(&banner_box);
        root.append(&tabs);

        let this = Rc::new(Self {
            root,
            series,
            nyaa_source,
            db,
            service,
            banner,
            banner_box,
            tabs,
            tab_buttons: tab_buttons.clone(),
            tab: RefCell::new("series".into()),
            releases,
            show_skipped,
            links,
            uploaders,
            new_uploader,
            port,
            user,
            password,
            interval,
            cap_gb,
            settings_msg,
            interval_min,
        });

        for (b, name) in tab_buttons {
            let w = Rc::downgrade(&this);
            b.connect_toggled(move |b| {
                if let (true, Some(t)) = (b.is_active(), w.upgrade()) {
                    t.tabs.set_visible_child_name(name);
                    *t.tab.borrow_mut() = name.into();
                    t.refresh();
                }
            });
        }
        let weak = Rc::downgrade(&this);
        let on = |f: fn(&Rc<NyaaPage>)| {
            let w = weak.clone();
            move |_: &gtk::Button| {
                if let Some(t) = w.upgrade() {
                    f(&t)
                }
            }
        };
        retry.connect_clicked(on(|t| t.service.run_cycle(true)));
        check_now.connect_clicked(on(|t| t.service.run_cycle(true)));
        start.connect_clicked(on(|t| t.start_qbittorrent()));
        launch.connect_clicked(on(|t| t.start_qbittorrent()));
        test.connect_clicked(on(|t| {
            t.settings_msg.set_text("Testing…");
            t.service.test_connection();
        }));
        save.connect_clicked(on(|t| t.save_settings()));
        save_pw.connect_clicked(on(|t| t.save_password()));
        forget_pw.connect_clicked(on(|t| t.forget_password()));
        add_uploader.connect_clicked(on(|t| {
            let name = t.new_uploader.text().trim().to_string();
            if !name.is_empty() {
                t.db.nyaa_add_uploader(&name);
                t.new_uploader.set_text("");
                t.refresh_uploaders();
            }
        }));
        this.show_skipped.connect_toggled({
            let w = Rc::downgrade(&this);
            move |_| {
                if let Some(t) = w.upgrade() {
                    t.refresh_releases()
                }
            }
        });
        this
    }

    /// Called when the place is opened.
    pub fn show(self: &Rc<Self>) {
        self.series.show_source(self.nyaa_source.clone());
        self.refresh();
    }

    /// Switches to a tab by name (also used by the `MANGA_VIEW=nyaa:<tab>` test hook).
    pub fn select_tab(self: &Rc<Self>, name: &str) {
        if let Some((b, _)) = self.tab_buttons.iter().find(|(_, n)| *n == name) {
            b.set_active(true);
        }
    }

    /// Reloads the visible tab from the database.
    pub fn refresh(self: &Rc<Self>) {
        match self.tab.borrow().as_str() {
            "releases" => self.refresh_releases(),
            "links" => self.refresh_links(),
            "settings" => {
                self.refresh_uploaders();
                self.load_settings();
            }
            _ => {}
        }
    }

    // ---- banner ----------------------------------------------------------------------------------

    pub fn show_report(&self, r: &Report) {
        match &r.qbit_error {
            Some(e) => self.set_banner(e, true),
            None if !r.errors.is_empty() => self.set_banner(&r.errors.join("; "), true),
            None => {
                self.banner_box.set_visible(false);
            }
        }
    }

    pub fn set_banner(&self, text: &str, error: bool) {
        self.banner.set_text(text);
        if error {
            self.banner.add_css_class("retro-error");
        } else {
            self.banner.remove_css_class("retro-error");
        }
        self.banner_box.set_visible(true);
    }

    pub fn show_connection_result(&self, r: &Result<String, String>) {
        match r {
            Ok(v) => self.settings_msg.set_text(&format!("Connected to qBittorrent {v}.")),
            Err(e) => self.settings_msg.set_text(e),
        }
    }

    fn start_qbittorrent(&self) {
        // Only when the user presses the button; qBittorrent keeps running after this app closes.
        match std::process::Command::new("qbittorrent").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
            Ok(_) => self.set_banner("Starting qBittorrent… then press \"Check now\".", false),
            Err(e) => self.set_banner(&format!("Cannot start qBittorrent: {e}. Is it installed?"), true),
        }
    }

    // ---- releases --------------------------------------------------------------------------------

    fn refresh_releases(self: &Rc<Self>) {
        let (db, this, all) = (self.db.clone(), self.clone(), self.show_skipped.is_active());
        background(move || db.nyaa_recent_releases(300), move |rels: Vec<Release>| {
            clear(&this.releases);
            let shown: Vec<&Release> = rels.iter().filter(|r| all || (r.state != store::SKIPPED && r.state != store::UNPARSED)).collect();
            if shown.is_empty() {
                this.releases.append(&label("Nothing here yet. Press \"Check now\".", "retro-text"));
            }
            for r in shown {
                this.releases.append(&this.release_row(r));
            }
        });
    }

    fn release_row(self: &Rc<Self>, r: &Release) -> gtk::Box {
        let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).build();
        let title = gtk::Label::builder().label(&r.title).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let mut meta = format!("{} · {} · {}", r.uploader, human(r.size), state_text(&r.state));
        if !r.note.is_empty() {
            meta.push_str(&format!(" — {}", r.note));
        }
        text.append(&title);
        text.append(&gtk::Label::builder().label(&meta).xalign(0.0).wrap(true).css_classes(["chapter-note"]).build());
        let row = gtk::Box::builder().spacing(8).css_classes(["chapter-row"]).build();
        row.append(&text);
        let actionable = r.link_id.is_some() && [store::MATCHED, store::CONFIRM, store::FAILED].contains(&r.state.as_str());
        if actionable {
            let b = button("Download");
            let (rel, w) = (r.clone(), Rc::downgrade(self));
            b.connect_clicked(move |b| {
                b.set_sensitive(false);
                b.set_label("Adding…");
                if let Some(t) = w.upgrade() {
                    t.service.queue(rel.clone());
                }
            });
            row.append(&b);
        }
        row
    }

    // ---- links -----------------------------------------------------------------------------------

    fn refresh_links(self: &Rc<Self>) {
        let (db, this) = (self.db.clone(), self.clone());
        background(
            move || db.nyaa_links().into_iter().map(|l| { let n = db.nyaa_owned_volumes(l.id).len(); (l, n) }).collect::<Vec<_>>(),
            move |links: Vec<(Link, usize)>| {
                clear(&this.links);
                if links.is_empty() {
                    this.links.append(&label("No linked series yet.", "retro-text"));
                }
                for (l, owned) in links {
                    this.links.append(&this.link_row(&l, owned));
                }
            },
        );
    }

    fn link_row(self: &Rc<Self>, l: &Link, owned: usize) -> gtk::Box {
        let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).build();
        text.append(&gtk::Label::builder().label(&l.title).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build());
        text.append(&gtk::Label::builder().label(format!("{} · \"{}\" · {owned} volumes on disk", l.uploader, l.query)).xalign(0.0).css_classes(["chapter-note"]).build());
        let auto = gtk::ToggleButton::builder().label("Auto").css_classes(["retro", "retro-small"]).active(l.auto).valign(gtk::Align::Center).tooltip_text("Download new volumes without asking").build();
        let (db, id) = (self.db.clone(), l.id);
        auto.connect_toggled(move |b| db.nyaa_set_link_auto(id, b.is_active()));
        let check = button("Check now");
        let unlink = button("Unlink");
        let w = Rc::downgrade(self);
        check.connect_clicked({
            let w = w.clone();
            move |_| {
                if let Some(t) = w.upgrade() {
                    t.service.run_cycle(true)
                }
            }
        });
        let db = self.db.clone();
        unlink.connect_clicked(move |_| {
            db.nyaa_remove_link(id);
            if let Some(t) = w.upgrade() {
                t.refresh_links();
            }
        });
        let row = gtk::Box::builder().spacing(8).css_classes(["chapter-row"]).build();
        row.append(&text);
        row.append(&auto);
        row.append(&check);
        row.append(&unlink);
        row
    }

    // ---- settings --------------------------------------------------------------------------------

    fn refresh_uploaders(self: &Rc<Self>) {
        let (db, this) = (self.db.clone(), self.clone());
        background(
            move || {
                db.nyaa_seed_uploaders(&DEFAULT_UPLOADERS);
                db.nyaa_uploaders()
            },
            move |ups| {
                clear(&this.uploaders);
                for u in ups {
                    let row = gtk::Box::builder().spacing(8).css_classes(["chapter-row"]).build();
                    row.append(&gtk::Label::builder().label(&u.name).xalign(0.0).hexpand(true).build());
                    let sw = gtk::Switch::builder().active(u.enabled).valign(gtk::Align::Center).build();
                    let (db, name) = (this.db.clone(), u.name.clone());
                    sw.connect_state_set(move |_, on| {
                        db.nyaa_set_uploader_enabled(&name, on);
                        gtk::glib::Propagation::Proceed
                    });
                    let remove = button("Remove");
                    let (w, db, name) = (Rc::downgrade(&this), this.db.clone(), u.name.clone());
                    remove.connect_clicked(move |_| {
                        db.nyaa_remove_uploader(&name);
                        if let Some(t) = w.upgrade() {
                            t.refresh_uploaders();
                        }
                    });
                    row.append(&sw);
                    row.append(&remove);
                    this.uploaders.append(&row);
                }
            },
        );
    }

    fn load_settings(self: &Rc<Self>) {
        let (db, this) = (self.db.clone(), self.clone());
        background(move || Settings::load(&db), move |s: Settings| {
            this.port.set_text(&s.qbit_port.to_string());
            this.user.set_text(s.qbit_user.as_deref().unwrap_or(""));
            this.interval.set_text(&s.interval_min.to_string());
            this.cap_gb.set_text(&format!("{}", s.size_cap / (1024 * 1024 * 1024)));
            this.interval_min.set(s.interval_min);
        });
    }

    fn save_settings(self: &Rc<Self>) {
        let port: u16 = self.port.text().trim().parse().unwrap_or(qbit::DEFAULT_PORT);
        let minutes = self.interval.text().trim().parse::<u32>().unwrap_or(60).max(15);
        let cap: u64 = self.cap_gb.text().trim().parse().unwrap_or(4).max(1);
        self.db.set_setting("qbit_port", &port.to_string());
        self.db.set_setting("qbit_user", self.user.text().trim());
        self.db.set_setting("nyaa_interval_min", &minutes.to_string());
        self.db.set_setting("nyaa_size_cap_mb", &(cap * 1024).to_string());
        self.interval_min.set(minutes);
        self.interval.set_text(&minutes.to_string());
        self.settings_msg.set_text("Saved.");
    }

    fn save_password(self: &Rc<Self>) {
        let (user, pw) = (self.user.text().trim().to_string(), self.password.text().to_string());
        if user.is_empty() || pw.is_empty() {
            self.settings_msg.set_text("Enter a user name and a password first.");
            return;
        }
        self.password.set_text(""); // never keep it in the widget
        self.db.set_setting("qbit_user", &user);
        let this = self.clone();
        background(move || secret::store(&user, &pw), move |r| {
            this.settings_msg.set_text(&match r {
                Ok(()) => "Password stored in the keyring.".to_string(),
                Err(e) => e,
            });
        });
    }

    fn forget_password(self: &Rc<Self>) {
        let user = self.user.text().trim().to_string();
        let this = self.clone();
        background(move || secret::clear(&user), move |_| this.settings_msg.set_text("Password removed from the keyring."));
    }
}

/// "Link to Nyaa…": uploader + search phrase, once per series.
pub fn show_link_dialog(parent: &gtk::Window, uploaders: Vec<String>, phrase: &str, on_confirm: impl Fn(String, String, bool) + 'static) {
    let win = gtk::Window::builder().transient_for(parent).modal(true).title("Link to Nyaa").default_width(520).css_classes(["retro-panel"]).build();
    let title = gtk::Label::builder().label("Link to Nyaa").css_classes(["retro-title"]).hexpand(true).build();
    let titlebar = gtk::Box::builder().css_classes(["retro-titlebar"]).build();
    titlebar.append(&title);
    let names: Vec<&str> = uploaders.iter().map(String::as_str).collect();
    let dropdown = gtk::DropDown::from_strings(&names);
    let entry = gtk::Entry::builder().text(phrase).css_classes(["retro"]).hexpand(true).build();
    let auto = gtk::CheckButton::builder().label("Download new volumes automatically").active(true).build();
    let ok = gtk::Button::builder().label("Link").css_classes(["retro"]).build();
    let cancel = gtk::Button::builder().label("Cancel").css_classes(["retro"]).build();
    let buttons = gtk::Box::builder().spacing(10).homogeneous(true).halign(gtk::Align::End).build();
    buttons.append(&cancel);
    buttons.append(&ok);
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).css_classes(["retro-body"]).build();
    body.append(&label("Uploader", "retro-text"));
    body.append(&dropdown);
    body.append(&label("Search phrase (words a release title must contain)", "retro-text"));
    body.append(&entry);
    body.append(&auto);
    body.append(&buttons);
    let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    root.append(&titlebar);
    root.append(&body);
    win.set_child(Some(&root));
    cancel.connect_clicked({
        let w = win.clone();
        move |_| w.close()
    });
    ok.connect_clicked({
        let (w, e, d, a) = (win.clone(), entry.clone(), dropdown.clone(), auto.clone());
        move |_| {
            let up = uploaders.get(d.selected() as usize).cloned().unwrap_or_default();
            on_confirm(up, e.text().trim().to_string(), a.is_active());
            w.close();
        }
    });
    win.present();
}
