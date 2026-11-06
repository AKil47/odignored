//! System tray UI. Owns the main-thread event loop; the daemon runs on a worker thread.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};

use crate::daemon::{self, Msg};

enum UserEvent {
    Menu(MenuEvent),
    DaemonExited,
}

/// A 32x32 blue disc with a white slash ("ignored").
fn icon() -> Icon {
    const N: u32 = 32;
    let mut px = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N as i32 {
        for x in 0..N as i32 {
            let (dx, dy) = (x - 16, y - 16);
            let rgba = if dx * dx + dy * dy > 15 * 15 {
                [0, 0, 0, 0]
            } else if (dx + dy).abs() <= 2 {
                [255, 255, 255, 255]
            } else {
                [0, 120, 212, 255]
            };
            px.extend_from_slice(&rgba);
        }
    }
    Icon::from_rgba(px, N, N).expect("valid icon")
}

/// Run the daemon with a tray icon until the user quits or the daemon dies.
pub fn run(roots: Vec<PathBuf>, cooldown: Duration, log: Option<PathBuf>) -> Result<()> {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some({
        let proxy = proxy.clone();
        move |e| {
            let _ = proxy.send_event(UserEvent::Menu(e));
        }
    }));

    let open_log = MenuItem::new("Open log", log.is_some(), None);
    let quit = MenuItem::new("Quit odignored", true, None);
    let menu = Menu::new();
    menu.append_items(&[&open_log, &PredefinedMenuItem::separator(), &quit])?;
    let (open_id, quit_id) = (open_log.id().clone(), quit.id().clone());

    // If the daemon dies (e.g. not elevated) the icon goes with it instead of lingering.
    let (tx, _daemon) = daemon::start(roots, cooldown, move || {
        let _ = proxy.send_event(UserEvent::DaemonExited);
    });

    let mut tray = None;
    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                tray = TrayIconBuilder::new()
                    .with_menu(Box::new(menu.clone()))
                    .with_tooltip("odignored - keeping OneDrive tidy")
                    .with_icon(icon())
                    .build()
                    .map_err(|e| tracing::error!("tray: {e}"))
                    .ok();
            }
            // Ask the daemon to stop (it relaunches OneDrive if we'd killed it); we exit once it finishes.
            Event::UserEvent(UserEvent::Menu(e)) if e.id == quit_id => {
                let _ = tx.send(Msg::Quit);
            }
            Event::UserEvent(UserEvent::Menu(e)) if e.id == open_id => {
                if let Some(p) = &log {
                    let _ = Command::new("explorer.exe").arg(p).spawn();
                }
            }
            Event::UserEvent(UserEvent::DaemonExited) => {
                tray.take(); // removes the icon
                *flow = ControlFlow::Exit;
            }
            _ => {}
        }
    })
}
