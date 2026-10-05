//! Editing project files (`.csproj`, `Directory.Packages.props`, …): NuGet package ids
//! and versions as completions, a hint after each package with a newer version, and code
//! actions to switch a package to another version.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::Result;
use dotnet_model::msbuild::text::{self as project_text, CompletionSite};
use dotnet_model::nuget::{PackageSource, version};
use editor::{CompletionProvider, Editor, Inlay, MultiBufferOffset};
use gpui::{App, AppContext as _, Context, Entity, Task, WeakEntity, Window};
use language::{Buffer, CodeLabel, ToOffset as _};
use project::{Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource, InlayHint, InlayHintLabel, InlayId, ResolveState};

use crate::config;

/// Inlay ids far above the ones language servers use.
const HINT_ID_BASE: usize = 1 << 48;

pub fn is_project_file(path: &Path) -> bool {
    let ext = dotnet_model::paths::extension(path);
    matches!(ext.as_str(), "csproj" | "fsproj" | "vbproj" | "props" | "targets" | "proj")
}

fn buffer_path(buffer: &Entity<Buffer>, cx: &App) -> Option<PathBuf> {
    let file = buffer.read(cx).file()?;
    Some(file.as_local()?.abs_path(cx))
}

fn sources(path: &Path) -> Vec<PackageSource> {
    dotnet_model::nuget::sources_for(path.parent().unwrap_or(Path::new(".")))
}

pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, _, cx| {
        if !editor.mode().is_full() {
            return;
        }
        let Some(buffer) = editor.buffer().read(cx).as_singleton() else { return };
        let Some(path) = buffer_path(&buffer, cx).filter(|p| is_project_file(p)) else { return };
        let config = config::get(cx).nuget;
        if config.completions {
            let fallback: Option<Rc<dyn CompletionProvider>> = editor.project().map(|p| Rc::new(p.clone()) as Rc<dyn CompletionProvider>);
            editor.set_completion_provider(Some(Rc::new(PackageCompletions { fallback, path: path.clone() })));
        }
        // ⌘. on a package reference (see the keymap in forge-native): its other versions.
        let this = cx.entity().downgrade();
        let version_path = path.clone();
        editor
            .register_action(move |_: &ChangePackageVersion, window, cx| {
                let path = version_path.clone();
                this.update(cx, |editor, cx| change_package_version(editor, path, window, cx)).ok();
            })
            .detach();
        if config.version_hints {
            let editor_handle = cx.weak_entity();
            let hints = cx.new(|cx| VersionHints::new(editor_handle, buffer.clone(), path, cx));
            editor.register_addon(HintsAddon { _hints: hints });
        }
    })
    .detach();
}

fn include_prerelease(cx: &App) -> bool {
    config::get(cx).nuget.include_prerelease
}

struct PackageCompletions {
    fallback: Option<Rc<dyn CompletionProvider>>,
    path: PathBuf,
}

impl CompletionProvider for PackageCompletions {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        trigger: editor::CompletionContext,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        let snapshot = buffer.read(cx).snapshot();
        let offset = position.to_offset(&snapshot);
        let text = snapshot.text();
        let Some((site, value_start)) = project_text::completion_site(&text, offset) else {
            return match &self.fallback {
                Some(fallback) => fallback.completions(buffer, position, trigger, window, cx),
                None => Task::ready(Ok(Vec::new())),
            };
        };
        let Some(client) = crate::fetch::client(cx) else { return Task::ready(Ok(Vec::new())) };
        let replace_range = snapshot.anchor_before(value_start)..position;
        let sources = sources(&self.path);
        let prerelease = include_prerelease(cx);
        cx.background_spawn(async move {
            let completion = |label: String, filter: &str, new_text: String, detail: Option<String>| Completion {
                replace_range: replace_range.clone(),
                label: CodeLabel::plain(label, Some(filter)),
                new_text,
                documentation: detail.map(|d| project::lsp_store::CompletionDocumentation::SingleLine(d.into())),
                source: CompletionSource::Custom,
                icon_path: None,
                icon_color: None,
                group: None,
                match_start: None,
                snippet_deduplication_key: None,
                insert_text_mode: None,
                confirm: None,
            };
            let completions: Vec<Completion> = match site {
                CompletionSite::PackageName { query } => {
                    if query.trim().is_empty() {
                        Vec::new()
                    } else {
                        let (results, _) = client.search(&sources, &query, prerelease, 0, 20).await;
                        results
                            .into_iter()
                            .map(|p| {
                                let detail = (!p.description.is_empty()).then(|| p.description.lines().next().unwrap_or("").to_string());
                                completion(format!("{}  {}", p.id, p.version), &p.id, p.id.clone(), detail)
                            })
                            .collect()
                    }
                }
                CompletionSite::Version { package, query } => {
                    let versions = client.versions(&sources, &package, prerelease || query.contains('-')).await;
                    versions
                        .into_iter()
                        .filter(|v| v.starts_with(query.trim()))
                        .enumerate()
                        .map(|(i, v)| completion(if i == 0 { format!("{v}  latest") } else { v.clone() }, &v, v.clone(), None))
                        .collect()
                }
            };
            Ok(vec![CompletionResponse { completions, display_options: CompletionDisplayOptions { dynamic_width: true }, is_incomplete: true }])
        })
    }

    fn is_completion_trigger(&self, buffer: &Entity<Buffer>, position: language::Anchor, typed: &str, trigger_in_words: bool, cx: &mut Context<Editor>) -> bool {
        let snapshot = buffer.read(cx).snapshot();
        let offset = position.to_offset(&snapshot);
        if project_text::completion_site(&snapshot.text(), offset).is_some() {
            return typed == "\"" || typed == "." || typed.chars().all(|c| c.is_alphanumeric() || c == '.' || c == '-');
        }
        match &self.fallback {
            Some(fallback) => fallback.is_completion_trigger(buffer, position, typed, trigger_in_words, cx),
            None => false,
        }
    }

    fn sort_completions(&self) -> bool {
        false
    }

    fn filter_completions(&self) -> bool {
        false
    }
}

gpui::actions!(forge_dotnet, [ChangePackageVersion]);

/// Lists the other versions of the package reference under the cursor and puts the
/// chosen one in the file.
fn change_package_version(editor: &mut Editor, path: PathBuf, window: &mut Window, cx: &mut Context<Editor>) {
    let Some(buffer) = editor.buffer().read(cx).as_singleton() else { return };
    let Some(workspace) = editor.workspace() else { return };
    let snapshot = buffer.read(cx).snapshot();
    let head = editor.selections.newest_anchor().head();
    let Some((_, at)) = editor.buffer().read(cx).text_anchor_for_position(head, cx) else { return };
    let offset = at.to_offset(&snapshot);
    let text = snapshot.text();
    let Some(reference) = project_text::package_references(&text).into_iter().find(|r| {
        let start = r.name_range.start.min(r.version_range.as_ref().map_or(usize::MAX, |v| v.start));
        let end = r.tag_end.max(r.version_range.as_ref().map_or(0, |v| v.end));
        (start.saturating_sub(40)..=end).contains(&offset)
    }) else {
        return;
    };
    let (Some(current), Some(version_range)) = (reference.version.clone(), reference.version_range.clone()) else { return };
    let Some(client) = crate::fetch::client(cx) else { return };
    let sources = sources(&path);
    let prerelease = include_prerelease(cx) || current.contains('-');
    let anchor_range = snapshot.anchor_before(version_range.start)..snapshot.anchor_after(version_range.end);
    let name = reference.name.clone();
    let lookup = cx.background_spawn(async move { client.versions(&sources, &name, prerelease).await });
    let workspace = workspace.downgrade();
    cx.spawn_in(window, async move |_, cx| {
        let versions = lookup.await;
        let latest = versions.first().cloned();
        let versions: Vec<String> = versions.into_iter().filter(|v| v != &current).take(20).collect();
        if versions.is_empty() {
            return;
        }
        let choices = versions
            .iter()
            .map(|v| {
                let detail = if Some(v) == latest.as_ref() {
                    "latest"
                } else if version::is_newer(v, &current) {
                    "update"
                } else {
                    "downgrade"
                };
                forge_ui::pick::Choice::new(v.clone()).detail(detail)
            })
            .collect();
        let placeholder = format!("{} {current} → which version?", reference.name);
        cx.update(|window, cx| {
            forge_ui::pick::defer_workspace(workspace, window, cx, move |workspace, window, cx| {
                forge_ui::pick::pick(workspace, &placeholder, choices, window, cx, move |ix, _, cx| {
                    let version = versions[ix].clone();
                    buffer.update(cx, |buffer, cx| buffer.edit([(anchor_range.clone(), version)], None, cx));
                });
            });
        })
        .ok();
    })
    .detach();
}

struct HintsAddon {
    _hints: Entity<VersionHints>,
}

impl editor::Addon for HintsAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Shows, after each package, whether a newer version exists.
struct VersionHints {
    editor: WeakEntity<Editor>,
    buffer: Entity<Buffer>,
    path: PathBuf,
    shown: Vec<InlayId>,
    task: Option<Task<()>>,
    _subscription: gpui::Subscription,
}

impl VersionHints {
    fn new(editor: WeakEntity<Editor>, buffer: Entity<Buffer>, path: PathBuf, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&buffer, |this, _, event: &language::BufferEvent, cx| {
            if matches!(event, language::BufferEvent::Edited { .. } | language::BufferEvent::Reloaded) {
                this.refresh(Duration::from_millis(800), cx);
            }
        });
        let mut hints = Self { editor, buffer, path, shown: Vec::new(), task: None, _subscription: subscription };
        hints.refresh(Duration::from_millis(50), cx);
        hints
    }

    fn refresh(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let Some(client) = crate::fetch::client(cx) else { return };
        let text = self.buffer.read(cx).text();
        let sources = sources(&self.path);
        let prerelease = include_prerelease(cx);
        self.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let references: Vec<_> = project_text::package_references(&text).into_iter().filter(|r| r.version.as_deref().is_some_and(|v| version::NuGetVersion::parse(v).is_some())).collect();
            let lookups = references.iter().map(|r| {
                let (client, sources) = (client.clone(), sources.clone());
                let prerelease = prerelease || r.version.as_deref().is_some_and(|v| v.contains('-'));
                async move { client.versions(&sources, &r.name, prerelease).await.into_iter().next() }
            });
            let latest = futures::future::join_all(lookups).await;
            this.update(cx, |this, cx| {
                // The text may have changed while looking versions up; only use fresh results.
                if this.buffer.read(cx).text() != text {
                    return;
                }
                let hints: Vec<(usize, String)> = references
                    .iter()
                    .zip(latest)
                    .filter_map(|(r, latest)| {
                        let latest = latest?;
                        let current = r.version.as_deref()?;
                        Some(if version::is_newer(&latest, current) { (r.tag_end, format!("  ⬆ {latest}")) } else { (r.tag_end, "  ✓".to_string()) })
                    })
                    .collect();
                this.show(hints, cx);
            })
            .ok();
        }));
    }

    fn show(&mut self, hints: Vec<(usize, String)>, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else { return };
        let old = std::mem::take(&mut self.shown);
        let mut new_ids = Vec::new();
        editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let buffer_snapshot = self.buffer.read(cx).snapshot();
            let inlays: Vec<Inlay> = hints
                .into_iter()
                .enumerate()
                .map(|(i, (offset, label))| {
                    let id = InlayId::Hint(HINT_ID_BASE + i);
                    new_ids.push(id);
                    let hint = InlayHint {
                        position: buffer_snapshot.anchor_after(offset),
                        label: InlayHintLabel::String(label),
                        kind: None,
                        padding_left: false,
                        padding_right: false,
                        tooltip: None,
                        resolve_state: ResolveState::Resolved,
                    };
                    Inlay::hint(id, snapshot.anchor_after(MultiBufferOffset(offset)), &hint)
                })
                .collect();
            editor.splice_inlays(&old, inlays, cx);
        });
        self.shown = new_ids;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_files() {
        assert!(is_project_file(Path::new("/a/App.csproj")));
        assert!(is_project_file(Path::new("/a/Directory.Packages.props")));
        assert!(!is_project_file(Path::new("/a/Program.cs")));
    }
}
