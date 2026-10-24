//! Writing to the agent: the composer (text, pasted and dropped files and images), the
//! session bar under it (mode, model, options, usage) and sending or cancelling.

use super::*;

impl ThreadView {
    pub(super) fn send(&mut self, _: &Send, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text(cx).trim().to_string();
        if text.is_empty() {
            return;
        }
        let active = if self.include_active { self.workspace.upgrade().and_then(|w| active_context(w.read(cx), cx)) } else { None };
        let status = self.thread.read(cx).status();
        let images = self.images.clone();
        let sent = match status {
            Status::Ready => self.thread.update(cx, |t, cx| t.send_with_images(text, active, images, cx)),
            // Not connected yet: connect the selected agent, then send.
            Status::Disconnected => {
                self.thread.update(cx, |t, cx| t.ask(text, window, cx));
                true
            }
            // Working: the message waits for the end of the turn.
            Status::Connecting | Status::Busy => {
                self.thread.update(cx, |t, cx| t.enqueue(text, active, images, cx));
                true
            }
        };
        if sent {
            self.images.clear();
            self.input.update(cx, |e, cx| e.set_text("", window, cx));
        }
    }

    /// Paste: images become attachments; anything else is pasted as usual.
    pub(super) fn paste(&mut self, _: &editor::actions::Paste, _: &mut Window, cx: &mut Context<Self>) {
        let images: Vec<gpui::Image> = cx
            .read_from_clipboard()
            .map(|item| item.entries().iter().filter_map(|e| if let gpui::ClipboardEntry::Image(image) = e { Some(image.clone()) } else { None }).collect())
            .unwrap_or_default();
        if images.is_empty() {
            cx.propagate();
            return;
        }
        self.images.extend(images.into_iter().map(std::sync::Arc::new));
        cx.notify();
    }

    /// Files dropped from Finder: images are attached, project files mentioned.
    pub(super) fn drop_paths(&mut self, paths: &gpui::ExternalPaths, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.thread.read(cx).root().clone();
        let mut mentions = String::new();
        for path in paths.paths() {
            let format = path.extension().and_then(|e| e.to_str()).and_then(|e| match e.to_ascii_lowercase().as_str() {
                "png" => Some(gpui::ImageFormat::Png),
                "jpg" | "jpeg" => Some(gpui::ImageFormat::Jpeg),
                "gif" => Some(gpui::ImageFormat::Gif),
                "webp" => Some(gpui::ImageFormat::Webp),
                _ => None,
            });
            match (format, std::fs::read(path)) {
                (Some(format), Ok(bytes)) => self.images.push(std::sync::Arc::new(gpui::Image::from_bytes(format, bytes))),
                _ => {
                    if let Ok(rel) = path.strip_prefix(&root) {
                        mentions.push_str(&format!("@{} ", rel.to_string_lossy()));
                    }
                }
            }
        }
        if !mentions.is_empty() {
            self.input.update(cx, |e, cx| e.insert(&mentions, window, cx));
        }
        cx.notify();
    }

    pub(super) fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        self.thread.update(cx, |t, cx| t.cancel(cx));
    }

    pub(super) fn render_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let status = self.thread.read(cx).status();
        let root = self.thread.read(cx).root().clone();
        let has_text = !self.input.read(cx).text(cx).trim().is_empty();
        let can_send = matches!(status, Status::Ready | Status::Disconnected) && has_text;
        let queued: Vec<String> = self.thread.read(cx).queue.iter().map(|q| q.text.clone()).collect();
        let handle = self.thread.downgrade();
        let input = self.input.clone();
        let queue = (!queued.is_empty()).then(|| {
            v_flex().gap_0p5().pb_1().children(queued.into_iter().enumerate().map(|(ix, text)| {
                let first_line: String = text.lines().next().unwrap_or_default().chars().take(120).collect();
                let (h1, h2, h3, input) = (handle.clone(), handle.clone(), handle.clone(), input.clone());
                h_flex()
                    .gap_1()
                    .px_1()
                    .rounded_sm()
                    .bg(colors.element_background)
                    .child(Icon::new(IconName::ListTodo).size(IconSize::XSmall).color(Color::Muted))
                    .child(div().flex_1().min_w_0().child(Label::new(first_line).size(LabelSize::Small).color(Color::Muted).truncate()))
                    .child(Label::new("queued").size(LabelSize::XSmall).color(Color::Muted))
                    .child(
                        IconButton::new(("queued-now", ix), IconName::Send)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Send now (stops the current turn)"))
                            .on_click(move |_, _, cx| {
                                h1.update(cx, |t, cx| t.send_queued_now(ix, cx)).ok();
                            }),
                    )
                    .child(
                        IconButton::new(("queued-edit", ix), IconName::Pencil)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Back to the input to edit it"))
                            .on_click(move |_, window, cx| {
                                if let Some(Some(text)) = h2.update(cx, |t, cx| t.unqueue(ix, cx)).ok() {
                                    input.update(cx, |e, cx| e.set_text(text, window, cx));
                                }
                            }),
                    )
                    .child(
                        IconButton::new(("queued-remove", ix), IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Don't send it"))
                            .on_click(move |_, _, cx| {
                                h3.update(cx, |t, cx| t.unqueue(ix, cx)).ok();
                            }),
                    )
            }))
        });
        let active = self.workspace.upgrade().and_then(|w| active_context(w.read(cx), cx));
        let chip = match (&active, self.include_active) {
            (Some(a), true) => Some(
                Button::new("thread-active-context", a.label(&root))
                    .style(ButtonStyle::Subtle)
                    .size(ButtonSize::Compact)
                    .start_icon(Icon::new(IconName::File).size(IconSize::XSmall))
                    .end_icon(Icon::new(IconName::Close).size(IconSize::XSmall))
                    .tooltip(Tooltip::text("Sent with your message. Click to leave it out."))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.include_active = false;
                        cx.notify();
                    }))
                    .into_any_element(),
            ),
            (Some(_), false) => Some(
                Button::new("thread-active-context", "Include the active file")
                    .style(ButtonStyle::Transparent)
                    .size(ButtonSize::Compact)
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.include_active = true;
                        cx.notify();
                    }))
                    .into_any_element(),
            ),
            (None, _) => None,
        };
        h_flex().w_full().justify_center().px_4().pb_4().child(
            v_flex()
                .w_full()
                .max_w(px(880.))
                .gap_1()
                .p_2()
                .rounded_lg()
                .border_1()
                .border_color(colors.border)
                .bg(colors.elevated_surface_background)
                .children(queue)
                .when(!self.images.is_empty(), |el| {
                    let accepts = self.thread.read(cx).accepts_images() || status == Status::Disconnected;
                    el.child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .children(self.images.iter().enumerate().map(|(ix, image)| {
                                h_flex()
                                    .gap_0p5()
                                    .p_0p5()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(colors.border)
                                    .child(gpui::img(image.clone()).h(px(40.)).max_w(px(80.)).rounded_sm())
                                    .child(
                                        IconButton::new(("remove-image", ix), IconName::Close)
                                            .icon_size(IconSize::XSmall)
                                            .tooltip(Tooltip::text("Remove"))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if ix < this.images.len() {
                                                    this.images.remove(ix);
                                                }
                                                cx.notify();
                                            })),
                                    )
                            }))
                            .when(!accepts, |el| el.child(Label::new("This agent doesn't take images").size(LabelSize::XSmall).color(Color::Warning))),
                    )
                })
                .when_some(chip, |el, chip| el.child(h_flex().child(chip)))
                .child(
                    h_flex()
                        .gap_2()
                        .items_end()
                        .child(
                            div()
                                .key_context("ForgeAgentInput")
                                .flex_1()
                                .min_w_0()
                                .px_1()
                                .capture_action(cx.listener(Self::paste))
                                .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| this.drop_paths(paths, window, cx)))
                                .child(self.input.clone()),
                        )
                        .when(status == Status::Busy && has_text, |el| {
                            el.child(
                                IconButton::new("thread-queue", IconName::Send)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Send when the agent finishes (Enter)"))
                                    .on_click(cx.listener(|this, _, window, cx| this.send(&Send, window, cx))),
                            )
                        })
                        .child(match status {
                            Status::Busy => IconButton::new("thread-cancel", IconName::Stop)
                                .icon_color(Color::Error)
                                .tooltip(Tooltip::text("Stop the agent"))
                                .on_click(cx.listener(|this, _, window, cx| this.cancel(&Cancel, window, cx)))
                                .into_any_element(),
                            _ => IconButton::new("thread-send", IconName::Send)
                                .icon_color(Color::Accent)
                                .disabled(!can_send)
                                .tooltip(Tooltip::text("Send (Enter)"))
                                .on_click(cx.listener(|this, _, window, cx| this.send(&Send, window, cx)))
                                .into_any_element(),
                        }),
                )
                .children(self.render_session_bar(cx)),
        )
        .into_any_element()
    }

    /// Under the input: the agent's session settings (mode, model, effort…), then what
    /// the session has used (the plan's limits, the context window).
    pub(super) fn render_session_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let thread = self.thread.read(cx);
        let handle = self.thread.downgrade();
        let mut settings: Vec<AnyElement> = Vec::new();
        for (ix, option) in thread.config_options.iter().enumerate() {
            let tooltip: SharedString = option.description.clone().map(|d| format!("{}: {d}", option.name)).unwrap_or_else(|| option.name.clone()).into();
            match &option.value {
                crate::thread::ConfigValue::Select { current, choices } => {
                    let (id, current, choices, handle) = (option.id.clone(), current.clone(), choices.clone(), handle.clone());
                    let icon = match option.category.as_deref() {
                        Some("mode") => IconName::Settings,
                        Some("model") => IconName::ZedAgent,
                        _ => IconName::Sparkle,
                    };
                    settings.push(
                        PopoverMenu::new(("thread-option", ix))
                            .trigger(
                                ButtonLike::new(("thread-option-trigger", ix))
                                    .style(ButtonStyle::Subtle)
                                    .size(ButtonSize::Compact)
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
                                            .child(Label::new(option.current_label()).size(LabelSize::XSmall))
                                            .child(Icon::new(IconName::ChevronDown).size(IconSize::XSmall).color(Color::Muted)),
                                    )
                                    .tooltip(Tooltip::text(tooltip)),
                            )
                            .menu(move |window, cx| {
                                let (id, current, choices, handle) = (id.clone(), current.clone(), choices.clone(), handle.clone());
                                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                    for (value, name, description) in choices {
                                        let label = match description {
                                            Some(d) if !d.is_empty() => format!("{name} — {d}"),
                                            _ => name,
                                        };
                                        let (id, handle, checked) = (id.clone(), handle.clone(), value == current);
                                        menu = menu.toggleable_entry(label, checked, ui::IconPosition::Start, None, move |_, cx| {
                                            handle.update(cx, |t, cx| t.set_config_option(id.clone(), serde_json::Value::String(value.clone()), cx)).ok();
                                        });
                                    }
                                    menu
                                }))
                            })
                            .into_any_element(),
                    );
                }
                crate::thread::ConfigValue::Boolean(on) => {
                    let (id, on, handle) = (option.id.clone(), *on, handle.clone());
                    settings.push(
                        Button::new(("thread-option-toggle", ix), option.name.clone())
                            .style(if on { ButtonStyle::Filled } else { ButtonStyle::Subtle })
                            .size(ButtonSize::Compact)
                            .label_size(LabelSize::XSmall)
                            .toggle_state(on)
                            .tooltip(Tooltip::text(tooltip))
                            .on_click(move |_, _, cx| {
                                handle.update(cx, |t, cx| t.set_config_option(id.clone(), serde_json::Value::Bool(!on), cx)).ok();
                            })
                            .into_any_element(),
                    );
                }
            }
        }
        // Agents with modes but no settings list (ACP `modes` only).
        let has_mode_setting = thread.config_options.iter().any(|o| o.category.as_deref() == Some("mode"));
        if !has_mode_setting && !thread.modes.is_empty() {
            let modes = thread.modes.clone();
            let current_mode = thread.current_mode.clone();
            let current_name = modes.iter().find(|m| Some(&m.id) == current_mode.as_ref()).map(|m| m.name.clone()).unwrap_or_else(|| "Mode".into());
            let handle = handle.clone();
            settings.push(
                PopoverMenu::new("thread-mode")
                    .trigger(
                        ButtonLike::new("thread-mode-trigger")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(Icon::new(IconName::Settings).size(IconSize::XSmall).color(Color::Muted))
                                    .child(Label::new(current_name).size(LabelSize::XSmall))
                                    .child(Icon::new(IconName::ChevronDown).size(IconSize::XSmall).color(Color::Muted)),
                            )
                            .tooltip(Tooltip::text("How much the agent asks before acting")),
                    )
                    .menu(move |window, cx| {
                        let (modes, current_mode, handle) = (modes.clone(), current_mode.clone(), handle.clone());
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for mode in modes {
                                let checked = Some(&mode.id) == current_mode.as_ref();
                                let label = match &mode.description {
                                    Some(d) => format!("{} — {d}", mode.name),
                                    None => mode.name.clone(),
                                };
                                let (handle, id) = (handle.clone(), mode.id.clone());
                                menu = menu.toggleable_entry(label, checked, ui::IconPosition::Start, None, move |_, cx| {
                                    handle.update(cx, |t, cx| t.set_mode(id.clone(), cx)).ok();
                                });
                            }
                            menu
                        }))
                    })
                    .into_any_element(),
            );
        }
        let plan = thread.plan_usage(cx).and_then(|plan| render_plan_usage(&plan));
        let usage = render_usage(&thread.usage, cx);
        if settings.is_empty() && plan.is_none() && usage.is_none() {
            return None;
        }
        Some(
            h_flex()
                .gap_1()
                .flex_wrap()
                .pt_1()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .children(settings)
                .child(div().flex_1())
                .children(plan)
                .children(usage)
                .into_any_element(),
        )
    }
}

/// `12.3k`, `1.2M`.
pub(crate) fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1_000.0).replace(".0k", "k"),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0).replace(".0M", "M"),
    }
}

/// How much of the account's limits are used (Claude's 5-hour and weekly windows): the
/// tightest one in the header, all of them in the tooltip.
pub(super) fn render_plan_usage(plan: &crate::plan_usage::PlanUsage) -> Option<AnyElement> {
    let tightest = plan.tightest()?;
    let color = if tightest.percent >= 90.0 {
        Color::Error
    } else if tightest.percent >= 70.0 {
        Color::Warning
    } else {
        Color::Muted
    };
    let mut lines: Vec<String> = Vec::new();
    lines.push(match &plan.subscription {
        Some(s) => format!("Claude {s} plan"),
        None => "Plan usage".into(),
    });
    for limit in &plan.limits {
        let resets = limit.resets.as_ref().map(|r| format!(" · resets {r}")).unwrap_or_default();
        lines.push(format!("{}: {}% used{resets}", limit.label, limit.percent.round()));
    }
    let short = if tightest.label.starts_with("5-hour") { "5h" } else if tightest.label.starts_with("Weekly") { "week" } else { "plan" };
    Some(
        div()
            .id("thread-plan-usage")
            .px_1()
            .child(
                h_flex()
                    .gap_1()
                    .child(Label::new("Plan").size(LabelSize::XSmall).color(Color::Muted))
                    .child(Label::new(format!("{}%", tightest.percent.round())).size(LabelSize::XSmall).color(color))
                    .child(Label::new(short).size(LabelSize::XSmall).color(Color::Muted)),
            )
            .tooltip(Tooltip::text(SharedString::from(lines.join("\n"))))
            .into_any_element(),
    )
}

/// How full the context window is (and, in the tooltip, what the session has spent).
pub(super) fn render_usage(usage: &crate::thread::Usage, cx: &App) -> Option<AnyElement> {
    if usage.is_empty() {
        return None;
    }
    let mut lines = Vec::new();
    if let (Some(used), Some(size)) = (usage.context_used, usage.context_size) {
        lines.push(format!("Context: {} of {} tokens ({} left)", tokens(used), tokens(size), tokens(size.saturating_sub(used))));
    }
    if usage.turns > 0 {
        lines.push(format!("This session: {} in, {} out", tokens(usage.input), tokens(usage.output)));
        if usage.cached_read > 0 {
            lines.push(format!("Read from cache: {}", tokens(usage.cached_read)));
        }
        if usage.thought > 0 {
            lines.push(format!("Reasoning: {}", tokens(usage.thought)));
        }
        lines.push(format!("{} turn{}", usage.turns, if usage.turns == 1 { "" } else { "s" }));
    }
    if let Some((amount, currency)) = &usage.cost {
        lines.push(format!("Cost: {amount:.2} {currency}"));
    }
    let tooltip: SharedString = lines.join("\n").into();
    let chip = match usage.context_ratio() {
        Some(ratio) => {
            // Nearly full: the agent will soon compact or forget the start of the thread.
            let (color, fill) = if ratio >= 0.9 {
                (Color::Error, cx.theme().status().error)
            } else if ratio >= 0.7 {
                (Color::Warning, cx.theme().status().warning)
            } else {
                (Color::Muted, cx.theme().colors().text_accent)
            };
            h_flex()
                .gap_1()
                .child(
                    div()
                        .w(px(36.))
                        .h(px(4.))
                        .rounded_full()
                        .bg(cx.theme().colors().element_background)
                        .child(div().h_full().rounded_full().w(relative(ratio)).bg(fill)),
                )
                .child(Label::new(format!("{}%", (ratio * 100.0).round() as u32)).size(LabelSize::XSmall).color(color))
                .child(Label::new(format!("{} / {}", tokens(usage.context_used.unwrap_or(0)), tokens(usage.context_size.unwrap_or(0)))).size(LabelSize::XSmall).color(Color::Muted))
        }
        None => h_flex().child(Label::new(format!("{} tokens", tokens(usage.input + usage.output))).size(LabelSize::XSmall).color(Color::Muted)),
    };
    Some(div().id("thread-usage").px_1().child(chip).tooltip(Tooltip::text(tooltip)).into_any_element())
}
