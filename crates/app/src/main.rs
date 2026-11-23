//! Cantrip v2 entry point. OWNER: integrator (main thread).
mod platform;
mod ui;

fn main() {
    let _ = ui::run;
}
