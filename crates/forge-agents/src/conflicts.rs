//! Merge conflicts resolved by an agent: the editor's "Resolve with Agent" buttons (one
//! conflict) and the status bar's (every conflicted file) come from Zed's git UI, which
//! shows them because Forge sets `git_ui::ExternalConflictAgent` (patch 0010).

use zed_actions::agent::ConflictContent;

pub fn init(cx: &mut gpui::App) {
    cx.set_global(git_ui::ExternalConflictAgent);
}

const HOW: &str = "Edit the file so it keeps what each side meant (combine both when they don't contradict each other), remove the conflict markers, and make sure the result still builds. Don't stage or commit anything. Then say in a sentence or two what you kept from each side.";

pub fn prompt_for_conflicts(conflicts: &[ConflictContent]) -> String {
    let mut prompt = String::from("Resolve this merge conflict.\n");
    for conflict in conflicts {
        prompt.push_str(&format!(
            "\nIn {} ({} is ours, {} is theirs):\n```\n{}\n```\n",
            conflict.file_path,
            conflict.ours_branch_name,
            conflict.theirs_branch_name,
            conflict.conflict_text.trim_end()
        ));
    }
    prompt.push('\n');
    prompt.push_str(HOW);
    prompt
}

/// The project's files with merge conflicts (for Git › Resolve Conflicts with the Agent,
/// which names none), as Zed's status bar indicator finds them.
pub fn conflicted_paths(project: &project::Project, cx: &gpui::App) -> Vec<String> {
    let mut paths = Vec::new();
    for repo in project.git_store().read(cx).repositories().values() {
        let snapshot = repo.read(cx).snapshot();
        for (repo_path, _) in snapshot.merge.merge_heads_by_conflicted_path.iter() {
            if !snapshot.status_for_path(repo_path).is_some_and(|entry| entry.status.is_conflicted()) {
                continue;
            }
            if let Some(project_path) = repo.read(cx).repo_path_to_project_path(repo_path, cx) {
                paths.push(project_path.path.as_std_path().to_string_lossy().into_owned());
            }
        }
    }
    paths
}

pub fn prompt_for_files(paths: &[String]) -> String {
    let list: String = paths.iter().map(|p| format!("- {p}\n")).collect();
    format!("These files have merge conflicts:\n{list}\nResolve every conflict in them, one file at a time. {HOW}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_carry_the_conflict() {
        let conflict = ConflictContent { file_path: "/p/a.rs".into(), conflict_text: "<<<<<<< HEAD\na\n=======\nb\n>>>>>>> feature\n".into(), ours_branch_name: "HEAD".into(), theirs_branch_name: "feature".into() };
        let prompt = prompt_for_conflicts(&[conflict]);
        assert!(prompt.contains("In /p/a.rs (HEAD is ours, feature is theirs):\n```\n<<<<<<< HEAD\na\n=======\nb\n>>>>>>> feature\n```"), "{prompt}");
        assert!(prompt_for_files(&["a.rs".into(), "b.rs".into()]).starts_with("These files have merge conflicts:\n- a.rs\n- b.rs\n"));
    }
}
