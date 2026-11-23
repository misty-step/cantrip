//! Cantrip v2 entry point. OWNER: integrator (main thread).
mod ipc;
mod net;
mod platform;
mod ui;

fn main() {
    let _ = ui::run;
}
