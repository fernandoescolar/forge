//! Finding text in a conversation (⌘F): the pane's find bar searches your messages, the
//! agent's answers and its thoughts, highlights what it finds and scrolls to each match.
//! Tool calls and cards are left out: what they show is mostly the agent's output.

use std::ops::Range;
use std::sync::Arc;

use gpui::{AppContext as _, Context, Entity, Task, Window};
use markdown::Markdown;
use project::search::SearchQuery;
use workspace::searchable::{Direction, SearchEvent, SearchOptions, SearchToken, SearchableItem};
use settings::SeedQuerySetting;

use super::ThreadView;
use super::turns::Row;
use crate::thread::{Entry, Status};

/// A match: the entry it is in and where in that entry's Markdown source.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadMatch {
    pub(super) entry: usize,
    pub(super) range: Range<usize>,
}

impl ThreadView {
    /// The entries the find bar searches, in order, with their Markdown.
    fn searchable(&mut self, cx: &mut Context<Self>) -> Vec<(usize, Entity<Markdown>)> {
        let count = self.thread.read(cx).entries.len();
        (0..count)
            .filter_map(|ix| match self.thread.read(cx).entries.get(ix)? {
                Entry::Agent(md) | Entry::Thought(md) => Some((ix, md.clone())),
                Entry::User(..) => self.user_markdown(ix, cx).map(|md| (ix, md)),
                _ => None,
            })
            .collect()
    }

    /// Shows `matches` highlighted in their entries, the `active` one apart.
    fn highlight(&mut self, matches: &[ThreadMatch], active: Option<usize>, cx: &mut Context<Self>) {
        let active = active.and_then(|ix| matches.get(ix));
        for (entry, md) in self.searchable(cx) {
            let ranges: Vec<Range<usize>> = matches.iter().filter(|m| m.entry == entry).map(|m| m.range.clone()).collect();
            let active_here = active.filter(|m| m.entry == entry).and_then(|a| ranges.iter().position(|r| *r == a.range));
            md.update(cx, |md, cx| {
                if md.search_highlights() != ranges.as_slice() || md.active_search_highlight() != active_here {
                    md.set_search_highlights(ranges, active_here, cx);
                }
            });
        }
    }
}

/// `matches` without those in source the Markdown doesn't show (sorted, disjoint ranges).
fn shown(matches: Vec<Range<usize>>, hidden: &[Range<usize>]) -> Vec<Range<usize>> {
    matches
        .into_iter()
        .filter(|m| {
            let candidate = hidden.partition_point(|h| h.end <= m.start);
            !hidden.get(candidate).is_some_and(|h| h.start < m.end)
        })
        .collect()
}

impl SearchableItem for ThreadView {
    type Match = ThreadMatch;

    fn supported_options(&self) -> SearchOptions {
        SearchOptions { case: true, word: true, regex: true, replacement: false, selection: false, select_all: false, find_in_results: false }
    }

    fn clear_matches(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let had = self.searchable(cx).iter().any(|(_, md)| !md.read(cx).search_highlights().is_empty());
        self.highlight(&[], None, cx);
        if had {
            cx.emit(SearchEvent::MatchesInvalidated);
        }
    }

    fn update_matches(&mut self, matches: &[ThreadMatch], active: Option<usize>, _: SearchToken, _: &mut Window, cx: &mut Context<Self>) {
        self.highlight(matches, active, cx);
    }

    fn query_suggestion(&mut self, _: Option<SeedQuerySetting>, _: &mut Window, cx: &mut Context<Self>) -> String {
        self.searchable(cx).iter().find_map(|(_, md)| md.read(cx).selected_source().map(str::to_string)).unwrap_or_default()
    }

    fn activate_match(&mut self, index: usize, matches: &[ThreadMatch], _: SearchToken, _: &mut Window, cx: &mut Context<Self>) {
        let Some(found) = matches.get(index) else { return };
        self.highlight(matches, Some(index), cx);
        let busy = self.thread.read(cx).status() == Status::Busy;
        let row = self.rows.iter().position(|row| match row {
            Row::Preamble(ix) | Row::User(ix) | Row::Entry(ix) => *ix == found.entry,
            Row::Thought { ix, .. } => *ix == found.entry,
            _ => false,
        });
        // A folded thought opens to show the match.
        if let Some(Row::Thought { ix, latest }) = row.and_then(|r| self.rows.get(r)) {
            let key = format!("thought-{ix}");
            if (*latest && busy) == self.toggled.contains(&key) {
                self.toggle(&key, cx);
            }
        }
        if let Some(row) = row {
            self.list.scroll_to_reveal_item(row);
        }
        // Within a long message, down to the match (on the next paint).
        if let Some((_, md)) = self.searchable(cx).into_iter().find(|(entry, _)| *entry == found.entry) {
            md.update(cx, |md, cx| md.request_autoscroll_to_source_index(found.range.start, cx));
        }
        cx.notify();
    }

    fn select_matches(&mut self, _: &[ThreadMatch], _: SearchToken, _: &mut Window, _: &mut Context<Self>) {}

    fn replace(&mut self, _: &ThreadMatch, _: &SearchQuery, _: SearchToken, _: &mut Window, _: &mut Context<Self>) {}

    fn find_matches(&mut self, query: Arc<SearchQuery>, _: &mut Window, cx: &mut Context<Self>) -> Task<Vec<ThreadMatch>> {
        let sources: Vec<(usize, String, Vec<Range<usize>>)> = self
            .searchable(cx)
            .into_iter()
            .map(|(entry, md)| {
                let md = md.read(cx);
                (entry, md.source().to_string(), md.non_rendered_source_ranges())
            })
            .collect();
        cx.background_spawn(async move {
            sources
                .into_iter()
                .flat_map(|(entry, source, hidden)| {
                    let mut ranges = shown(query.search_str(&source), &hidden);
                    ranges.sort_by_key(|r| (r.start, r.end));
                    ranges.into_iter().map(move |range| ThreadMatch { entry, range })
                })
                .collect()
        })
    }

    fn active_match_index(&mut self, direction: Direction, matches: &[ThreadMatch], _: SearchToken, _: &mut Window, cx: &mut Context<Self>) -> Option<usize> {
        if matches.is_empty() {
            return None;
        }
        // From the active match, if any; else from the top of what is on screen.
        let current = self.searchable(cx).into_iter().find_map(|(entry, md)| {
            let md = md.read(cx);
            md.active_search_highlight().and_then(|ix| md.search_highlights().get(ix)).map(|r| (entry, r.start))
        });
        let current = current.unwrap_or_else(|| {
            let top = self.list.logical_scroll_top().item_ix;
            let entry = self.rows.get(top..).into_iter().flatten().find_map(|row| match row {
                Row::Preamble(ix) | Row::User(ix) | Row::Entry(ix) | Row::Thought { ix, .. } => Some(*ix),
                _ => None,
            });
            (entry.unwrap_or(0), 0)
        });
        let key = |m: &ThreadMatch| (m.entry, m.range.start);
        match direction {
            Direction::Next => matches.iter().position(|m| key(m) >= current).or(Some(0)),
            Direction::Prev => matches.iter().rposition(|m| key(m) <= current).or(Some(matches.len() - 1)),
        }
    }
}

impl gpui::EventEmitter<SearchEvent> for ThreadView {}
