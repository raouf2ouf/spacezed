//! Spacezed spaceline: a Spacemacs-style powerline status bar.
//!
//! Replaces the stock status bar items with a single component rendering the
//! whole bar: window-number badge + mode block (state-colored), buffer name,
//! git branch on the left; language, cursor position, and scroll percent on
//! the right, joined by powerline arrows. Opinionated by design: the
//! spacemacs-dark palette is hardcoded where no theme key exists.

use crate::zed::agent_registry::{AgentRegistry, AgentStatus};
use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, Entity, Hsla, IntoElement, ParentElement, PathBuilder, Render, SharedString,
    Styled, Subscription, WeakEntity, Window, canvas, point, px, rgb,
};
use language::Point;
use settings::Settings;
use terminal_view::TerminalView;
use theme::ThemeSettings;
use ui::prelude::*;
use util::truncate_and_trailoff;
use vim::{Mode, ModeIndicator};
use workspace::{StatusItemView, Workspace, item::ItemHandle};

const BAR_HEIGHT: f32 = 26.0;
const ARROW_WIDTH: f32 = 9.0;
const BADGE_SIZE: f32 = 16.0;
const MAX_SEGMENT_TEXT: usize = 40;

// Spacemacs palette values that have no theme key (vim mode colors come from
// the theme's `vim_*` keys so they stay user-themeable).
fn seg2_background() -> Hsla {
    rgb(0x34323e).into()
}
fn seg3_background() -> Hsla {
    rgb(0x2a2a32).into()
}
fn segment_text_color() -> Hsla {
    rgb(0xb2b2b2).into()
}
fn dim_text_color() -> Hsla {
    rgb(0x7d7a8a).into()
}
fn term_vi_background() -> Hsla {
    rgb(0x4f97d7).into()
}
fn on_state_color() -> Hsla {
    rgb(0x1a1a22).into()
}
fn agent_working_color() -> Hsla {
    rgb(0x67b11d).into()
}
fn agent_blocked_color() -> Hsla {
    rgb(0xf2241f).into()
}

enum ActiveItem {
    // Editor data arrives via subscriptions, so no handle is stored here.
    Editor,
    Terminal(WeakEntity<TerminalView>),
    Other,
    None,
}

pub struct Spaceline {
    workspace: WeakEntity<Workspace>,
    mode_indicator: Entity<ModeIndicator>,
    active_item: ActiveItem,
    item_name: Option<SharedString>,
    item_dirty: bool,
    cursor_position: Option<(u32, u32)>,
    scroll_percent: Option<SharedString>,
    language_name: Option<SharedString>,
    _item_subscriptions: Vec<Subscription>,
}

impl Spaceline {
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // The embedded ModeIndicator is never rendered; it owns the
        // vim-focus subscription machinery and notifies on mode changes.
        let mode_indicator = cx.new(|cx| ModeIndicator::new(window, cx));
        cx.observe(&mode_indicator, |_, _, cx| cx.notify()).detach();

        let git_store = workspace.project().read(cx).git_store().clone();
        cx.observe(&git_store, |_, _, cx| cx.notify()).detach();

        if let Some(agent_registry) = AgentRegistry::global(cx) {
            cx.observe(&agent_registry, |_, _, cx| cx.notify()).detach();
        }

        Self {
            workspace: workspace.weak_handle(),
            mode_indicator,
            active_item: ActiveItem::None,
            item_name: None,
            item_dirty: false,
            cursor_position: None,
            scroll_percent: None,
            language_name: None,
            _item_subscriptions: Vec::new(),
        }
    }

    fn refresh_editor_state(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let (cursor, scroll, max_row, visible_lines) = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let head = editor.selections.newest::<Point>(&snapshot).head();
            (
                (head.row + 1, head.column + 1),
                editor.scroll_position(cx).y,
                editor.max_point(cx).row().0 as f64,
                editor.visible_line_count(),
            )
        });
        self.cursor_position = Some(cursor);
        self.scroll_percent = Some(scroll_percent_label(scroll, max_row, visible_lines));

        let editor = editor.read(cx);
        self.language_name = editor
            .active_excerpt(cx)
            .and_then(|(_, buffer, _)| buffer.read(cx).language().map(|language| language.name()))
            .map(|name| SharedString::from(name.to_string()));
        cx.notify();
    }

    fn block_state(&self, cx: &App) -> (SharedString, Hsla, Hsla) {
        let theme_colors = cx.theme().colors();
        if let ActiveItem::Terminal(terminal_view) = &self.active_item
            && let Some(terminal_view) = terminal_view.upgrade()
        {
            return if terminal_view
                .read(cx)
                .terminal()
                .read(cx)
                .vi_mode_enabled()
            {
                ("TERM-VI".into(), term_vi_background(), on_state_color())
            } else {
                (
                    "TERM".into(),
                    theme_colors.vim_insert_background,
                    theme_colors.vim_insert_foreground,
                )
            };
        }
        match self.mode_indicator.read(cx).current_mode(cx) {
            Some(Mode::Insert) => (
                "INSERT".into(),
                theme_colors.vim_insert_background,
                theme_colors.vim_insert_foreground,
            ),
            Some(Mode::Replace) => (
                "REPLACE".into(),
                theme_colors.vim_replace_background,
                theme_colors.vim_replace_foreground,
            ),
            Some(Mode::Visual) => (
                "VISUAL".into(),
                theme_colors.vim_visual_background,
                theme_colors.vim_visual_foreground,
            ),
            Some(Mode::VisualLine) => (
                "V-LINE".into(),
                theme_colors.vim_visual_line_background,
                theme_colors.vim_visual_line_foreground,
            ),
            Some(Mode::VisualBlock) => (
                "V-BLOCK".into(),
                theme_colors.vim_visual_block_background,
                theme_colors.vim_visual_block_foreground,
            ),
            Some(Mode::HelixNormal) => (
                "HELIX".into(),
                theme_colors.vim_helix_normal_background,
                theme_colors.vim_helix_normal_foreground,
            ),
            Some(Mode::HelixSelect) => (
                "SELECT".into(),
                theme_colors.vim_helix_select_background,
                theme_colors.vim_helix_select_foreground,
            ),
            // Normal mode, and the pre-vim fallback so the bar is never blank.
            _ => (
                "NORMAL".into(),
                theme_colors.vim_normal_background,
                theme_colors.vim_normal_foreground,
            ),
        }
    }

    fn branch_name(&self, cx: &App) -> Option<SharedString> {
        let workspace = self.workspace.upgrade()?;
        let repository = workspace
            .read(cx)
            .project()
            .read(cx)
            .git_store()
            .read(cx)
            .active_repository()?;
        let name = repository.read(cx).branch.as_ref()?.name().to_string();
        Some(SharedString::from(truncate_and_trailoff(
            &name,
            MAX_SEGMENT_TEXT,
        )))
    }

    /// Aggregate agent counts across ALL windows: blocked in red, working in
    /// green, idle dimmed. None when no agents are registered.
    fn agent_segment(&self, cx: &App) -> Option<gpui::AnyElement> {
        let registry = AgentRegistry::global(cx)?;
        let registry = registry.read(cx);
        let blocked = registry.count_with_status(AgentStatus::Blocked);
        let working = registry.count_with_status(AgentStatus::Working);
        let idle = registry.count_with_status(AgentStatus::Idle);
        if blocked + working + idle == 0 {
            return None;
        }
        let mut segment = h_flex()
            .h_full()
            .px(px(10.0))
            .gap(px(8.0))
            .bg(seg3_background());
        if blocked > 0 {
            segment = segment.child(
                div()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(agent_blocked_color())
                    .child(SharedString::from(format!("◉ {blocked}"))),
            );
        }
        if working > 0 {
            segment = segment.child(
                div()
                    .text_color(agent_working_color())
                    .child(SharedString::from(format!("● {working}"))),
            );
        }
        if idle > 0 {
            segment = segment.child(
                div()
                    .text_color(dim_text_color())
                    .child(SharedString::from(format!("○ {idle}"))),
            );
        }
        Some(segment.into_any_element())
    }

    fn window_number(&self, window: &Window, cx: &App) -> usize {
        let window_handle = window.window_handle();
        cx.windows()
            .iter()
            .position(|handle| *handle == window_handle)
            .map_or(1, |index| index + 1)
    }
}

fn scroll_percent_label(scroll_y: f64, max_row: f64, visible_lines: Option<f64>) -> SharedString {
    let Some(visible_lines) = visible_lines else {
        return "All".into();
    };
    if max_row + 1.0 <= visible_lines {
        "All".into()
    } else if scroll_y <= 0.0 {
        "Top".into()
    } else if scroll_y + visible_lines >= max_row + 1.0 {
        "Bot".into()
    } else {
        let denominator = (max_row + 1.0 - visible_lines).max(1.0);
        SharedString::from(format!(
            "{}%",
            ((scroll_y / denominator) * 100.0).round() as u32
        ))
    }
}

/// A powerline arrow pointing right: a triangle of the previous segment's
/// color drawn over the next segment's background.
fn arrow_right(previous: Hsla, next: Hsla) -> impl IntoElement {
    div().w(px(ARROW_WIDTH)).h_full().bg(next).child(
        canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                let mut builder = PathBuilder::fill();
                builder.move_to(bounds.origin);
                builder.line_to(point(
                    bounds.origin.x + bounds.size.width,
                    bounds.origin.y + bounds.size.height / 2.0,
                ));
                builder.line_to(point(bounds.origin.x, bounds.origin.y + bounds.size.height));
                builder.close();
                if let Ok(path) = builder.build() {
                    window.paint_path(path, previous);
                }
            },
        )
        .size_full(),
    )
}

/// A powerline arrow pointing left: a triangle of the next (inner) segment's
/// color drawn over the previous (outer) segment's background.
fn arrow_left(previous: Hsla, next: Hsla) -> impl IntoElement {
    div().w(px(ARROW_WIDTH)).h_full().bg(previous).child(
        canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                let mut builder = PathBuilder::fill();
                builder.move_to(point(bounds.origin.x + bounds.size.width, bounds.origin.y));
                builder.line_to(point(
                    bounds.origin.x,
                    bounds.origin.y + bounds.size.height / 2.0,
                ));
                builder.line_to(point(
                    bounds.origin.x + bounds.size.width,
                    bounds.origin.y + bounds.size.height,
                ));
                builder.close();
                if let Ok(path) = builder.build() {
                    window.paint_path(path, next);
                }
            },
        )
        .size_full(),
    )
}

fn segment(background: Hsla, foreground: Hsla, text: SharedString) -> impl IntoElement {
    h_flex()
        .h_full()
        .px(px(10.0))
        .bg(background)
        .text_color(foreground)
        .child(text)
}

impl Render for Spaceline {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (mode_text, state_background, state_foreground) = self.block_state(cx);
        let bar_background = cx.theme().colors().status_bar_background;
        let buffer_font = ThemeSettings::get_global(cx).buffer_font.family.clone();
        let window_number = self.window_number(window, cx);

        let mut left = h_flex().h_full();

        // State block: circle badge with the window number + bold mode text.
        left = left.child(
            h_flex()
                .h_full()
                .px(px(10.0))
                .gap(px(7.0))
                .bg(state_background)
                .child(
                    div()
                        .size(px(BADGE_SIZE))
                        .rounded_full()
                        .bg(on_state_color())
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(11.0))
                        .text_color(state_background)
                        .child(SharedString::from(window_number.to_string())),
                )
                .child(
                    div()
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(state_foreground)
                        .child(mode_text),
                ),
        );

        let item_name = self.item_name.clone().map(|name| {
            SharedString::from(truncate_and_trailoff(name.as_ref(), MAX_SEGMENT_TEXT))
        });
        let branch_name = self.branch_name(cx);

        // Left chain: state -> buffer name -> branch -> bar background.
        match (&item_name, &branch_name) {
            (Some(name), Some(branch)) => {
                left = left
                    .child(arrow_right(state_background, seg2_background()))
                    .child(
                        h_flex()
                            .h_full()
                            .px(px(10.0))
                            .bg(seg2_background())
                            .text_color(segment_text_color())
                            .child(name.clone())
                            .when(self.item_dirty, |this| {
                                this.child(
                                    div()
                                        .pl(px(6.0))
                                        .text_color(dim_text_color())
                                        .child("[+]"),
                                )
                            }),
                    )
                    .child(arrow_right(seg2_background(), seg3_background()))
                    .child(
                        h_flex()
                            .h_full()
                            .px(px(10.0))
                            .gap(px(4.0))
                            .bg(seg3_background())
                            .text_color(segment_text_color())
                            .child(
                                Icon::new(IconName::GitBranch)
                                    .size(IconSize::XSmall)
                                    .color(Color::Custom(segment_text_color())),
                            )
                            .child(branch.clone()),
                    )
                    .child(arrow_right(seg3_background(), bar_background));
            }
            (Some(name), None) => {
                left = left
                    .child(arrow_right(state_background, seg2_background()))
                    .child(
                        h_flex()
                            .h_full()
                            .px(px(10.0))
                            .bg(seg2_background())
                            .text_color(segment_text_color())
                            .child(name.clone())
                            .when(self.item_dirty, |this| {
                                this.child(
                                    div()
                                        .pl(px(6.0))
                                        .text_color(dim_text_color())
                                        .child("[+]"),
                                )
                            }),
                    )
                    .child(arrow_right(seg2_background(), bar_background));
            }
            (None, _) => {
                left = left.child(arrow_right(state_background, bar_background));
            }
        }

        // Right chain, inner to outer: agents -> language -> line:col ->
        // scroll percent. `previous_background` threads the arrow colors.
        let mut right = h_flex().h_full();
        let mut previous_background = bar_background;

        if let Some(agent_segment) = self.agent_segment(cx) {
            right = right
                .child(arrow_left(previous_background, seg3_background()))
                .child(agent_segment);
            previous_background = seg3_background();
        }

        match &self.active_item {
            ActiveItem::Editor => {
                if let Some(language_name) = &self.language_name {
                    right = right
                        .child(arrow_left(previous_background, seg3_background()))
                        .child(segment(
                            seg3_background(),
                            segment_text_color(),
                            language_name.clone(),
                        ));
                    previous_background = seg3_background();
                }
                if let Some((line, column)) = self.cursor_position {
                    right = right
                        .child(arrow_left(previous_background, seg2_background()))
                        .child(segment(
                            seg2_background(),
                            segment_text_color(),
                            SharedString::from(format!("{line}:{column}")),
                        ));
                    previous_background = seg2_background();
                }
                if let Some(scroll_percent) = &self.scroll_percent {
                    right = right
                        .child(arrow_left(previous_background, state_background))
                        .child(
                            h_flex()
                                .h_full()
                                .px(px(10.0))
                                .bg(state_background)
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(state_foreground)
                                .child(scroll_percent.clone()),
                        );
                }
            }
            ActiveItem::Terminal(terminal_view) => {
                if let Some(terminal_view) = terminal_view.upgrade() {
                    let directory_name = terminal_view
                        .read(cx)
                        .terminal()
                        .read(cx)
                        .working_directory()
                        .and_then(|path| {
                            path.file_name()
                                .map(|name| name.to_string_lossy().to_string())
                        });
                    if let Some(directory_name) = directory_name {
                        right = right
                            .child(arrow_left(previous_background, state_background))
                            .child(
                                h_flex()
                                    .h_full()
                                    .px(px(10.0))
                                    .bg(state_background)
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(state_foreground)
                                    .child(SharedString::from(directory_name)),
                            );
                    }
                }
            }
            ActiveItem::Other | ActiveItem::None => {}
        }

        h_flex()
            .w_full()
            .h(px(BAR_HEIGHT))
            .justify_between()
            .font_family(buffer_font)
            .text_size(px(12.0))
            .child(left)
            .child(right)
    }
}

impl StatusItemView for Spaceline {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._item_subscriptions.clear();
        self.cursor_position = None;
        self.scroll_percent = None;
        self.language_name = None;
        self.item_name = active_pane_item.map(|item| item.tab_content_text(0, cx));
        self.item_dirty = active_pane_item.is_some_and(|item| item.is_dirty(cx));

        if let Some(item) = active_pane_item {
            if let Some(editor) = item.act_as::<Editor>(cx) {
                self._item_subscriptions.push(cx.subscribe(
                    &editor,
                    |this, editor, event: &EditorEvent, cx| match event {
                        EditorEvent::SelectionsChanged { .. }
                        | EditorEvent::ScrollPositionChanged { .. }
                        | EditorEvent::BufferEdited
                        | EditorEvent::Saved => this.refresh_editor_state(&editor, cx),
                        _ => {}
                    },
                ));
                // Covers dirty-flag and language changes that don't emit the
                // events above.
                self._item_subscriptions
                    .push(cx.observe(&editor, |this, editor, cx| {
                        this.item_dirty = editor.read(cx).buffer().read(cx).is_dirty(cx);
                        cx.notify();
                    }));
                self.active_item = ActiveItem::Editor;
                self.refresh_editor_state(&editor, cx);
            } else if let Some(terminal_view) = item.act_as::<TerminalView>(cx) {
                self._item_subscriptions
                    .push(cx.observe(&terminal_view, |_, _, cx| cx.notify()));
                self.active_item = ActiveItem::Terminal(terminal_view.downgrade());
            } else {
                self.active_item = ActiveItem::Other;
            }
        } else {
            self.active_item = ActiveItem::None;
        }
        cx.notify();
    }
}
