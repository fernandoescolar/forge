//! Dialogs (`window.prompt`: confirmations, About, update offers) drawn by GPUI inside the
//! window, in Forge's theme, instead of the platform's own.
//!
//! macOS keeps its native alert sheets: that is what Mac apps do. On Linux GPUI has no
//! native dialog and would fall back to a plain white box; on Windows the system
//! message box clashes with the rest of the window. Both get this renderer.

use gpui::{
    App, Decorations, Entity, EventEmitter, FocusHandle, Focusable, Pixels, PromptButton, PromptHandle, PromptLevel, PromptResponse, RenderablePromptHandle,
    SharedString, TextStyleRefinement, Window, div, prelude::*,
};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use theme::ClientDecorationsExt as _;
use theme_settings::ThemeSettings;
use settings::Settings as _;
use ui::{TintColor, prelude::*};

pub fn init(cx: &mut App) {
    if cfg!(not(target_os = "macos")) {
        cx.set_prompt_builder(forge_prompt_renderer);
    }
}

fn forge_prompt_renderer(
    level: PromptLevel,
    message: &str,
    detail: Option<&str>,
    actions: &[PromptButton],
    handle: PromptHandle,
    window: &mut Window,
    cx: &mut App,
) -> RenderablePromptHandle {
    let renderer = cx.new(|cx| ForgePrompt {
        level,
        message: cx.new(|cx| Markdown::new(SharedString::new(message), None, None, cx)),
        detail: detail.filter(|text| !text.is_empty()).map(|text| cx.new(|cx| Markdown::new(SharedString::new(text), None, None, cx))),
        actions: actions.iter().map(|a| a.label().to_string()).collect(),
        focus: cx.focus_handle(),
        active: 0,
    });
    handle.with_view(renderer, window, cx)
}

pub struct ForgePrompt {
    level: PromptLevel,
    message: Entity<Markdown>,
    detail: Option<Entity<Markdown>>,
    actions: Vec<String>,
    focus: FocusHandle,
    /// The button Enter activates; arrows and Tab move it.
    active: usize,
}

impl ForgePrompt {
    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PromptResponse(self.active));
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.actions.iter().position(|a| a == "Cancel") {
            cx.emit(PromptResponse(ix));
        } else if self.actions.len() == 1 {
            // An OK-only notice (About, say) closes with Escape too.
            cx.emit(PromptResponse(0));
        }
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.active = (self.active + 1) % self.actions.len().max(1);
        cx.notify();
    }

    fn select_previous(&mut self, _: &menu::SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.active = if self.active == 0 { self.actions.len().saturating_sub(1) } else { self.active - 1 };
        cx.notify();
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.active = 0;
        cx.notify();
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.active = self.actions.len().saturating_sub(1);
        cx.notify();
    }

    fn icon(&self) -> Option<(IconName, Color)> {
        match self.level {
            PromptLevel::Info => None,
            PromptLevel::Warning => Some((IconName::Warning, Color::Warning)),
            PromptLevel::Critical => Some((IconName::XCircle, Color::Error)),
        }
    }
}

impl Render for ForgePrompt {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);
        let icon = self.icon();

        let dialog = v_flex()
            .key_context("Prompt")
            .cursor_default()
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .w(px(400.))
            .p_4()
            .gap_4()
            .elevation_3(cx)
            .overflow_hidden()
            .font_family(settings.ui_font.family.clone())
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .children(icon.map(|(name, color)| Icon::new(name).size(IconSize::Medium).color(color)))
                    .child(div().flex_1().child(MarkdownElement::new(self.message.clone(), markdown_style(true, window, cx)))),
            )
            .children(self.detail.clone().map(|detail| div().w_full().text_sm().child(MarkdownElement::new(detail, markdown_style(false, window, cx)))))
            .child(
                h_flex().gap_1().justify_end().flex_wrap().children(self.actions.iter().enumerate().map(|(ix, action)| {
                    Button::new(ix, action.clone())
                        .style(ButtonStyle::Outlined)
                        .when(ix == self.active, |b| b.style(ButtonStyle::Tinted(TintColor::Accent)))
                        .tab_index(ix as isize)
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(PromptResponse(ix))))
                })),
            );

        let decorations = window.window_decorations();
        let inset = window.client_inset().unwrap_or(Pixels::ZERO);

        div().size_full().child(
            v_flex()
                .occlude()
                .absolute()
                .inset_0()
                .bg(gpui::black().opacity(0.3))
                .map(|this| match decorations {
                    Decorations::Server => this,
                    Decorations::Client { tiling } => this
                        .when(!tiling.top, |this| this.top(inset))
                        .when(!tiling.bottom, |this| this.bottom(inset))
                        .when(!tiling.left, |this| this.left(inset))
                        .when(!tiling.right, |this| this.right(inset))
                        .rounded_client_corners(tiling),
                })
                .items_center()
                .justify_center()
                .child(dialog),
        )
    }
}

fn markdown_style(main_message: bool, window: &Window, cx: &App) -> MarkdownStyle {
    let mut base_text_style = window.text_style();
    let settings = ThemeSettings::get_global(cx);
    let font_size: Pixels = settings.ui_font_size(cx).into();
    let (font_size, color, weight) = if main_message {
        (font_size * 1.1, Color::Default.color(cx), Some(gpui::FontWeight::SEMIBOLD))
    } else {
        (font_size, Color::Muted.color(cx), None)
    };
    base_text_style.refine(&TextStyleRefinement {
        font_family: Some(settings.ui_font.family.clone()),
        font_size: Some(font_size.into()),
        font_weight: weight,
        color: Some(color),
        ..Default::default()
    });
    MarkdownStyle { base_text_style, selection_background_color: cx.theme().colors().element_selection_background, ..Default::default() }
}

impl EventEmitter<PromptResponse> for ForgePrompt {}

impl Focusable for ForgePrompt {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
