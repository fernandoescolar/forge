use super::*;
use gpui::{TestAppContext, VisualTestContext};

fn pr() -> PullRequest {
    PullRequest {
        number: 12,
        title: "Faster startup".into(),
        author: "ana".into(),
        branch: "perf/startup".into(),
        base: "main".into(),
        draft: false,
        url: "https://github.com/o/r/pull/12".into(),
        review: None,
        checks: Default::default(),
    }
}

#[test]
fn the_review_prompt_points_the_agent_at_gh() {
    let prompt = review_prompt(&pr());
    assert!(prompt.starts_with("Review pull request #12 \"Faster startup\" by @ana (perf/startup into main): https://github.com/o/r/pull/12"), "{prompt}");
    assert!(prompt.contains("`gh pr diff 12`") && prompt.contains("Don't change any files"));
}

/// Review comments show under their lines in the file's editor, and are replaced when
/// they are read again.
#[gpui::test]
async fn review_comments_show_in_the_code(cx: &mut TestAppContext) {
    let params = cx.update(workspace::AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        init(cx);
    });
    params.fs.as_fake().insert_tree("/root", serde_json::json!({ "src": { "a.rs": "fn a() {}\nfn b() {}\nfn c() {}\n" } })).await;
    let project = project::Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = window.read_with(cx, |mw, _| mw.workspace().clone()).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    let editor = workspace
        .update_in(cx, |ws, window, cx| ws.open_abs_path(PathBuf::from("/root/src/a.rs"), workspace::OpenOptions::default(), window, cx))
        .await
        .unwrap()
        .downcast::<Editor>()
        .unwrap();
    let github = cx.update(|_, cx| GitHub::for_workspace(workspace.entity_id(), cx)).expect("each workspace has one");

    let comment = |id: u64, line: Option<u32>, reply_to: Option<u64>| ReviewComment { id, path: "src/a.rs".into(), line, author: "bo".into(), body: format!("comment {id}"), reply_to, url: String::new() };
    github.update(cx, |g, cx| g.set_comments(12, Path::new("/root"), vec![comment(1, Some(2), None), comment(2, Some(2), Some(1)), comment(3, None, None)], cx));
    let blocks = |cx: &mut VisualTestContext| editor.read_with(cx, |e, _| e.addon::<CommentsAddon>().map(|a| a.blocks.len()));
    assert_eq!(blocks(cx), Some(1), "one thread on its line; the outdated one has no line");

    github.update(cx, |g, cx| g.set_comments(12, Path::new("/root"), vec![comment(1, Some(1), None), comment(4, Some(3), None)], cx));
    assert_eq!(blocks(cx), Some(2), "read again: the new threads replace the old");
    let generation = github.read_with(cx, |g, _| g.generation);
    assert_eq!(editor.read_with(cx, |e, _| e.addon::<CommentsAddon>().map(|a| a.generation)), Some(generation));
}
