mod app;
mod archive;
mod browse;
mod covers;
mod db;
mod details;
mod downloads;
mod ext;
mod library;
mod loader;
mod local;
mod net;
mod nyaa;
mod nyaa_ui;
mod pages;
mod player;
mod paths;
mod probe;
mod reader;
mod source;
mod stats;
mod strip;

fn main() -> gtk::glib::ExitCode {
    paths::migrate_legacy_dirs();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--nyaa-probe") {
        std::process::exit(nyaa::probe(&args[1..]));
    }
    if args.first().map(String::as_str) == Some("--probe-raw") {
        std::process::exit(probe::run_raw(&args[1..]));
    }
    if args.first().map(String::as_str) == Some("--probe") {
        std::process::exit(probe::run(&args[1..]));
    }
    // On the target laptop (Intel UHD 620) the Vulkan renderer GTK prefers costs
    // ~10 ms of CPU per frame, the GL renderer ~2 ms. Respect an explicit choice.
    if std::env::var_os("GSK_RENDERER").is_none() {
        // SAFETY: nothing else is running yet; this is the first statement of main.
        unsafe { std::env::set_var("GSK_RENDERER", "gl") };
    }
    app::run()
}
