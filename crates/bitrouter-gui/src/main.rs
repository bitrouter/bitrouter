use gpui::{div, px, size, AppContext, Context, IntoElement, Render, Styled, Window};
use gpui_component::Root;

struct BlankView;

impl Render for BlankView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full()
    }
}

fn main() {
    gpui_platform::application().run(move |cx| {
        gpui_component::init(cx);

        cx.spawn(async move |cx| {
            let window_opts = gpui::WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds {
                    origin: gpui::Point::default(),
                    size: size(px(1024.0), px(768.0)),
                })),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(gpui::SharedString::from("BitRouter")),
                    ..Default::default()
                }),
                ..Default::default()
            };

            if let Err(err) = cx.open_window(window_opts, |window, cx| {
                let view = cx.new(|_| BlankView);
                cx.new(|cx| Root::new(view, window, cx))
            }) {
                eprintln!("failed to open window: {err}");
            }
        })
        .detach();
    });
}
