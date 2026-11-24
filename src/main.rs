mod app;
mod library;
mod loader;
mod stats;
mod strip;

fn main() -> gtk::glib::ExitCode {
    // On the target laptop (Intel UHD 620) the Vulkan renderer GTK prefers costs
    // ~10 ms of CPU per frame, the GL renderer ~2 ms. Respect an explicit choice.
    if std::env::var_os("GSK_RENDERER").is_none() {
        // SAFETY: nothing else is running yet; this is the first statement of main.
        unsafe { std::env::set_var("GSK_RENDERER", "gl") };
    }
    app::run()
}
