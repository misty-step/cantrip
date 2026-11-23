//! Cantrip v2 entry point. OWNER: integrator (main thread).
mod ipc;
mod net;
mod platform;
mod ui;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Temporary (ui fork): `cantrip gallery …` renders evidence screenshots.
    if args.first().map(String::as_str) == Some("gallery") {
        if let Err(e) = ui::gallery::run_gallery(&args[1..]) {
            eprintln!("cantrip gallery: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    let _ = ui::run;
}
