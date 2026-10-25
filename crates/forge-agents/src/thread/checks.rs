//! Checking the files a turn changed: their errors and warnings, kept current as the
//! language servers report, and sent back to the agent to fix.

use super::*;

impl Thread {
    pub(super) fn verify_enabled(&self, cx: &gpui::App) -> bool {
        crate::settings::try_global(cx).is_none_or(|s| s.read(cx).config().verify_changes)
    }

    /// After a turn that wrote files: their errors and warnings, once the language
    /// servers have caught up.
    pub(crate) fn check_changed_files(&mut self, cx: &mut Context<Self>) {
        let files: Vec<PathBuf> = self.checkpoints.last().map(|c| c.files.keys().cloned().collect()).unwrap_or_default();
        if files.is_empty() || !self.verify_enabled(cx) {
            return;
        }
        let ix = self.entries.len();
        self.entries.push(Entry::Check(None));
        let before = std::mem::take(&mut self.diagnostics_before);
        let task = crate::verify::check(self.project.clone(), files.clone(), before.clone(), cx);
        self.check_watch = None;
        cx.spawn(async move |this, cx| {
            let checks = task.await;
            this.update(cx, |this, cx| {
                if let Some(entry @ Entry::Check(None)) = this.entries.get_mut(ix) {
                    *entry = Entry::Check(Some(checks));
                }
                this.watch_check(ix, files, before, cx);
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Language servers may report on the files later (a slow first build): the check at
    /// `ix` follows their diagnostics until the next one starts.
    fn watch_check(&mut self, ix: usize, files: Vec<PathBuf>, before: HashMap<PathBuf, (usize, usize)>, cx: &mut Context<Self>) {
        let Some(project) = self.project.upgrade() else { return };
        self.check_watch = Some(cx.subscribe(&project, move |_, project, event: &project::Event, cx| {
            let project::Event::DiagnosticsUpdated { paths, .. } = event else { return };
            let touched = paths.iter().filter_map(|p| project.read(cx).absolute_path(p, cx)).any(|abs| files.contains(&abs));
            if !touched {
                return;
            }
            let (weak, files, before) = (project.downgrade(), files.clone(), before.clone());
            cx.spawn(async move |this, cx| {
                let checks = crate::verify::read_problems(weak, files, before, cx).await;
                this.update(cx, |this, cx| {
                    if let Some(entry @ Entry::Check(Some(_))) = this.entries.get_mut(ix) {
                        *entry = Entry::Check(Some(checks));
                        this.changed(cx);
                    }
                })
                .ok();
            })
            .detach();
        }));
    }

    /// Sends the problems found by the check at `ix` back to the agent (queued if it is working).
    pub(crate) fn fix_problems(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Entry::Check(Some(checks))) = self.entries.get(ix) else { return };
        let prompt = crate::verify::fix_prompt(checks, &self.root);
        self.ask(prompt, window, cx);
    }
}
