//! Tray icon and menu (Windows and macOS).

use std::sync::{Arc, Mutex};

use tao::event::{Event as TaoEvent, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::{Action, App, Event, Status};

pub fn run() {
    let mut builder = EventLoopBuilder::<Event>::with_user_event();
    let event_loop = builder.build();

    // Worker threads and the menu post events into the UI loop.
    let proxy = Mutex::new(event_loop.create_proxy());
    let sink: crate::Sink = Arc::new(move |e| {
        let _ = proxy.lock().unwrap().send_event(e);
    });
    let menu_sink = sink.clone();

    let save = MenuItem::new("Save clip now", true, None);
    let clips = MenuItem::new("Open clips folder", true, None);
    let settings = MenuItem::new("Settings…", true, None);
    let restart = MenuItem::new("Restart recorder", true, None);
    let log = MenuItem::new("Open log", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let actions = [
        (save.id().clone(), Action::SaveClip),
        (clips.id().clone(), Action::OpenClips),
        (settings.id().clone(), Action::OpenSettings),
        (restart.id().clone(), Action::Restart),
        (log.id().clone(), Action::OpenLog),
        (quit.id().clone(), Action::Quit),
    ];
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if let Some((_, a)) = actions.iter().find(|(id, _)| *id == e.id) {
            menu_sink(Event::Action(*a));
        }
    }));

    let menu = Menu::new();
    let _ = menu.append_items(&[
        &save,
        &clips,
        &PredefinedMenuItem::separator(),
        &settings,
        &restart,
        &log,
        &PredefinedMenuItem::separator(),
        &quit,
    ]);

    let mut app = App::new(sink);
    let mut tray: Option<TrayIcon> = None;
    let mut shown: Option<(String, bool)> = None;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            // The tray must be created once the loop is running (macOS).
            TaoEvent::NewEvents(StartCause::Init) => {
                app.start();
                tray = TrayIconBuilder::new()
                    .with_menu(Box::new(menu.clone()))
                    .with_tooltip(app.tooltip())
                    .with_icon(icon(false))
                    .build()
                    .ok();
                if tray.is_none() {
                    crate::alert("No tray icon", "The tray icon could not be created; recording continues.");
                }
            }
            TaoEvent::UserEvent(e) => {
                if !app.handle(e) {
                    app.shutdown();
                    tray = None;
                    *control_flow = ControlFlow::Exit;
                    return;
                }
            }
            TaoEvent::LoopDestroyed => app.shutdown(),
            _ => {}
        }

        // Keep the tooltip and icon in step with the recorder.
        let now = (app.tooltip(), matches!(app.status, Status::Stopped(_)));
        if let Some(t) = &tray {
            if shown.as_ref() != Some(&now) {
                let _ = t.set_tooltip(Some(&now.0));
                let _ = t.set_icon(Some(icon(now.1)));
                save.set_enabled(app.status == Status::Recording);
                shown = Some(now);
            }
        }
    });
}

/// A 32x32 bee-striped dot, drawn in code so there is no image file to ship:
/// honey yellow while recording, grey when stopped.
fn icon(stopped: bool) -> Icon {
    const N: u32 = 32;
    let fill = if stopped { [150, 150, 150] } else { [247, 181, 0] };
    let stripe = [40, 30, 10];
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let d = (dx * dx + dy * dy).sqrt();
            // Soft edge for anti-aliasing.
            let alpha = ((15.5 - d).clamp(0.0, 1.0) * 255.0) as u8;
            let striped = matches!(x, 11..=14 | 19..=22);
            let c = if striped { stripe } else { fill };
            rgba.extend_from_slice(&[c[0], c[1], c[2], alpha]);
        }
    }
    Icon::from_rgba(rgba, N, N).expect("icon size matches its pixels")
}
