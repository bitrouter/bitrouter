use bitrouter_gui::terminal::terminal::Terminal;
use bitrouter_gui::terminal::view::TerminalView;
use gpui::{
    div, px, size, AppContext, Context, IntoElement, ParentElement, Render, Styled, Window,
};
use gpui_component::Root;

/// Root view hosting a single terminal, or an empty pane if the PTY failed to
/// spawn.
struct AppView {
    terminal: Option<gpui::Entity<TerminalView>>,
}

impl Render for AppView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div().size_full();
        if let Some(terminal) = self.terminal.clone() {
            root = root.child(terminal);
        }
        root
    }
}

/// Build a [`Terminal`] entity from the fallible [`Terminal::spawn`] constructor.
///
/// `cx.new` requires the build closure to return `Self`, so we capture the
/// spawn error out of the closure and discard the half-built entity on failure.
fn build_terminal(cx: &mut gpui::App, shell: &str) -> anyhow::Result<gpui::Entity<Terminal>> {
    let mut spawn_result: Option<anyhow::Result<()>> = None;
    let entity = cx.new(|cx| {
        match Terminal::spawn(shell, &[], None, 24, 80, cx) {
            Ok(terminal) => {
                spawn_result = Some(Ok(()));
                terminal
            }
            Err(err) => {
                // Record the failure and build a detached, never-rendered
                // placeholder; the caller drops the entity below.
                spawn_result = Some(Err(err));
                Terminal::placeholder()
            }
        }
    });
    match spawn_result {
        Some(Ok(())) => Ok(entity),
        Some(Err(err)) => Err(err),
        None => anyhow::bail!("terminal build closure did not run"),
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
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
                let terminal = match build_terminal(cx, &shell) {
                    Ok(terminal) => Some(cx.new(|cx| TerminalView::new(terminal, cx))),
                    Err(err) => {
                        eprintln!("failed to spawn terminal: {err}");
                        None
                    }
                };
                let view = cx.new(|_| AppView { terminal });
                cx.new(|cx| Root::new(view, window, cx))
            }) {
                eprintln!("failed to open window: {err}");
            }
        })
        .detach();
    });
}
