//! Rule severities in `.editorconfig`, as Visual Studio writes them:
//!
//! ```text
//! [*.cs]
//! dotnet_diagnostic.CA1822.severity = none
//! ```
//!
//! Edits keep the rest of the file as it is: an existing line for the rule is replaced, a
//! new one goes at the end of the first section for C# files, and that section is added
//! when the file has none.

/// The severities an analyzer rule can have.
pub const SEVERITIES: [&str; 6] = ["none", "silent", "suggestion", "warning", "error", "default"];

/// `text` (an `.editorconfig`, possibly empty) with `rule` set to `severity`.
pub fn set_severity(text: &str, rule: &str, severity: &str) -> String {
    let key = format!("dotnet_diagnostic.{rule}.severity");
    let line = format!("{key} = {severity}");
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

    let is_key = |l: &str| l.split('=').next().is_some_and(|k| k.trim().eq_ignore_ascii_case(&key));
    if let Some(existing) = lines.iter().position(|l| is_key(l)) {
        // Keep the key as the file spells it.
        let written = lines[existing].split('=').next().unwrap_or_default().trim().to_string();
        lines[existing] = format!("{written} = {severity}");
        return join(lines, newline);
    }

    match lines.iter().position(|l| is_csharp_section(l)) {
        Some(header) => {
            // After the section's last setting, before the blank lines that end it.
            let end = lines[header + 1..].iter().position(|l| l.trim_start().starts_with('[')).map_or(lines.len(), |i| header + 1 + i);
            let mut at = end;
            while at > header + 1 && lines[at - 1].trim().is_empty() {
                at -= 1;
            }
            lines.insert(at, line);
        }
        None => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push("[*.cs]".into());
            lines.push(line);
        }
    }
    join(lines, newline)
}

/// The severity `text` sets for `rule`, if any.
pub fn severity(text: &str, rule: &str) -> Option<String> {
    let key = format!("dotnet_diagnostic.{rule}.severity");
    text.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        name.trim().eq_ignore_ascii_case(&key).then(|| value.trim().to_string())
    })
}

/// `[*.cs]`, `[*.{cs,vb}]` and the like.
fn is_csharp_section(line: &str) -> bool {
    let line = line.trim();
    let Some(glob) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) else { return false };
    glob == "*.cs" || (glob.starts_with("*.{") && glob.trim_start_matches("*.{").trim_end_matches('}').split(',').any(|ext| ext.trim() == "cs"))
}

fn join(lines: Vec<String>, newline: &str) -> String {
    let mut text = lines.join(newline);
    text.push_str(newline);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_replaces_and_creates_sections() {
        assert_eq!(set_severity("", "CA1822", "none"), "[*.cs]\ndotnet_diagnostic.CA1822.severity = none\n");

        let text = "root = true\n\n[*]\nindent_style = space\n\n[*.{cs,vb}]\ndotnet_diagnostic.IDE0005.severity = warning\n\n[*.json]\nindent_size = 2\n";
        let added = set_severity(text, "CA1822", "error");
        assert_eq!(added, "root = true\n\n[*]\nindent_style = space\n\n[*.{cs,vb}]\ndotnet_diagnostic.IDE0005.severity = warning\ndotnet_diagnostic.CA1822.severity = error\n\n[*.json]\nindent_size = 2\n");
        assert_eq!(severity(&added, "CA1822").as_deref(), Some("error"));

        let replaced = set_severity(&added, "ide0005", "none");
        assert!(replaced.contains("dotnet_diagnostic.IDE0005.severity = none\n"), "{replaced}");
        assert_eq!(replaced.lines().count(), added.lines().count(), "replaced, not added");

        let windows = set_severity("[*]\r\nindent_style = tab\r\n", "CS8019", "silent");
        assert_eq!(windows, "[*]\r\nindent_style = tab\r\n\r\n[*.cs]\r\ndotnet_diagnostic.CS8019.severity = silent\r\n");
    }
}
