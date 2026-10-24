//! Dock buttons with drag & drop.
//!
//! Replaces Zed's status-bar panel buttons with Forge's own, each next to the dock it
//! controls: a vertical strip along the left edge of the window for the left dock, one
//! along the right edge for the right dock, and a row in the status bar for the bottom
//! dock. Clicking a button opens its panel, or hides the dock when that panel is the one
//! showing; right-click moves it to another dock, and every icon can be dragged.
//! While a panel or an extension tab is being dragged, drop zones appear along the left,
//! right and bottom edges of the window.
//!
//! Panels can also be shown together in one dock ([`PanelGroups`]): drop a panel's button
//! on the middle of another's, or use "Show with" in the button's menu. Dropped on a
//! button's edge (or past the last one), it goes there instead, so the buttons can be put
//! in any order ([`PanelOrder`]).

use forge_extension_host::{ExtensionHost, panel::SLOT_KEYS};
use forge_ui::{DraggedExtensionTab, DraggedPanel};
use gpui::{
    AnchoredPositionMode, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, MouseUpEvent, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window, anchored, canvas, deferred, div, point, px,
};
use std::{rc::Rc, sync::Arc};
use theme::ActiveTheme as _;
use gpui::prelude::FluentBuilder as _;
use ui::{ButtonCommon as _, ButtonLike, ButtonSize, Clickable as _, Color, ContextMenu, Icon, IconPosition, IconSize, Label, LabelCommon as _, LabelSize, Toggleable as _, Tooltip, h_flex, right_click_menu, v_flex};
use workspace::{
    ItemHandle, StatusItemView, Workspace,
    dock::{DockPosition, PanelButtons, PanelHandle},
};

pub use forge_extension_host::panel::DockEntry;

mod layout;
mod registry;
mod buttons;
pub(crate) use layout::*;
pub(crate) use registry::*;
pub(crate) use buttons::*;

#[cfg(test)]
mod tests;
