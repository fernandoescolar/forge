//! Code coverage in the editors: after a run with coverage, the gutter of each covered file
//! marks the lines that ran (green), the ones that ran without taking every branch
//! (yellow) and the ones that never ran (red). Editors opened later get the marks too;
//! they follow edits until the next run, or until the coverage is cleared.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use editor::{Anchor, Editor};
use gpui::{App, Context, Entity, Global};
use language::{Bias, Point};
use theme::ActiveTheme as _;
use workspace::Workspace;

use crate::cobertura::{Coverage, LineState};

#[derive(Default)]
struct Shown(Option<Arc<Coverage>>);

impl Global for Shown {}

enum CoveredLines {}
enum PartialLines {}
enum UncoveredLines {}

pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, _, cx| {
        if cx.try_global::<Shown>().is_some_and(|shown| shown.0.is_some()) {
            mark(editor, cx);
        }
    })
    .detach();
}

/// Shows `coverage` (or, with `None`, no coverage) in the workspace's editors and in those
/// opened later.
pub fn show(coverage: Option<Arc<Coverage>>, workspace: Option<&Entity<Workspace>>, cx: &mut App) {
    cx.set_global(Shown(coverage));
    let Some(workspace) = workspace.map(|w| w.downgrade()) else { return };
    // Runs end inside the panel's update: mark the editors after it.
    cx.defer(move |cx| {
        let Some(workspace) = workspace.upgrade() else { return };
        let editors: Vec<_> = workspace.read(cx).items_of_type::<Editor>(cx).collect();
        for editor in editors {
            editor.update(cx, |editor, cx| mark(editor, cx));
        }
    });
}

/// The percentage of lines covered in `file`, when the shown coverage has it.
pub fn percent_for(file: &std::path::Path, cx: &App) -> Option<f64> {
    cx.try_global::<Shown>()?.0.as_ref()?.percent(Some(file))
}

fn mark(editor: &mut Editor, cx: &mut Context<Editor>) {
    editor.clear_gutter_highlights::<CoveredLines>(cx);
    editor.clear_gutter_highlights::<PartialLines>(cx);
    editor.clear_gutter_highlights::<UncoveredLines>(cx);
    let Some(coverage) = cx.try_global::<Shown>().and_then(|shown| shown.0.clone()) else {
        cx.notify();
        return;
    };
    let Some(path) = file_path(editor, cx) else { return };
    let Some(lines) = coverage.files.get(&path) else {
        cx.notify();
        return;
    };
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let max_row = snapshot.max_point().row;
    let mut ranges: [Vec<Range<Anchor>>; 3] = Default::default();
    for (line, coverage) in lines {
        let row = line.saturating_sub(1);
        if row > max_row {
            continue;
        }
        let index = match coverage.state() {
            LineState::Covered => 0,
            LineState::Partial => 1,
            LineState::Uncovered => 2,
        };
        let start = snapshot.anchor_before(Point::new(row, 0));
        let end = snapshot.anchor_after(snapshot.clip_point(Point::new(row, u32::MAX), Bias::Left));
        ranges[index].push(start..end);
    }
    let [covered, partial, uncovered] = ranges;
    editor.highlight_gutter::<CoveredLines>(covered, |cx| cx.theme().status().success, cx);
    editor.highlight_gutter::<PartialLines>(partial, |cx| cx.theme().status().warning, cx);
    editor.highlight_gutter::<UncoveredLines>(uncovered, |cx| cx.theme().status().error, cx);
    cx.notify();
}

/// The file an editor shows, for editors of a single file.
fn file_path(editor: &Editor, cx: &App) -> Option<PathBuf> {
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let file = buffer.read(cx).file()?.as_local()?.abs_path(cx);
    Some(file.canonicalize().unwrap_or(file))
}
