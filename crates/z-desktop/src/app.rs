use crate::ui::field_input::{
    Backspace, Copy, Cut, Delete, LineEnd, LineStart, MoveLeft, MoveRight, Paste, SelectAll,
    SelectLeft, SelectRight,
};
use crate::ui::home::HomeView;
use crate::ui::{NextField, PrevField, SubmitFocused};
use gpui::*;

pub fn run() {
    let app = Application::new();

    app.run(move |cx| {
        cx.bind_keys([
            KeyBinding::new("tab", NextField, Some("Home")),
            KeyBinding::new("shift-tab", PrevField, Some("Home")),
            KeyBinding::new("enter", SubmitFocused, Some("Home")),
            KeyBinding::new("tab", NextField, Some("FieldInput")),
            KeyBinding::new("shift-tab", PrevField, Some("FieldInput")),
            KeyBinding::new("enter", SubmitFocused, Some("FieldInput")),
            KeyBinding::new("backspace", Backspace, Some("FieldInput")),
            KeyBinding::new("delete", Delete, Some("FieldInput")),
            KeyBinding::new("left", MoveLeft, Some("FieldInput")),
            KeyBinding::new("right", MoveRight, Some("FieldInput")),
            KeyBinding::new("shift-left", SelectLeft, Some("FieldInput")),
            KeyBinding::new("shift-right", SelectRight, Some("FieldInput")),
            KeyBinding::new("ctrl-a", SelectAll, Some("FieldInput")),
            KeyBinding::new("cmd-a", SelectAll, Some("FieldInput")),
            KeyBinding::new("ctrl-c", Copy, Some("FieldInput")),
            KeyBinding::new("cmd-c", Copy, Some("FieldInput")),
            KeyBinding::new("ctrl-v", Paste, Some("FieldInput")),
            KeyBinding::new("cmd-v", Paste, Some("FieldInput")),
            KeyBinding::new("ctrl-x", Cut, Some("FieldInput")),
            KeyBinding::new("cmd-x", Cut, Some("FieldInput")),
            KeyBinding::new("home", LineStart, Some("FieldInput")),
            KeyBinding::new("end", LineEnd, Some("FieldInput")),
        ]);
        cx.spawn(async move |cx| {
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point {
                            x: px(80.0),
                            y: px(60.0),
                        },
                        size: Size {
                            width: px(1120.0),
                            height: px(900.0),
                        },
                    })),
                    titlebar: Some(TitlebarOptions {
                        title: Some("z-stack".into()),
                        appears_transparent: false,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |_window, cx| cx.new(HomeView::new),
            )?;

            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
}
