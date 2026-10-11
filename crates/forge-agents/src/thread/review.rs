//! The agent's writes: reviews waiting for the user, the changed files, checkpoints, and
//! keeping or undoing changes (a file, a turn, or since a message).

use super::*;

impl Thread {
    pub(super) fn add_review(&mut self, review: WriteReview, window: &mut Window, cx: &mut Context<Self>) {
        let edit = Edit { path: review.path.to_string_lossy().into_owned(), old_text: review.old_text, new_text: review.new_text };
        let hunks: Vec<(Hunk, bool)> = hunks(edit.old_text.as_deref().unwrap_or_default(), &edit.new_text).into_iter().map(|h| (h, true)).collect();
        let first_change = hunks.first().map(|(h, _): &(Hunk, bool)| h.new.start);
        self.location = Some(AgentLocation { path: review.path.clone(), line: first_change, kind: Some("edit".into()) });
        // Editable: the user can adjust the proposal before accepting it.
        let diff = DiffView::editable(edit, self.languages.clone(), window, cx);
        self.entries.push(Entry::Review { diff, hunks, reply: Some(review.reply), outcome: None });
        self.changed(cx);
    }

    pub(crate) fn answer_review(&mut self, ix: usize, accept: bool, cx: &mut Context<Self>) {
        let edited = match self.entries.get(ix) {
            Some(Entry::Review { diff, .. }) => diff.edited_text(cx),
            _ => None,
        };
        if let Some(Entry::Review { diff, hunks, reply, outcome }) = self.entries.get_mut(ix) {
            if let Some(reply) = reply.take() {
                let selected = hunks.iter().filter(|(_, on)| *on).count();
                let (decision, label) = match (accept, selected, edited) {
                    // The user rewrote the proposal: write what they left.
                    (true, _, Some(text)) => (ReviewDecision::Partial(text), "Applied with your edits"),
                    (false, _, _) | (true, 0, None) => (ReviewDecision::Reject, "Rejected"),
                    (true, n, None) if n == hunks.len() => (ReviewDecision::Accept, "Applied"),
                    (true, _, None) => {
                        let (h, on): (Vec<Hunk>, Vec<bool>) = hunks.iter().cloned().unzip();
                        (ReviewDecision::Partial(apply_hunks(diff.edit.old_text.as_deref().unwrap_or_default(), &diff.edit.new_text, &h, &on)), "Partially applied")
                    }
                };
                let _ = reply.send(decision);
                *outcome = Some(label);
                diff.set_read_only(cx);
            }
        }
        self.changed(cx);
    }

    /// Accepts or rejects every write waiting for review.
    pub fn answer_all_reviews(&mut self, accept: bool, cx: &mut Context<Self>) {
        let pending: Vec<usize> = self.entries.iter().enumerate().filter(|(_, e)| matches!(e, Entry::Review { reply: Some(_), .. })).map(|(i, _)| i).collect();
        for ix in pending {
            self.answer_review(ix, accept, cx);
        }
    }

    pub(crate) fn toggle_hunk(&mut self, ix: usize, hunk: usize, cx: &mut Context<Self>) {
        if let Some(Entry::Review { hunks, reply: Some(_), .. }) = self.entries.get_mut(ix) {
            if let Some((_, on)) = hunks.get_mut(hunk) {
                *on = !*on;
            }
        }
        self.changed(cx);
    }

    /// Another thread has unreviewed changes to `path`, which this one just wrote: both
    /// threads say so, once per file.
    fn warn_about_conflict(&mut self, path: &PathBuf, cx: &mut Context<Self>) {
        let me = cx.entity();
        let Some(other) = crate::agent_review::other_thread_changing(path, &me, cx) else { return };
        if !self.conflicts_warned.insert(path.clone()) {
            return;
        }
        let name = path.strip_prefix(&self.root).unwrap_or(path).to_string_lossy().into_owned();
        let other_title = other.read(cx).title().unwrap_or_else(|| "another thread".into());
        let my_title = self.title().unwrap_or_else(|| "another thread".into());
        self.system(
            format!("⚠ {name} also has changes from “{other_title}” you haven't reviewed. This thread's changes are on top of them: undoing them here puts the file back as it was before this thread."),
            Color::Warning,
        );
        let path = path.clone();
        other.update(cx, |other, cx| {
            if other.conflicts_warned.insert(path) {
                other.system(format!("⚠ “{my_title}” changed {name} too, on top of this thread's changes. Undoing them here also undoes that thread's."), Color::Warning);
                other.changed(cx);
            }
        });
    }

    /// A checkpoint for the message just added.
    pub(crate) fn begin_checkpoint(&mut self) {
        self.checkpoints.push(Checkpoint { entry: self.entries.len().saturating_sub(1), files: Default::default(), turn_files: Default::default(), undone: Default::default(), started: std::time::Instant::now(), duration: None });
    }

    /// The turn the message at `entry` started (its checkpoint), files changed or not.
    pub(crate) fn turn_at(&self, entry: usize) -> Option<&Checkpoint> {
        self.checkpoints.iter().find(|c| c.entry == entry)
    }

    /// The checkpoint taken when the message at `entry` was sent, if it has files to restore.
    pub(crate) fn checkpoint_at(&self, entry: usize) -> Option<&Checkpoint> {
        self.checkpoints.iter().find(|c| c.entry == entry).filter(|c| !c.files.is_empty())
    }

    /// The message at `entry` goes back to the input to be edited and sent again. When the
    /// agent changed files after it, the user chooses whether they go back as they were
    /// then too (the agent remembers the conversation either way).
    pub(crate) fn edit_message(&mut self, entry: usize, text: String, files: usize, window: &mut Window, cx: &mut Context<Self>) {
        if files == 0 {
            cx.emit(ThreadEvent::RestoreInput(text));
            return;
        }
        let detail = format!(
            "The agent changed {files} file{} after this message. Put {} back as {} then, before you send the new version?",
            if files == 1 { "" } else { "s" },
            if files == 1 { "it" } else { "them" },
            if files == 1 { "it was" } else { "they were" },
        );
        let answer = window.prompt(gpui::PromptLevel::Info, "Edit this message", Some(&detail), &["Restore files and edit", "Just edit", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            let choice = answer.await.ok();
            this.update(cx, |this, cx| {
                if choice == Some(0) {
                    this.restore_checkpoint(entry, cx);
                }
                if matches!(choice, Some(0 | 1)) {
                    cx.emit(ThreadEvent::RestoreInput(text));
                }
            })
            .ok();
        })
        .detach();
    }

    /// Puts every file the agent wrote since the message at `entry` back as it was then.
    /// The conversation stays; later checkpoints go (they describe a state that is gone).
    pub(crate) fn restore_checkpoint(&mut self, entry: usize, cx: &mut Context<Self>) {
        if self.status == Status::Busy {
            self.system("Stop the agent before restoring a checkpoint.", Color::Warning);
            return;
        }
        let Some(ix) = self.checkpoints.iter().position(|c| c.entry == entry) else { return };
        let files: Vec<(PathBuf, Option<String>)> = self.checkpoints[ix].files.clone().into_iter().collect();
        let message = match self.entries.get(entry) {
            Some(Entry::User(text, _)) => text.lines().next().unwrap_or_default().chars().take(60).collect::<String>(),
            _ => String::new(),
        };
        self.checkpoints.truncate(ix + 1);
        self.checkpoints[ix].files.clear();
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            let mut failed = Vec::new();
            for (path, content) in &files {
                if let Err(e) = crate::project_fs::restore(&project, path, content.clone(), cx).await {
                    failed.push(format!("{}: {e:#}", path.display()));
                }
            }
            this.update(cx, |this, cx| {
                for (path, content) in &files {
                    if let Some(change) = this.changes.iter_mut().find(|c| &c.path == path) {
                        change.current = content.clone().unwrap_or_default();
                    }
                }
                this.changes.retain(|c| c.original.as_deref() != Some(c.current.as_str()) && !(c.original.is_none() && c.current.is_empty()));
                let n = files.len() - failed.len();
                this.system(format!("Restored {n} file{} to how {} before “{message}”.", if n == 1 { "" } else { "s" }, if n == 1 { "it was" } else { "they were" }), Color::Muted);
                for f in failed {
                    this.system(format!("Could not restore {f}"), Color::Error);
                }
                this.changes_updated(cx);
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn record_write(&mut self, record: WriteRecord, cx: &mut Context<Self>) {
        self.warn_about_conflict(&record.path, cx);
        // Right after the write the language servers haven't looked at it yet: the problems
        // the file shows are still the ones it had before.
        if !self.problem_baseline.contains_key(&record.path) {
            let before = match (&record.problems_before, &record.old_text) {
                (Some(problems), _) => problems.clone(),
                (None, None) => Vec::new(),
                // Written without Forge's file system (language-server edits, agents that
                // write files themselves): the open buffer's, if any.
                (None, Some(_)) => self.project.upgrade().and_then(|p| crate::verify::open_file_problems(p.read(cx), &record.path, cx)).unwrap_or_default(),
            };
            self.problem_baseline.insert(record.path.clone(), before);
        }
        for checkpoint in &mut self.checkpoints {
            checkpoint.files.entry(record.path.clone()).or_insert_with(|| record.old_text.clone());
        }
        if let Some(turn) = self.checkpoints.last_mut().filter(|t| t.duration.is_none()) {
            let file = turn.turn_files.entry(record.path.clone()).or_insert_with(|| (record.old_text.clone(), String::new()));
            file.1 = record.new_text.clone();
            // Written back as it was before the turn: nothing changed in it.
            if file.0.as_deref() == Some(file.1.as_str()) {
                turn.turn_files.remove(&record.path);
            }
        }
        match self.changes.iter().position(|c| c.path == record.path) {
            Some(ix) => self.changes[ix].current = record.new_text,
            None => self.changes.push(ChangedFile { path: record.path, original: record.old_text, current: record.new_text }),
        }
        // Back to how it was: nothing left to review.
        self.changes.retain(|c| c.original.as_deref() != Some(c.current.as_str()));
        self.changes_updated(cx);
        self.changed(cx);
    }

    /// `change`'s (added, removed) lines, as counted when the files last changed.
    pub(crate) fn change_stats(&self, change: &ChangedFile) -> (usize, usize) {
        self.change_stats.get(&change.path).copied().unwrap_or_else(|| change.stats())
    }

    /// The changed files changed: views and editors showing them follow.
    pub(crate) fn changes_updated(&mut self, cx: &mut Context<Self>) {
        self.change_stats = self.changes.iter().map(|c| (c.path.clone(), c.stats())).collect();
        cx.emit(ThreadEvent::ChangesUpdated);
        let this = cx.entity();
        cx.defer(move |cx| crate::agent_review::thread_changed(&this, cx));
    }

    /// Part of a change was kept or undone in an editor: `baseline` is what the file is
    /// now compared against, `current` its content.
    pub(crate) fn set_change_baseline(&mut self, path: &std::path::Path, baseline: String, current: String, cx: &mut Context<Self>) {
        if let Some(change) = self.changes.iter_mut().find(|c| c.path == path) {
            change.original = Some(baseline);
            change.current = current;
        }
        self.changes.retain(|c| c.original.as_deref() != Some(c.current.as_str()));
        self.changes_updated(cx);
        self.changed(cx);
    }

    /// Accepts the agent's changes to `path` (all files when `None`): they stop being
    /// listed and can no longer be undone from the thread.
    pub(crate) fn keep_changes(&mut self, path: Option<&std::path::Path>, cx: &mut Context<Self>) {
        self.changes.retain(|c| path.is_some_and(|p| p != c.path));
        self.changes_updated(cx);
        self.changed(cx);
    }

    /// The files of the turn started by the message at `entry` (`path` alone if given, all
    /// of them otherwise) that a later turn changed again: undoing this turn loses those
    /// changes too.
    pub(crate) fn later_turns_touching(&self, entry: usize, path: Option<&std::path::Path>) -> Vec<PathBuf> {
        let Some(ix) = self.checkpoints.iter().position(|c| c.entry == entry) else { return vec![] };
        let turn = &self.checkpoints[ix];
        let paths: Vec<&PathBuf> = turn.turn_files.keys().filter(|p| !turn.undone.contains(*p) && path.is_none_or(|only| only == p.as_path())).collect();
        paths.into_iter().filter(|p| self.checkpoints[ix + 1..].iter().any(|later| later.turn_files.contains_key(*p) && !later.undone.contains(*p))).cloned().collect()
    }

    /// Rolls back the turn started by the message at `entry`: its files (`path` alone if
    /// given) go back as they were before the turn. The conversation stays.
    pub(crate) fn undo_turn(&mut self, entry: usize, path: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.status == Status::Busy {
            self.system("Stop the agent before undoing a turn.", Color::Warning);
            self.changed(cx);
            return;
        }
        let Some(turn) = self.checkpoints.iter().find(|c| c.entry == entry) else { return };
        let files: Vec<(PathBuf, Option<String>)> =
            turn.turn_files.iter().filter(|(p, _)| !turn.undone.contains(*p) && path.as_ref().is_none_or(|only| only == *p)).map(|(p, (before, _))| (p.clone(), before.clone())).collect();
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            let mut restored = Vec::new();
            let mut failed = Vec::new();
            for (path, before) in &files {
                match crate::project_fs::restore(&project, path, before.clone(), cx).await {
                    Ok(()) => restored.push((path.clone(), before.clone())),
                    Err(e) => failed.push(format!("{}: {e:#}", path.display())),
                }
            }
            this.update(cx, |this, cx| {
                let from = this.checkpoints.iter().position(|c| c.entry == entry).unwrap_or(this.checkpoints.len());
                for (path, before) in &restored {
                    // This turn and the later ones that changed it again are undone for it.
                    for turn in &mut this.checkpoints[from..] {
                        if turn.turn_files.contains_key(path) {
                            turn.undone.insert(path.clone());
                        }
                    }
                    if let Some(change) = this.changes.iter_mut().find(|c| &c.path == path) {
                        change.current = before.clone().unwrap_or_default();
                    }
                }
                this.changes.retain(|c| c.original.as_deref() != Some(c.current.as_str()) && !(c.original.is_none() && c.current.is_empty()));
                let n = restored.len();
                if n > 0 {
                    let what = match &restored[..] {
                        [(path, _)] => path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                        _ => format!("{n} files"),
                    };
                    this.system(format!("Undid this turn's changes to {what}."), Color::Muted);
                }
                for f in failed {
                    this.system(format!("Could not undo {f}"), Color::Error);
                }
                this.changes_updated(cx);
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Puts `path` (all changed files when `None`) back as it was before the agent.
    pub(crate) fn undo_changes(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        let undo: Vec<ChangedFile> = self.changes.iter().filter(|c| path.as_ref().is_none_or(|p| *p == c.path)).cloned().collect();
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            for change in undo {
                let result = crate::project_fs::restore(&project, &change.path, change.original.clone(), cx).await;
                this.update(cx, |this, cx| {
                    match result {
                        Ok(()) => {
                            this.changes.retain(|c| c.path != change.path);
                            let name = change.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                            this.system(format!("Undid the agent's changes to {name}."), Color::Muted);
                        }
                        Err(e) => this.system(format!("Could not undo the changes to {}: {e:#}", change.path.display()), Color::Error),
                    }
                    this.changes_updated(cx);
                    this.changed(cx);
                })
                .ok();
            }
        })
        .detach();
    }

    /// Records the files a finished edit tool call changed on disk (see `apply_update`).
    pub(super) fn record_tool_edits(&mut self, tool_call_id: &str, cx: &mut Context<Self>) {
        // Each file's diff: before and after, when the tool call carried whole files.
        let edits: Vec<(PathBuf, Option<String>, String)> = self
            .entries
            .iter()
            .find_map(|e| match e {
                Entry::Tool { id, diffs, .. } if id == tool_call_id => Some(diffs.iter().map(|d| (PathBuf::from(&d.edit.path), d.edit.old_text.clone(), d.edit.new_text.clone())).collect()),
                _ => None,
            })
            .unwrap_or_default();
        if edits.is_empty() {
            return;
        }
        let fs = <dyn fs::Fs>::global(cx);
        cx.spawn(async move |this, cx| {
            let mut now = Vec::new();
            for (path, old_text, new_text) in edits {
                now.push((fs.load(&path).await.ok(), path, old_text, new_text));
            }
            this.update(cx, |this, cx| {
                // A tool call may change a file more than once (several snippets of it).
                let mut by_path: Vec<(PathBuf, Option<String>, Vec<(Option<String>, String)>)> = Vec::new();
                for (on_disk, path, old_text, new_text) in now {
                    match by_path.iter_mut().find(|(p, _, _)| *p == path) {
                        Some((_, _, diffs)) => diffs.push((old_text, new_text)),
                        None => by_path.push((path, on_disk, vec![(old_text, new_text)])),
                    }
                }
                for (path, on_disk, diffs) in by_path {
                    // Deleted or unreadable: nothing to show.
                    let Some(on_disk) = on_disk else { continue };
                    // Already recorded with this content (it went through `ProjectFs`).
                    if this.changes.iter().any(|c| c.path == path && c.current == on_disk) {
                        continue;
                    }
                    let Some(old_text) = file_before(&on_disk, &diffs) else { continue };
                    if old_text.as_deref() == Some(on_disk.as_str()) {
                        continue;
                    }
                    this.record_write(WriteRecord { path, old_text, new_text: on_disk, problems_before: None }, cx);
                }
            })
            .ok();
        })
        .detach();
    }
}
