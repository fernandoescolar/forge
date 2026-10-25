//! Keeps agents from reading, searching and writing files through their shell, where
//! Forge has a tool for it. Forge gives Claude a `PreToolUse` hook (in the settings it
//! passes with each session, see [`crate::forge_mcp::session_meta`]) that runs Forge itself
//! with [`HOOK_ARG`]. A simple command Forge's tools do exactly (`cat a.rs`, `grep -rn x
//! src`, `git log -n 5`) is run through them: the hook rewrites it into a call to Forge's
//! MCP server ([`crate::shell_translate`]). A command that greps, cats, pipes into `tail` or
//! writes a file and isn't that simple is refused with the tool to use instead, which Claude
//! reads and follows; so are `git commit` and `git push`, which the user makes from Forge.
//! Test runs and commands piped into `grep`, `head` or `tail` go to `run_tests` and
//! `run_command`. Everything else runs as usual. OpenCode runs the same hook from a plugin
//! of Forge's and takes the changed command too; Copilot and agy run it but can't have a
//! command changed, so they are refused; Codex and Gemini get rules generated from the
//! same table, [`RULES`] (see [`crate::agent_setup`]).
//!
//! Claude's hook also sees other MCP servers' tools: those that name a file kept from
//! agents are refused, and those that change files ask the user first, since their changes
//! skip the review Forge gives the agent's own edits.
//!
//! It's a guide, not a sandbox: it reads the command line as a shell would, well enough to
//! catch the usual ways (`cd x && grep`, `$(cat a)`, `bash -c '…'`, `sudo`, `xargs grep`),
//! and lets uncertain ones run.

use std::io::Read as _;

use serde_json::{Value, json};

/// `forge --agent-shell-hook [url]`: answers Claude Code's `PreToolUse` hook for `Bash`.
pub const HOOK_ARG: &str = "--agent-shell-hook";

/// Runs the hook: reads the tool call from stdin and prints the refusal, if any, in the
/// form of the agent that asks. Nothing printed lets the command run. With the URL of
/// the thread's Forge MCP server, Claude's simple commands are run through its tools
/// instead of refused (see [`crate::shell_translate`]).
pub fn run_hook(url: Option<&str>) {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let Ok(call) = serde_json::from_str::<Value>(&input) else { return };
    let tools = url.zip(std::env::current_exe().ok());
    if let Some(answer) = hook_answer_with(&call, tools.as_ref().map(|(url, exe)| (exe.as_path(), *url))) {
        println!("{answer}");
    }
}

/// The hook's answer to a tool call, `None` to let it run.
pub fn hook_answer(call: &Value) -> Option<Value> {
    hook_answer_with(call, None)
}

/// The longest a Claude Code shell command may run (ms): what translated test runs and
/// commands get, instead of the default two minutes.
const LONGEST_COMMAND_MS: u64 = 600_000;

/// [`hook_answer`], translating Claude's commands that Forge's tools can run (`exe` runs
/// them against the server at the URL) instead of refusing them.
fn hook_answer_with(call: &Value, tools: Option<(&std::path::Path, &str)>) -> Option<Value> {
    // Claude Code: another MCP server's tool (`{"tool_name": "mcp__…", "tool_input"}`).
    if let Some(name) = call.get("tool_name").and_then(Value::as_str).filter(|n| n.starts_with("mcp__")) {
        let root = call.get("cwd").and_then(Value::as_str).map(std::path::Path::new);
        return mcp_answer(name, &call["tool_input"], root);
    }
    // Claude Code: `{"tool_input": {"command"}}`.
    if let Some(command) = call.pointer("/tool_input/command").and_then(Value::as_str) {
        // Test runs aren't refused, but go to the Tests panel when they can.
        if let (Some((exe, url)), Some(translated)) = (tools, crate::shell_translate::translate(command)) {
            let mut input = call["tool_input"].clone();
            input["command"] = json!(crate::shell_translate::exec_command(exe, url, &translated));
            if crate::shell_translate::takes_long(translated.0) {
                let asked = input.get("timeout").and_then(Value::as_u64).unwrap_or(0);
                input["timeout"] = json!(asked.max(LONGEST_COMMAND_MS));
            }
            return Some(json!({ "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow",
                "permissionDecisionReason": format!("Forge ran this with its `{}` tool.", translated.0),
                "updatedInput": input,
            } }));
        }
        let reason = refusal(command)?;
        return Some(json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason } }));
    }
    // Antigravity (agy): `{"toolCall": {"name": "run_command", "args": {"CommandLine"}}}`.
    if let Some(command) = call.pointer("/toolCall/args/CommandLine").and_then(Value::as_str) {
        let reason = refusal(command)?;
        return Some(json!({ "decision": "deny", "reason": reason }));
    }
    // GitHub Copilot CLI: `{"toolName": "bash", "toolArgs": {"command"}}`.
    if let Some(command) = call.pointer("/toolArgs/command").and_then(Value::as_str) {
        let reason = refusal(command)?;
        return Some(json!({ "permissionDecision": "deny", "permissionDecisionReason": reason }));
    }
    None
}

/// The command line that runs the hook with `exe` (this Forge).
pub fn hook_command(exe: &std::path::Path) -> String {
    format!("\"{}\" {HOOK_ARG}", exe.display())
}

/// Claude Code settings that install the hook, running `exe` (this Forge). With the URL
/// of the thread's MCP server, the hook runs simple commands through its tools.
pub fn hook_settings(exe: &std::path::Path, url: Option<&str>) -> Value {
    let command = match url {
        Some(url) => format!("{} {url}", hook_command(exe)),
        None => hook_command(exe),
    };
    json!({ "hooks": { "PreToolUse": [
        { "matcher": "Bash", "hooks": [{ "type": "command", "command": command, "timeout": 10 }] },
        // Other MCP servers' tools (Forge's own check their arguments themselves).
        { "matcher": OTHER_MCP_TOOLS, "hooks": [{ "type": "command", "command": hook_command(exe), "timeout": 10 }] },
    ] } })
}

/// The hook's matcher for tools of MCP servers other than Forge's (a regex for Claude Code).
const OTHER_MCP_TOOLS: &str = "^mcp__(?!forge__)";

/// Words in the name of a tool that changes files (`replace_content`, `write_file`).
const WRITING_TOOL_WORDS: &[&str] = &["write", "edit", "replace", "insert", "create", "delete", "remove", "rename", "move", "patch", "append", "save"];

/// Argument names that hold a file or folder.
const PATH_KEYS: &[&str] = &["path", "paths", "file", "files", "file_path", "filepath", "filename", "relative_path", "source", "destination"];

/// The hook's answer to a tool of another MCP server (`name` as Claude calls it, `root`
/// the project): refused when it names a file kept from agents, and asked about when it
/// changes files, since those changes skip the review Forge gives the agent's own edits.
fn mcp_answer(name: &str, input: &Value, root: Option<&std::path::Path>) -> Option<Value> {
    if name.starts_with(&format!("mcp__{}__", crate::forge_mcp::SERVER_NAME)) {
        return None;
    }
    let paths: Vec<&str> = PATH_KEYS
        .iter()
        .filter_map(|key| input.get(*key))
        .flat_map(|value| match value {
            Value::String(s) => vec![s.as_str()],
            Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        })
        .collect();
    let answer = |decision: &str, reason: String| Some(json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "permissionDecision": decision, "permissionDecisionReason": reason } }));
    if let Some(root) = root {
        let kept = crate::agent_ignore::AgentIgnore::load(root);
        if let Some(path) = paths.iter().find(|p| kept.denies(std::path::Path::new(p))) {
            return answer("deny", format!("Forge refused `{name}`: the user keeps {path} from agents. Ask the user if you need something from it."));
        }
    }
    let tool = name.rsplit("__").next().unwrap_or(name).to_lowercase();
    if !paths.is_empty() && WRITING_TOOL_WORDS.iter().any(|w| tool.contains(w)) {
        return answer("ask", format!("`{name}` changes files without the review Forge gives your own edits. {WRITE}."));
    }
    None
}

/// The commands of the user's own `PreToolUse` hooks for Claude Code's shell tool, in
/// `~/.claude` and the project in `root`: they may change the same commands Forge's hook
/// changes, and Claude Code doesn't say which change wins.
pub fn other_shell_hooks(home: Option<&std::path::Path>, root: &std::path::Path) -> Vec<String> {
    let files = home.map(|h| h.join(".claude/settings.json")).into_iter().chain([root.join(".claude/settings.json"), root.join(".claude/settings.local.json")]);
    let mut commands = Vec::new();
    for file in files {
        let Some(settings) = std::fs::read_to_string(&file).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok()) else { continue };
        for entry in settings.pointer("/hooks/PreToolUse").and_then(Value::as_array).into_iter().flatten() {
            let matcher = entry.get("matcher").and_then(Value::as_str).unwrap_or_default();
            let matches_bash = matcher.is_empty() || matcher == "*" || regex::Regex::new(&format!("^(?:{matcher})$")).is_ok_and(|r| r.is_match("Bash"));
            if !matches_bash {
                continue;
            }
            for hook in entry.get("hooks").and_then(Value::as_array).into_iter().flatten() {
                if let Some(command) = hook.get("command").and_then(Value::as_str).filter(|c| !c.contains(HOOK_ARG)) {
                    commands.push(command.to_string());
                }
            }
        }
    }
    commands
}

// Agents name MCP tools their own way (`mcp__forge__search`, `forge/search`…): the
// reasons name the tool and the server.
pub(crate) const READ: &str = "Read files with your own file-reading tool, or the `read_file` tool of the `forge` MCP server (`start_line` and `lines`, or `tail` for the last lines)";
pub(crate) const SEARCH: &str = "Search with the `search` tool of the `forge` MCP server (text or `regex`, `include`/`exclude` globs, `context` lines, `files_only`): Forge's search, unsaved edits included";
pub(crate) const FIND: &str = "Find files with your own file-finding tool, or the `search` tool of the `forge` MCP server with `files_only`";
pub(crate) const WRITE: &str = "Change files with your own edit and write tools: the user reviews each change as a diff in Forge, and what a shell writes skips that review";
pub(crate) const GIT: &str = "Read the history with the `git_history` tool of the `forge` MCP server (`log`, `changes` for git log -S/-G, `show`, `file_at`, `blame`, `diff`)";
pub(crate) const PIPE: &str = "Run the command with the `run_command` tool of the `forge` MCP server and pass `tail`, `head` or `filter` (a regex) instead of piping: you get that part of the output and the user watches all of it live";
pub(crate) const PUBLISH: &str = "Commits and pushes are the user's to make: call the `propose_commit` tool of the `forge` MCP server (`message`, and `files` instead of staging them) or `propose_push`, and the user makes them from Forge";

/// When the programs of a [`Rule`] are refused.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum When {
    Always,
    /// With this short flag, alone or after lowercase ones (`sed -i`, `perl -pi`, `ls -lR`).
    Flag(char),
    /// When it names a file, as an argument or with `<` (`cat a.rs`, `tee out.txt`); not
    /// `cat <<EOF | git commit -F -` or `tee /dev/stderr`.
    Files,
    /// When another command's output is piped into it (`… | tail`), not through `xargs`.
    Piped,
}

/// Programs Forge has a tool for (`git` subcommands as `git log`), and why.
pub(crate) struct Rule {
    pub programs: &'static [&'static str],
    pub when: When,
    pub reason: &'static str,
}

const FILTERS: &[&str] = &["grep", "egrep", "fgrep", "rg", "ag", "ack", "head", "tail", "sed", "awk", "cut", "sort", "uniq", "wc", "less", "more"];

/// The commands Forge refuses; the first rule that applies gives the reason. [`refusal`],
/// Codex's rules ([`codex_rules`]) and Gemini's policy ([`gemini_command_regex`]) all come
/// from it. Writing a file with `>` is refused too, outside the table.
pub(crate) const RULES: &[Rule] = &[
    Rule { programs: &["sed", "perl"], when: When::Flag('i'), reason: WRITE },
    Rule { programs: &["tee"], when: When::Files, reason: WRITE },
    Rule { programs: FILTERS, when: When::Piped, reason: PIPE },
    Rule { programs: &["grep", "egrep", "fgrep", "rg", "ag", "ack", "git grep"], when: When::Always, reason: SEARCH },
    Rule { programs: &["cat"], when: When::Files, reason: READ },
    Rule { programs: &["head", "tail", "less", "more", "nl", "bat", "sed", "awk"], when: When::Always, reason: READ },
    Rule { programs: &["find", "fd", "tree"], when: When::Always, reason: FIND },
    Rule { programs: &["ls"], when: When::Flag('R'), reason: FIND },
    Rule { programs: &["git log", "git show", "git blame", "git diff", "git annotate", "git whatchanged"], when: When::Always, reason: GIT },
    Rule { programs: &["git commit", "git push"], when: When::Always, reason: PUBLISH },
];

/// Commands that run the command after them (`sudo grep …`), with their options that take
/// a separate value.
const PREFIXES: &[(&str, &[&str])] = &[
    ("env", &["-u", "-C", "-S", "--unset", "--chdir"]),
    ("command", &[]),
    ("builtin", &[]),
    ("exec", &["-a"]),
    ("nohup", &[]),
    ("time", &[]),
    ("sudo", &["-u", "-g", "-C", "-D", "-h", "-p", "-r", "-t", "-U", "-T"]),
    ("doas", &["-u", "-C"]),
    ("nice", &["-n"]),
    ("timeout", &["-s", "-k", "--signal", "--kill-after"]),
    ("stdbuf", &["-i", "-o", "-e"]),
    ("xargs", &["-I", "-n", "-P", "-L", "-s", "-d", "-E", "-a"]),
    // RTK (a token-saving proxy): `rtk grep …`, `rtk proxy git log`.
    ("rtk", &[]),
];

/// Shell keywords before a command (`if grep …`, `{ cat a; }`, `! grep …`).
const KEYWORDS: &[&str] = &["{", "}", "!", "if", "then", "elif", "else", "do", "while", "until"];

/// Shells whose `-c` script is read too.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh"];

/// Why `command` should use a Forge tool instead, or `None` to let it run.
pub fn refusal(command: &str) -> Option<String> {
    refusal_in(command, 0)
}

fn refusal_in(command: &str, depth: usize) -> Option<String> {
    if depth > 8 {
        return None;
    }
    let line = parse(command);
    for segment in &line.segments {
        if segment.outputs.iter().any(|target| names_file(target)) {
            return Some(format!("Forge refused writing a file with `>`. {WRITE}."));
        }
        let Some((words, xargs)) = command_words(&segment.words) else { continue };
        let Some(program) = words.first().map(|w| basename(w)) else { continue };
        let args = &words[1..];
        if SHELLS.contains(&program) {
            let script = args.iter().position(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c')).and_then(|ix| args.get(ix + 1));
            match script.and_then(|s| refusal_in(s, depth + 1)) {
                Some(reason) => return Some(reason),
                None => continue,
            }
        }
        if program == "eval" {
            match refusal_in(&args.join(" "), depth + 1) {
                Some(reason) => return Some(reason),
                None => continue,
            }
        }
        let name = match program {
            "git" => match git_subcommand(args) {
                Some(sub) => format!("git {sub}"),
                None => continue,
            },
            _ => program.to_string(),
        };
        let applies = |when: When| match when {
            When::Always => true,
            When::Flag(flag) => args.iter().any(|a| has_flag(a, flag)),
            When::Files => args.iter().any(|a| !a.starts_with('-') && names_file(a)) || segment.inputs.iter().any(|a| names_file(a)),
            When::Piped => segment.piped && !xargs,
        };
        if let Some(rule) = RULES.iter().find(|r| r.programs.contains(&name.as_str()) && applies(r.when)) {
            return Some(format!("Forge refused `{}`. {}.", segment.words.join(" "), rule.reason));
        }
    }
    line.nested.iter().find_map(|nested| refusal_in(nested, depth + 1))
}

/// `/usr/bin/grep` → `grep`.
pub(crate) fn basename(word: &str) -> &str {
    let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
    name.strip_suffix(".exe").unwrap_or(name)
}

/// A file a redirection or argument names: not `-`, `&2` or `/dev/null`.
pub(crate) fn names_file(target: &str) -> bool {
    target != "-" && !target.starts_with('&') && !target.starts_with("/dev/")
}

/// `-i`, `-pi`, `-lR`: `flag` among the first letters of a short option.
fn has_flag(arg: &str, flag: char) -> bool {
    arg.strip_prefix('-').filter(|rest| !rest.starts_with('-')).is_some_and(|rest| rest.chars().take_while(|c| c.is_ascii_lowercase() || *c == flag).any(|c| c == flag))
}

/// `FOO=1`.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

/// The command a segment runs, past keywords, `FOO=1` and prefixes such as `sudo` or
/// `xargs`, and whether `xargs` runs it; `None` for `command -v grep`, which runs nothing.
fn command_words(words: &[String]) -> Option<(&[String], bool)> {
    let (mut i, mut xargs) = (0, false);
    while let Some(word) = words.get(i) {
        if KEYWORDS.contains(&word.as_str()) || is_assignment(word) {
            i += 1;
            continue;
        }
        let name = basename(word);
        let Some((_, takes_value)) = PREFIXES.iter().find(|(prefix, _)| *prefix == name) else { break };
        xargs |= name == "xargs";
        i += 1;
        while let Some(option) = words.get(i).filter(|w| w.starts_with('-') && w.len() > 1) {
            if name == "command" && (option == "-v" || option == "-V") {
                return None;
            }
            i += if takes_value.contains(&option.as_str()) { 2 } else { 1 };
            if option == "--" {
                break;
            }
        }
        if name == "timeout" {
            i += 1; // Its duration.
        }
        if name == "rtk" && words.get(i).is_some_and(|w| w == "proxy") {
            i += 1;
        }
    }
    Some((&words[i.min(words.len())..], xargs))
}

/// `git -C dir --no-pager log …` → `log`.
fn git_subcommand(args: &[String]) -> Option<&str> {
    let mut words = args.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" => {
                words.next();
            }
            w if w.starts_with('-') => {}
            w => return Some(w),
        }
    }
    None
}

/// A simple command of a command line.
#[derive(Default)]
pub(crate) struct Segment {
    pub words: Vec<String>,
    /// Its input is piped from the command before.
    pub piped: bool,
    /// Where that `|` is in the command line, in characters.
    pub pipe_at: usize,
    /// Where it redirects output (`> file`, `>> file`, `2>&1` as `&1`).
    pub outputs: Vec<String>,
    /// What it reads with `<`.
    pub inputs: Vec<String>,
}

/// A command line: its simple commands, and the scripts it runs inside (`$(…)`, backticks,
/// `<(…)`).
#[derive(Default)]
pub(crate) struct Line {
    pub segments: Vec<Segment>,
    pub nested: Vec<String>,
}

/// What the word being read is.
#[derive(Clone, Copy, PartialEq)]
enum Target {
    Word,
    Output,
    Input,
    /// A here-document's delimiter, and whether its body may be indented with tabs (`<<-`).
    Heredoc(bool),
    /// A here-string or a descriptor (`<<< text`, `<&3`).
    Skip,
}

/// Reads `command` as a shell would, roughly: quotes group words; `&&`, `||`, `;`, `|`,
/// `&`, newlines and parentheses separate commands; `>`, `<`, `<<EOF` (whose body is
/// skipped) and `<<<` are redirections; `#` starts a comment.
pub(crate) fn parse(command: &str) -> Line {
    let chars: Vec<char> = command.chars().collect();
    let mut line = Line::default();
    let mut current = Segment::default();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut target = Target::Word;
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let end_word = |current: &mut Segment, word: &mut String, target: &mut Target, heredocs: &mut Vec<(String, bool)>| {
        if word.is_empty() {
            return;
        }
        let word = std::mem::take(word);
        match std::mem::replace(target, Target::Word) {
            Target::Word => current.words.push(word),
            Target::Output => current.outputs.push(word),
            Target::Input => current.inputs.push(word),
            Target::Heredoc(tabs) => heredocs.push((word, tabs)),
            Target::Skip => {}
        }
    };
    let finish = |line: &mut Line, current: &mut Segment| {
        if !current.words.is_empty() || !current.outputs.is_empty() {
            line.segments.push(std::mem::take(current));
        }
        *current = Segment::default();
    };
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        i += 1;
        let next = chars.get(i).copied();
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => word.push(c),
            (_, '`') => {
                line.nested.push(until_backtick(&chars, &mut i));
                word.push('$');
            }
            (_, '$') if next == Some('(') => {
                i += 1;
                line.nested.push(until_paren(&chars, &mut i));
                word.push('$');
            }
            (Some(_), '\\') => match next {
                Some(n @ ('"' | '\\' | '$' | '`')) => {
                    word.push(n);
                    i += 1;
                }
                Some('\n') => i += 1,
                _ => word.push('\\'),
            },
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => quote = Some(c),
            (None, '\\') => {
                if let Some(n) = next {
                    i += 1;
                    if n != '\n' {
                        word.push(n);
                    }
                }
            }
            (None, ' ' | '\t') => end_word(&mut current, &mut word, &mut target, &mut heredocs),
            (None, '#') if word.is_empty() => {
                while chars.get(i).is_some_and(|&c| c != '\n') {
                    i += 1;
                }
            }
            (None, '<' | '>') if next == Some('(') => {
                i += 1;
                line.nested.push(until_paren(&chars, &mut i));
                word.push('$');
            }
            (None, '<' | '>') => {
                // `2>` / `0<`: the file descriptor isn't a word.
                if word.chars().all(|ch| ch.is_ascii_digit()) {
                    word.clear();
                } else {
                    end_word(&mut current, &mut word, &mut target, &mut heredocs);
                }
                target = if c == '>' {
                    if matches!(next, Some('>' | '|')) {
                        i += 1;
                    }
                    Target::Output
                } else if next == Some('<') {
                    i += 1;
                    match chars.get(i) {
                        Some('<') => {
                            i += 1;
                            Target::Skip
                        }
                        Some('-') => {
                            i += 1;
                            Target::Heredoc(true)
                        }
                        _ => Target::Heredoc(false),
                    }
                } else {
                    Target::Input
                };
                if chars.get(i) == Some(&'&') {
                    i += 1;
                    word.push('&');
                }
            }
            (None, '&') if next == Some('>') => {
                end_word(&mut current, &mut word, &mut target, &mut heredocs);
                i += if chars.get(i + 1) == Some(&'>') { 2 } else { 1 };
                target = Target::Output;
            }
            (None, '|' | '&' | ';' | '\n' | '(' | ')') => {
                let at = i - 1;
                end_word(&mut current, &mut word, &mut target, &mut heredocs);
                let doubled = matches!(c, '|' | '&' | ';') && next == Some(c);
                if doubled || (c == '|' && next == Some('&')) {
                    i += 1;
                }
                finish(&mut line, &mut current);
                current.piped = c == '|' && !doubled;
                current.pipe_at = at;
                if c == '\n' {
                    skip_heredocs(&chars, &mut i, &mut heredocs);
                }
            }
            (None, c) => word.push(c),
        }
    }
    end_word(&mut current, &mut word, &mut target, &mut heredocs);
    finish(&mut line, &mut current);
    line
}

/// Skips the bodies of the here-documents started on the line just ended.
fn skip_heredocs(chars: &[char], i: &mut usize, heredocs: &mut Vec<(String, bool)>) {
    for (delimiter, tabs) in heredocs.drain(..) {
        while *i < chars.len() {
            let start = *i;
            while chars.get(*i).is_some_and(|&c| c != '\n') {
                *i += 1;
            }
            let text: String = chars[start..*i].iter().collect();
            *i += 1;
            if (if tabs { text.trim_start_matches('\t') } else { &text }) == delimiter {
                break;
            }
        }
    }
}

/// The script of a `$(…)` from after its `(`, leaving `i` after its `)`.
fn until_paren(chars: &[char], i: &mut usize) -> String {
    let (mut depth, mut quote, mut out) = (1, None, String::new());
    while let Some(&c) = chars.get(*i) {
        *i += 1;
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => quote = Some(c),
            (None | Some('"'), '\\') => {
                out.push(c);
                if let Some(&n) = chars.get(*i) {
                    out.push(n);
                    *i += 1;
                }
                continue;
            }
            (None, '(') => depth += 1,
            (None, ')') => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        out.push(c);
    }
    out
}

/// The script between backticks from after the first, leaving `i` after the second.
fn until_backtick(chars: &[char], i: &mut usize) -> String {
    let mut out = String::new();
    while let Some(&c) = chars.get(*i) {
        *i += 1;
        match c {
            '`' => break,
            '\\' => {
                out.extend(chars.get(*i));
                *i += 1;
            }
            c => out.push(c),
        }
    }
    out
}

/// Where a program may be run from by its full path.
const BIN_DIRS: &[&str] = &["", "/bin/", "/usr/bin/", "/usr/local/bin/", "/opt/homebrew/bin/"];

/// Codex's execpolicy rules, from [`RULES`]: each forbids commands starting with given
/// words, with the reason. Codex checks each command of a plain script (`a | tail`,
/// `cd x && grep`), but execpolicy can't express everything the hook checks:
/// - `>` redirects and here-documents: Codex doesn't split scripts that have them into
///   commands, so no rule sees them (Forge's instructions say not to);
/// - `Piped` rules: it doesn't see what pipes into a command (most of those programs are
///   refused anyway);
/// - `Files` ones refuse the program whatever its arguments (`cat <<EOF | …` too);
/// - `Flag` ones only when the flag comes first, spelled as a few usual combinations;
/// - prefixes (`sudo`, `xargs`…) only right before the program, without their options;
///   `git -C dir log` isn't caught, `git --no-pager log` is.
pub(crate) fn codex_rules() -> String {
    let mut out = String::from("# Written by Forge each time it starts Codex, from its shell guard.\n");
    let prefixes: Vec<&str> = PREFIXES.iter().map(|(name, _)| *name).collect();
    for rule in RULES.iter().filter(|r| r.when != When::Piped) {
        let justification = serde_json::to_string(&format!("Forge has a tool for this. {}.", rule.reason)).unwrap_or_default();
        let (git, programs): (Vec<&str>, Vec<&str>) = rule.programs.iter().partition(|p| p.starts_with("git "));
        let mut patterns: Vec<Vec<Value>> = Vec::new();
        if !programs.is_empty() {
            let mut pattern = vec![json!(programs.iter().flat_map(|p| BIN_DIRS.iter().map(move |dir| format!("{dir}{p}"))).collect::<Vec<_>>())];
            if let When::Flag(flag) = rule.when {
                pattern.push(json!(["", "l", "a", "p", "n", "la", "al"].iter().map(|before| format!("-{before}{flag}")).collect::<Vec<_>>()));
            }
            patterns.push([vec![json!(prefixes)], pattern.clone()].concat());
            patterns.push(pattern);
        }
        if !git.is_empty() {
            let git_spellings: Vec<String> = BIN_DIRS.iter().map(|dir| format!("{dir}git")).collect();
            let subcommands: Vec<&str> = git.iter().map(|g| &g[4..]).collect();
            patterns.push(vec![json!(prefixes), json!(git_spellings), json!(subcommands)]);
            patterns.push(vec![json!(git_spellings), json!(subcommands)]);
            patterns.push(vec![json!(git_spellings), json!(["--no-pager", "-P"]), json!(subcommands)]);
        }
        for pattern in patterns {
            out += &format!("prefix_rule(pattern={}, decision=\"forbidden\", justification={justification})\n", Value::Array(pattern));
        }
    }
    out
}

/// Gemini's `commandRegex` for the shell commands to deny, from [`RULES`]. Gemini puts
/// `"command":"` before it and matches it in the call's arguments as JSON (quotes as `\"`,
/// newlines as `\n`), so it matches a command at the start, or after a separator (`&&`,
/// `||`, `;`, `|`, newline, `(`, `` ` ``) before the first quote; Gemini also checks each
/// command of a script, and `bash -c`'s, on its own. It refuses a few prefixes (`sudo`,
/// `FOO=1`, `/usr/bin/`), `git -C dir log`, and `>` writing a file before the first quote.
/// Gemini refuses a regex with a quantifier after a group (`(…)?`, against ReDoS), so the
/// optional parts are alternations with an empty branch, and repeats are spelled out.
pub(crate) fn gemini_command_regex() -> String {
    let words = |names: &[&str]| names.join("|");
    // Where a command starts.
    let lead = r#"(?:|[^"']*?(?:[;&|(`]|\\n))\s*"#;
    let slot = format!(r#"(?:|[A-Za-z_][A-Za-z0-9_]*=[^"\s]*\s+|(?:{})\s+|-[^"\s]*\s+|-[^"\s]*\s+[^-"\s][^"\s]*\s+)"#,words(&PREFIXES.iter().map(|(name, _)| *name).collect::<Vec<_>>()));
    let path = r#"(?:|[^"\s;&|]*/)"#;
    let end = r#"(?:[\s;&|)]|\\|"|$)"#;
    let git_option = r#"(?:|-[Cc]\s+[^"\s]+\s+|--(?:git-dir|work-tree|namespace)\s+[^"\s]+\s+|-[^"\s]*\s+)"#;
    let mut commands = Vec::new();
    let mut filters = Vec::new();
    for rule in RULES {
        let (git, programs): (Vec<&str>, Vec<&str>) = rule.programs.iter().partition(|p| p.starts_with("git "));
        if !programs.is_empty() {
            let programs = words(&programs);
            match rule.when {
                When::Always => commands.push(format!("(?:{programs}){end}")),
                When::Flag(flag) => commands.push(format!(r#"(?:{programs})\s(?:|[^"\\;&|]*\s)-[a-z]*{flag}"#)),
                When::Files => commands.push(format!(r#"(?:{programs})\s+(?:|-[^"\s]*\s+)(?:[^-<>\s"\\|;&/(]|/[^d]|\\")"#)),
                When::Piped => filters.push(programs),
            }
        }
        if !git.is_empty() {
            let subcommands = words(&git.iter().map(|g| &g[4..]).collect::<Vec<_>>());
            commands.push(format!(r"git\s+{git_option}{git_option}(?:{subcommands}){end}"));
        }
    }
    let command = format!("{lead}{slot}{slot}{slot}{path}(?:{})", commands.join("|"));
    // A filter after a single `|`.
    let piped = format!(r#"[^"']*?[^|]\|\s*{path}(?:{}){end}"#, filters.join("|"));
    // `>`, `>>` writing a file: not `>&2`, `>/dev/null` or `>(…)`.
    let redirect = r#"[^"']*?>>?\s*(?:[^&/\s"\\(]|/[^d])"#;
    format!("(?:{command}|{piped}|{redirect})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_what_forge_has_a_tool_for() {
        let (search, read, write, find, git, pipe) = ("`search` tool", "read_file", "edit and write tools", "files_only", "git_history", "run_command");
        let cases: &[(&str, Option<&str>)] = &[
            ("grep -rn parse src", Some(search)),
            ("cd crates/x && rg foo", Some(search)),
            ("git grep -n foo", Some(search)),
            ("cat src/main.rs", Some(read)),
            ("cat < src/main.rs", Some(read)),
            ("head -5 < a.rs", Some(read)),
            ("sed -n 10,20p a.rs", Some(read)),
            ("sed -i '' 's/a/b/' a.rs", Some(write)),
            ("perl -pi -e 's/a/b/' a.rs", Some(write)),
            ("cat >> notes.md <<'EOF'\nhi\nEOF", Some(write)),
            ("cat <<EOF > notes.md\nhi\nEOF", Some(write)),
            ("tee out.txt <<EOF\nhi\nEOF", Some(write)),
            ("npm test | tee log.txt", Some(write)),
            ("echo hi > out.txt", Some(write)),
            ("echo hi >| out.txt", Some(write)),
            ("echo hi 1>out.txt", Some(write)),
            ("echo hi &> out.txt", Some(write)),
            ("cargo build 2>build.log", Some(write)),
            ("git -C repo log -S foo", Some(git)),
            ("git --no-pager diff", Some(git)),
            ("git -c core.pager=less show HEAD", Some(git)),
            ("cargo test 2>&1 | tail -20", Some(pipe)),
            ("npm ls | grep react", Some(pipe)),
            ("npm ls |& grep react", Some(pipe)),
            ("cargo tree | sort | uniq", Some(pipe)),
            ("find . -name '*.rs'", Some(find)),
            ("ls -lR src", Some(find)),
            // Substitutions, shells and prefixes.
            ("echo $(grep -r foo .)", Some(search)),
            ("echo \"$(cat a.rs)\"", Some(read)),
            ("echo `head -5 a.rs`", Some(read)),
            ("diff <(cat a.rs) b.rs", Some(read)),
            ("bash -c 'grep foo src'", Some(search)),
            ("sh -lc \"cd x && cat a.rs\"", Some(read)),
            ("zsh -c 'git log'", Some(git)),
            ("eval 'cat a.rs'", Some(read)),
            ("git ls-files | xargs grep foo", Some(search)),
            ("xargs -0 -n 1 grep foo < files.txt", Some(search)),
            ("env LANG=C grep foo a.rs", Some(search)),
            ("FOO=1 BAR=2 grep foo a.rs", Some(search)),
            ("command grep foo a.rs", Some(search)),
            ("exec cat a.rs", Some(read)),
            ("nohup tail -f log.txt", Some(read)),
            ("time rg foo", Some(search)),
            ("sudo -u root cat /etc/hosts", Some(read)),
            ("timeout 5 grep foo a.rs", Some(search)),
            ("/usr/bin/grep foo a.rs", Some(search)),
            ("\\grep foo a.rs", Some(search)),
            ("(cd src && grep foo *.rs)", Some(search)),
            ("{ cat a.rs; }", Some(read)),
            ("if grep -q foo a.rs; then echo y; fi", Some(search)),
            ("rtk grep foo src", Some(search)),
            ("rtk proxy git log -n 3", Some(git)),
            // Commits and pushes are the user's.
            ("git commit -m \"fix: cat > dog\"", Some("propose_commit")),
            ("git add -A && git commit -m x && git push", Some("propose_commit")),
            ("git -C repo push origin main", Some("propose_push")),
            ("cat <<EOF | git commit -F -\nfix\nEOF", Some("propose_commit")),
            // Allowed.
            ("cargo build", None),
            ("git status", None),
            ("git -C repo status", None),
            ("git add -A", None),
            ("npm install", None),
            ("ls", None),
            ("ls -la", None),
            ("pwd", None),
            ("cargo test 2>&1", None),
            ("make > /dev/null 2>&1", None),
            ("cargo build 2>/dev/null", None),
            ("echo error >&2", None),
            ("echo 'a | grep b'", None),
            ("echo \"fix: cat > dog\"", None),
            ("dotnet build && dotnet test", None),
            ("cat <<EOF | gh pr create --body-file -\nfix: find the grep bug\n\ncat > dog\nEOF", None),
            ("gh pr create --body-file - <<'EOF'\nfind things\n| grep\nEOF\ncargo build", None),
            ("rtk gain", None),
            ("git add -A", None),
            ("python3 - <<<'print(1)'", None),
            ("command -v grep", None),
            ("which rg", None),
            ("echo x | cat", None),
            ("npm test | tee /dev/stderr", None),
            ("perl -Mstrict -e 'print 1'", None),
            ("echo $(date) `whoami`", None),
            ("bash -c 'cargo build'", None),
            ("sh script.sh", None),
            ("env FOO=1 cargo test", None),
            ("sudo make install", None),
            ("grep_tool --check", None),
            ("cargo test # then grep the output", None),
        ];
        for (command, expected) in cases {
            let got = refusal(command);
            match expected {
                Some(part) => assert!(got.as_deref().is_some_and(|r| r.contains(part)), "{command:?} should be refused with {part}: {got:?}"),
                None => assert!(got.is_none(), "{command:?} should run: {got:?}"),
            }
        }
    }

    fn codex_forbids(rules: &str, command: &str) -> bool {
        let words: Vec<&str> = command.split_whitespace().collect();
        rules.lines().filter_map(|l| l.strip_prefix("prefix_rule(pattern=")?.split_once(", decision=")).any(|(pattern, _)| {
            let pattern: Vec<Value> = serde_json::from_str(pattern).unwrap();
            pattern.len() <= words.len()
                && pattern.iter().zip(&words).all(|(p, w)| match p {
                    Value::String(s) => s == w,
                    Value::Array(a) => a.iter().any(|s| s.as_str() == Some(*w)),
                    _ => false,
                })
        })
    }

    fn gemini_regex() -> regex::Regex {
        regex::Regex::new(&format!("\"command\":\"{}", gemini_command_regex())).unwrap()
    }

    fn gemini_denies(regex: &regex::Regex, command: &str) -> bool {
        regex.is_match(&json!({ "command": command }).to_string())
    }

    #[test]
    fn codex_and_gemini_refuse_the_same_programs() {
        let (rules, gemini) = (codex_rules(), gemini_regex());
        for rule in RULES.iter().filter(|r| r.when != When::Piped) {
            for program in rule.programs {
                let command = match rule.when {
                    When::Flag(flag) => format!("{program} -{flag} x"),
                    _ => format!("{program} a.rs"),
                };
                assert!(refusal(&command).is_some(), "hook: {command}");
                assert!(codex_forbids(&rules, &command), "codex: {command}\n{rules}");
                assert!(gemini_denies(&gemini, &command), "gemini: {command}");
                let sudo = format!("sudo {command}");
                assert!(codex_forbids(&rules, &sudo) && gemini_denies(&gemini, &sudo), "{sudo}");
            }
        }
        for filter in FILTERS {
            assert!(gemini_denies(&gemini, &format!("npm ls | {filter}")), "gemini: | {filter}");
        }
        for command in ["/usr/bin/grep x", "git --no-pager log"] {
            assert!(codex_forbids(&rules, command) && gemini_denies(&gemini, command), "{command}");
        }
        for command in ["cd x && grep foo", "FOO=1 grep x", "git -C dir log", "echo $(cat a.rs)", "echo hi > out.txt", "a; b\ncat a.rs", "xargs -0 grep x", "sudo -u root cat /etc/hosts", "cat \"a.rs\""] {
            assert!(gemini_denies(&gemini, command), "gemini: {command}");
        }
        for command in ["cargo build", "git status", "git add log", "ls -la", "npm test 2>&1", "echo hi >/dev/null", "echo hi >&2", "perl -Mstrict -e 1", "a || sort-files", "cargo test --features grep", "my-grep x"] {
            assert!(!codex_forbids(&rules, command), "codex: {command}");
            assert!(!gemini_denies(&gemini, command), "gemini: {command}");
            assert!(refusal(command).is_none(), "hook: {command}");
        }
        // Execpolicy refuses `cat` whatever follows.
        for command in ["cat", "cat <<EOF | gh pr create --body-file -"] {
            assert!(!gemini_denies(&gemini, command) && refusal(command).is_none(), "{command}");
        }
    }

    #[test]
    fn gemini_takes_the_regex() {
        // Gemini's own checks: at most 2048 characters, and no quantifier after a group.
        let pattern = format!("\"command\":\"{}", gemini_command_regex());
        assert!(pattern.len() <= 2048, "{}", pattern.len());
        assert!(!regex::Regex::new(r"\([^)]*[*+?{].*\)[*+?{]").unwrap().is_match(&pattern), "{pattern}");
    }

    #[test]
    fn answers_each_agents_hook_in_its_form() {
        let claude = hook_answer(&json!({ "tool_name": "Bash", "tool_input": { "command": "grep -rn x ." } })).unwrap();
        assert_eq!(claude["hookSpecificOutput"]["permissionDecision"], "deny");
        let agy = hook_answer(&json!({ "toolCall": { "name": "run_command", "args": { "CommandLine": "cat a.rs" } } })).unwrap();
        assert_eq!(agy["decision"], "deny");
        assert!(agy["reason"].as_str().unwrap().contains("read_file"));
        let copilot = hook_answer(&json!({ "toolName": "bash", "toolArgs": { "command": "git log -S foo" } })).unwrap();
        assert_eq!(copilot["permissionDecision"], "deny");
        assert!(copilot["permissionDecisionReason"].as_str().unwrap().contains("git_history"));
        assert_eq!(hook_answer(&json!({ "tool_input": { "command": "cargo build" } })), None);
        assert_eq!(hook_answer(&json!({ "toolCall": { "args": { "CommandLine": "npm install" } } })), None);
    }

    #[test]
    fn answers_claude_codes_hook() {
        let exe = std::path::Path::new("/Applications/Forge.app/Contents/MacOS/forge");
        let settings = hook_settings(exe, None);
        assert_eq!(settings["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "\"/Applications/Forge.app/Contents/MacOS/forge\" --agent-shell-hook");
        let with_tools = hook_settings(exe, Some("http://127.0.0.1:5/mcp/t"));
        assert_eq!(with_tools["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "\"/Applications/Forge.app/Contents/MacOS/forge\" --agent-shell-hook http://127.0.0.1:5/mcp/t");
        // Other MCP servers' tools, not Forge's.
        let matcher = regex::Regex::new(&settings["hooks"]["PreToolUse"][1]["matcher"].as_str().unwrap().replace("(?!forge__)", "")).unwrap();
        assert!(matcher.is_match("mcp__serena__replace_content"));
        assert_eq!(settings["hooks"]["PreToolUse"][1]["matcher"], "^mcp__(?!forge__)", "a lookahead: Claude Code's matchers are JavaScript regexes");
    }

    #[test]
    fn guards_other_mcp_servers_tools() {
        let project = tempfile::tempdir().unwrap();
        let cwd = project.path().to_str().unwrap();
        let call = |name: &str, input: Value| hook_answer(&json!({ "tool_name": name, "tool_input": input, "cwd": cwd }));
        let decision = |answer: Option<Value>| answer.map(|a| a["hookSpecificOutput"]["permissionDecision"].as_str().unwrap().to_string());
        assert_eq!(decision(call("mcp__serena__read_file", json!({ "relative_path": ".env" }))).as_deref(), Some("deny"), "kept from agents");
        assert_eq!(decision(call("mcp__serena__replace_content", json!({ "relative_path": "src/a.rs", "needle": "a", "repl": "b" }))).as_deref(), Some("ask"));
        assert_eq!(decision(call("mcp__fs__write_file", json!({ "paths": ["a.rs"] }))).as_deref(), Some("ask"));
        assert_eq!(call("mcp__serena__find_symbol", json!({ "relative_path": "src/a.rs" })), None, "reads go ahead");
        assert_eq!(call("mcp__github__create_issue", json!({ "title": "x" })), None, "names no file");
        assert_eq!(call("mcp__forge__read_file", json!({ "path": ".env" })), None, "Forge's own tools check themselves");
        // A tool's `command` argument isn't a shell command.
        assert_eq!(call("mcp__ssh__run", json!({ "command": "cat a.rs" })), None);
    }

    #[test]
    fn finds_the_users_own_shell_hooks() {
        let (home, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::create_dir_all(project.path().join(".claude")).unwrap();
        let hooks = |matcher: &str, command: &str| json!({ "hooks": { "PreToolUse": [{ "matcher": matcher, "hooks": [{ "type": "command", "command": command }] }] } }).to_string();
        std::fs::write(home.path().join(".claude/settings.json"), hooks("Bash", "rtk-rewrite.sh")).unwrap();
        std::fs::write(project.path().join(".claude/settings.json"), hooks("Edit|Write", "fmt.sh")).unwrap();
        std::fs::write(project.path().join(".claude/settings.local.json"), hooks("*", "\"/F/forge\" --agent-shell-hook")).unwrap();
        assert_eq!(other_shell_hooks(Some(home.path()), project.path()), ["rtk-rewrite.sh"]);
        assert!(other_shell_hooks(None, project.path()).is_empty());
    }

    #[test]
    fn runs_what_forge_has_a_tool_for_when_it_can() {
        let tools = Some((std::path::Path::new("/F/forge"), "http://127.0.0.1:5/mcp/t"));
        let call = |command: &str| json!({ "tool_name": "Bash", "tool_input": { "command": command, "description": "Read it" } });
        let answer = hook_answer_with(&call("head -n 5 src/a.rs"), tools).unwrap();
        let output = &answer["hookSpecificOutput"];
        assert_eq!(output["permissionDecision"], "allow");
        assert_eq!(output["updatedInput"]["description"], "Read it", "the rest of the input stays");
        assert!(output["updatedInput"]["command"].as_str().unwrap().starts_with("\"/F/forge\" --agent-shell-exec http://127.0.0.1:5/mcp/t "));
        // Not exact, or no server: refused as before.
        assert_eq!(hook_answer_with(&call("grep -c foo a.rs"), tools).unwrap()["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(hook_answer_with(&call("npm ls | sort"), tools).unwrap()["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(hook_answer_with(&call("git commit -m x"), tools).unwrap()["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(hook_answer_with(&call("head -n 5 src/a.rs"), None).unwrap()["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(hook_answer_with(&call("cargo build"), tools), None);
        // Tests and piped commands go to the Tests panel and the terminal, with time to run.
        let tests = &hook_answer_with(&call("cargo test parser 2>&1 | tail -20"), tools).unwrap()["hookSpecificOutput"];
        assert_eq!((tests["permissionDecision"].as_str(), tests["updatedInput"]["timeout"].as_u64()), (Some("allow"), Some(LONGEST_COMMAND_MS)));
        assert!(tests["permissionDecisionReason"].as_str().unwrap().contains("run_tests"));
        let piped = &hook_answer_with(&call("cargo build 2>&1 | tail -20"), tools).unwrap()["hookSpecificOutput"];
        assert!(piped["permissionDecisionReason"].as_str().unwrap().contains("run_command"));
        let read = &hook_answer_with(&call("cat a.rs"), tools).unwrap()["hookSpecificOutput"];
        assert_eq!(read["updatedInput"].get("timeout"), None, "reads keep the agent's timeout");
        // Without Forge's server, tests run as they are.
        assert_eq!(hook_answer_with(&call("cargo test"), None), None);
        // Other agents have no way to change the command: they are refused.
        let agy = hook_answer_with(&json!({ "toolCall": { "args": { "CommandLine": "cat a.rs" } } }), tools).unwrap();
        assert_eq!(agy["decision"], "deny");
    }
}
