//! `LiveStage --preview DIR`: each view of a demo session, rendered to PNG
//! off-screen. A developer tool behind the `ui-preview` feature.
//!
//! The demo opens no input device — nothing is captured — and its faders and
//! sends are set the way a soundcheck leaves them, so the strips show real
//! controls in real states.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{AnyWindowHandle, App, AppContext};
use livestage_engine::{
    BusStrip, ChannelStrip, InputPatch, InsertPlugin, InsertSlot, OutputPatch, PatchSource,
    SendSlot, Session, StripOutput,
};

use crate::app::{LiveStageApp, View};

fn demo_session() -> Session {
    let mut session = Session {
        name: "Friday Show".to_string(),
        ..Session::default()
    };
    session.audio.input_device = Some("(none: preview)".to_string());
    let reverb = session.allocate_id();
    let monitor = session.allocate_id();
    let builtin = |session: &mut Session, stem: &str| InsertSlot {
        id: session.allocate_id(),
        bypass: false,
        plugin: InsertPlugin::Builtin {
            stem: stem.to_string(),
            params: Default::default(),
        },
    };
    let channels = [
        ("Kick", InputPatch::mono(0), -4.0, 0.0),
        ("Snare", InputPatch::mono(1), -6.0, 0.1),
        ("Bass DI", InputPatch::mono(2), -3.0, 0.0),
        ("Keys", InputPatch::stereo(3, 4), -8.0, 0.0),
        ("Lead Vox", InputPatch::mono(5), -2.5, 0.0),
        ("BV", InputPatch::mono(6), -9.0, -0.3),
    ];
    for (name, input, fader, pan) in channels {
        let id = session.allocate_id();
        let mut strip = ChannelStrip::new(id, name.to_string(), input);
        strip.core.fader_db = fader;
        strip.core.pan = pan;
        session.channels.push(strip);
    }
    let fa76 = builtin(&mut session, "fa76");
    let eq = builtin(&mut session, "equz8");
    let vox = &mut session.channels[4];
    vox.core.inserts = vec![eq, fa76];
    vox.record_arm = true;
    vox.sends = vec![
        SendSlot {
            bus: reverb,
            level_db: -12.0,
            pre_fader: false,
        },
        SendSlot {
            bus: monitor,
            level_db: 0.0,
            pre_fader: true,
        },
    ];
    let comp = builtin(&mut session, "zcomp");
    session.channels[1].core.inserts = vec![comp];
    session.channels[0].core.solo = true;
    session.channels[2].core.mute = true;
    let verb = builtin(&mut session, "verbspace");
    session.buses.push(BusStrip {
        id: reverb,
        name: "Reverb".to_string(),
        core: livestage_engine::StripCore {
            inserts: vec![verb],
            fader_db: -6.0,
            ..Default::default()
        },
        output: StripOutput::Master,
        record_arm: false,
    });
    session.buses.push(BusStrip {
        id: monitor,
        name: "IEM 1".to_string(),
        output: StripOutput::None,
        ..BusStrip::default()
    });
    let limit = builtin(&mut session, "burnlimit");
    session.master.core.inserts = vec![limit];
    session.master.record_arm = true;
    session.outputs.push(OutputPatch {
        source: PatchSource::Bus(monitor),
        left: 0,
        right: Some(1),
    });
    session
}

pub fn run(out: PathBuf, cx: &mut App) {
    let _ = std::fs::create_dir_all(&out);
    let path = std::env::temp_dir().join("livestage-preview.livestage.json");
    let handle = match crate::app::open_window_with(demo_session(), path, cx) {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("could not open: {error}");
            cx.quit();
            return;
        }
    };
    let any: AnyWindowHandle = handle.into();
    let _ = any.update(cx, |_, window, _| {
        sphere_ui_components::preview::tuck_away(window)
    });
    cx.spawn(async move |cx| {
        for (view, name) in [
            (View::Mixer, "mixer"),
            (View::Patch, "patch"),
            (View::Setup, "setup"),
        ] {
            let _ = handle.update(cx, |app: &mut LiveStageApp, _, cx| {
                app.view = view;
                cx.notify();
            });
            cx.background_executor()
                .timer(Duration::from_millis(1200))
                .await;
            let file = out.join(format!("{name}.png"));
            let saved = cx
                .update_window(any, |_, window, _| window.render_to_image())
                .and_then(|frame| frame)
                .and_then(|image| {
                    image.save(&file)?;
                    Ok((image.width(), image.height()))
                });
            match saved {
                Ok((w, h)) => println!("{name} → {} ({w}×{h})", file.display()),
                Err(error) => eprintln!("{name}: {error:#}"),
            }
        }
        // A built-in editor, opened from the mixer the way a click opens it:
        // the vocal's FA-76, with a changed value in the session.
        let opened = handle.update(cx, |app: &mut LiveStageApp, _, cx| {
            let vox = app.engine.session().channels[4].clone();
            let fa76 = vox.core.inserts[1].id;
            app.run(livestage_engine::Command::SetInsertParam {
                insert: fa76,
                index: fa76::ipc::INPUT_INDEX,
                value: 6.0,
            });
            crate::editors::open_editor(
                app,
                livestage_engine::StripRef::Channel(vox.id),
                fa76,
                None,
                cx,
            );
            app.editors.window(fa76)
        });
        if let Ok(Some(editor)) = opened {
            let _ = editor.update(cx, |_, window, _| {
                sphere_ui_components::preview::tuck_away(window)
            });
            cx.background_executor()
                .timer(Duration::from_millis(1200))
                .await;
            let file = out.join("editor-fa76.png");
            match cx
                .update_window(editor, |_, window, _| window.render_to_image())
                .and_then(|frame| frame)
                .and_then(|image| {
                    image.save(&file)?;
                    Ok((image.width(), image.height()))
                }) {
                Ok((w, h)) => println!("editor → {} ({w}×{h})", file.display()),
                Err(error) => eprintln!("editor: {error:#}"),
            }
        } else {
            eprintln!("editor: did not open");
        }
        let _ = cx.update(|cx| cx.quit());
    })
    .detach();
}
