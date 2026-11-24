//! Test helper: a persistent wlroots virtual pointer. `vptr X Y W H` moves to
//! (X, Y) in a WxH output, waits so clients see the pointer enter, clicks the
//! left button, waits again, exits.
use std::time::Duration;
use wayland_client::{globals::{registry_queue_init, GlobalListContents}, protocol::{wl_registry, wl_seat::WlSeat, wl_pointer::ButtonState}, Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::virtual_pointer::v1::client::{zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1 as Mgr, zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1 as Ptr};
struct S;
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for S { fn event(_: &mut S, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<S>) {} }
impl Dispatch<WlSeat, ()> for S { fn event(_: &mut S, _: &WlSeat, _: <WlSeat as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<S>) {} }
impl Dispatch<Mgr, ()> for S { fn event(_: &mut S, _: &Mgr, _: <Mgr as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<S>) {} }
impl Dispatch<Ptr, ()> for S { fn event(_: &mut S, _: &Ptr, _: <Ptr as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<S>) {} }
fn main() {
    let a: Vec<u32> = std::env::args().skip(1).map(|s| s.parse().unwrap()).collect();
    let conn = Connection::connect_to_env().unwrap();
    let (globals, mut q) = registry_queue_init::<S>(&conn).unwrap();
    let qh = q.handle();
    let seat: WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
    let mgr: Mgr = globals.bind(&qh, 1..=2, ()).unwrap();
    let p = mgr.create_virtual_pointer(Some(&seat), &qh, ());
    let t = |ms: u64| (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 + ms) as u32;
    q.roundtrip(&mut S).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    p.motion_absolute(t(0), a[0], a[1], a[2], a[3]); p.frame();
    q.roundtrip(&mut S).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    p.motion_absolute(t(0), a[0], a[1], a[2], a[3]); p.frame();
    q.roundtrip(&mut S).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    p.button(t(0), 0x110, ButtonState::Pressed); p.frame();
    q.roundtrip(&mut S).unwrap();
    std::thread::sleep(Duration::from_millis(60));
    p.button(t(0), 0x110, ButtonState::Released); p.frame();
    q.roundtrip(&mut S).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    p.destroy(); q.roundtrip(&mut S).unwrap();
}
