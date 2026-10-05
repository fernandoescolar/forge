//! Package references in the text of a project file being edited, which may not be valid
//! XML at the moment: where they are, and what the cursor is on for completions.

use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;

const PACKAGE_ELEMENTS: &[&str] = &["PackageReference", "PackageVersion", "GlobalPackageReference", "PackageDownload"];

static START_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<(PackageReference|PackageVersion|GlobalPackageReference|PackageDownload)\b([^<>]*?)(/?)>").unwrap());
static ATTRIBUTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"([A-Za-z_][\w.-]*)\s*=\s*"([^"]*)""#).unwrap());
static VERSION_CHILD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<(Version|VersionOverride)>\s*([^<]*?)\s*</(?:Version|VersionOverride)>").unwrap());

#[derive(Clone, Debug, PartialEq)]
pub struct TextReference {
    pub element: String,
    pub name: String,
    pub name_range: Range<usize>,
    pub version: Option<String>,
    /// Byte range of the version text, to replace it.
    pub version_range: Option<Range<usize>>,
    /// Where the start tag ends: hints go after it.
    pub tag_end: usize,
}

/// Every package reference or version with a name, in order.
pub fn package_references(text: &str) -> Vec<TextReference> {
    let mut found = Vec::new();
    for caps in START_TAG.captures_iter(text) {
        let whole = caps.get(0).unwrap();
        let attrs = caps.get(2).unwrap();
        let self_closing = !caps[3].is_empty();
        let mut name = None;
        let mut version = None;
        for attr in ATTRIBUTE.captures_iter(attrs.as_str()) {
            let value = attr.get(2).unwrap();
            let range = attrs.start() + value.start()..attrs.start() + value.end();
            match &attr[1] {
                "Include" | "Update" => name = Some((value.as_str().to_string(), range)),
                "Version" if version.is_none() => version = Some((value.as_str().to_string(), range)),
                "VersionOverride" => version = Some((value.as_str().to_string(), range)),
                _ => {}
            }
        }
        if version.is_none() && !self_closing {
            // `<PackageReference Include="X"><Version>1.0</Version></PackageReference>`
            let rest = &text[whole.end()..];
            let body_end = rest.find(&format!("</{}", &caps[1])).unwrap_or(0);
            if let Some(child) = VERSION_CHILD.captures(&rest[..body_end]) {
                let value = child.get(2).unwrap();
                version = Some((value.as_str().to_string(), whole.end() + value.start()..whole.end() + value.end()));
            }
        }
        let Some((name, name_range)) = name else { continue };
        if name.trim().is_empty() || name.contains("$(") {
            continue;
        }
        found.push(TextReference {
            element: caps[1].to_string(),
            name,
            name_range,
            version: version.as_ref().map(|(v, _)| v.clone()),
            version_range: version.map(|(_, r)| r),
            tag_end: whole.end(),
        });
    }
    found
}

#[derive(Clone, Debug, PartialEq)]
pub enum CompletionSite {
    /// Typing a package id; `query` is what is typed so far.
    PackageName { query: String },
    /// Typing a version of `package`.
    Version { package: String, query: String },
}

/// What the cursor at `offset` is typing, with where the value starts.
pub fn completion_site(text: &str, offset: usize) -> Option<(CompletionSite, usize)> {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let tag_start = before.rfind('<')?;
    if before[tag_start..].contains('>') {
        return None;
    }
    let tag = &text[tag_start + 1..];
    let name_end = tag.find(|c: char| c.is_whitespace() || c == '/' || c == '>').unwrap_or(tag.len());
    let element = &tag[..name_end];
    if !PACKAGE_ELEMENTS.contains(&element) {
        return None;
    }
    // The attribute whose opening quote is the last unclosed one before the cursor.
    let inside = &before[tag_start..];
    if inside.matches('"').count().is_multiple_of(2) {
        return None;
    }
    let quote = tag_start + inside.rfind('"')?;
    let attr_part = text[tag_start..quote].trim_end().trim_end_matches('=').trim_end();
    let attr_name = attr_part.rsplit(|c: char| c.is_whitespace()).next()?;
    let query = text[quote + 1..offset].to_string();
    let value_start = quote + 1;
    match attr_name {
        "Include" | "Update" => Some((CompletionSite::PackageName { query }, value_start)),
        "Version" | "VersionOverride" => {
            // The package is named in the same tag, before or after the cursor.
            let tag_end = text[tag_start..].find('>').map_or(text.len(), |e| tag_start + e);
            let attrs = &text[tag_start..tag_end];
            let package = ATTRIBUTE.captures_iter(attrs).find(|a| &a[1] == "Include" || &a[1] == "Update").map(|a| a[2].to_string())?;
            Some((CompletionSite::Version { package, query }, value_start))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = r#"<Project Sdk="Microsoft.NET.Sdk">
  <ItemGroup>
    <PackageReference Include="Serilog" Version="3.1.1" />
    <PackageReference Include="Polly">
      <Version>8.0.0</Version>
    </PackageReference>
    <PackageReference Include="NoVersion" />
    <PackageReference Include="$(Prop)" Version="1" />
    <PackageVersion Include="Central" VersionOverride="2.0.0" Version="1.0.0" />
  </ItemGroup>
</Project>"#;

    #[test]
    fn finds_references_and_versions() {
        let refs = package_references(TEXT);
        let summary: Vec<(&str, Option<&str>)> = refs.iter().map(|r| (r.name.as_str(), r.version.as_deref())).collect();
        assert_eq!(summary, vec![("Serilog", Some("3.1.1")), ("Polly", Some("8.0.0")), ("NoVersion", None), ("Central", Some("2.0.0"))]);
        assert_eq!(&TEXT[refs[0].version_range.clone().unwrap()], "3.1.1");
        assert_eq!(&TEXT[refs[1].version_range.clone().unwrap()], "8.0.0");
        assert_eq!(&TEXT[refs[0].name_range.clone()], "Serilog");
        assert!(TEXT[..refs[0].tag_end].ends_with("/>"));
    }

    #[test]
    fn knows_what_the_cursor_types() {
        let text = r#"<PackageReference Include="Seri" Version="3." />"#;
        let at_name = text.find("Seri").unwrap() + 4;
        assert_eq!(completion_site(text, at_name), Some((CompletionSite::PackageName { query: "Seri".into() }, text.find("Seri").unwrap())));
        let at_version = text.find("3.").unwrap() + 2;
        assert_eq!(completion_site(text, at_version).unwrap().0, CompletionSite::Version { package: "Seri".into(), query: "3.".into() });
        assert_eq!(completion_site(text, 5), None);
        assert_eq!(completion_site(r#"<Compile Include="x"#, 18), None);
        let open = r#"<PackageVersion Version="" Include="Polly" />"#;
        assert_eq!(completion_site(open, open.find("\"\"").unwrap() + 1).unwrap().0, CompletionSite::Version { package: "Polly".into(), query: "".into() });
    }
}
