use bitrouter_gui::app_model::AppModel;
use bitrouter_gui::keymap;
use bitrouter_gui::views::root::Root;
use bitrouter_gui_core::feed::MockFeed;
use gpui::{px, size, AppContext as _};
use gpui_component::{Root as ComponentRoot, Theme, ThemeMode};

fn main() {
    gpui_platform::application().run(move |cx| {
        gpui_component::init(cx);
        // Switch to dark mode; `init` sets Light by default.
        Theme::change(ThemeMode::Dark, None, cx);
        // Register global key bindings (⌘K, ⌘N, ⌘1–⌘9).
        keymap::register(cx);

        cx.spawn(async move |cx| {
            let window_opts = gpui::WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds {
                    origin: gpui::Point::default(),
                    size: size(px(1200.0), px(800.0)),
                })),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(gpui::SharedString::from("BitRouter")),
                    ..Default::default()
                }),
                ..Default::default()
            };

            if let Err(err) = cx.open_window(window_opts, |window, cx| {
                let model = cx.new(|cx| AppModel::new(MockFeed::scenario(), cx));
                let root_view = cx.new(|cx| Root::new(model, cx));
                cx.new(|cx| ComponentRoot::new(root_view, window, cx))
            }) {
                eprintln!("failed to open window: {err}");
            }
        })
        .detach();
    });
}
