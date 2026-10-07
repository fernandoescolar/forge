//! Languages Forge ships built in, on top of the ones Zed's `languages` crate registers.
//!
//! Each language lives in `src/<name>/` with a `config.toml` and tree-sitter queries,
//! laid out like Zed's own `grammars` crate so files can move between the two.

use std::sync::Arc;

use language::{ContextProvider, LanguageConfig, LanguageRegistry, LoadedLanguage, LspAdapter};
use futures::FutureExt as _;
use language_core::{LanguageQueries, QueryFile, QueryFileContents};
use gpui::BorrowAppContext as _;
use rust_embed::RustEmbed;

pub mod csharp_discovery;
mod csharp_metadata;
pub mod csharp_tasks;
pub mod launch_settings;
mod netcoredbg;
mod omnisharp;
mod csharp_fix_all;

#[derive(RustEmbed)]
#[folder = "src/"]
#[exclude = "*.rs"]
pub(crate) struct LanguageDir;

/// Registers Forge's built-in languages. Call after `languages::init`.
pub fn init(languages: Arc<LanguageRegistry>, cx: &mut gpui::App) {
    csharp_metadata::init(cx);
    csharp_fix_all::init(cx);
    cx.update_default_global(|registry: &mut dap::DapRegistry, _| {
        registry.add_adapter(Arc::new(netcoredbg::NetcoredbgAdapter));
        registry.add_locator(Arc::new(netcoredbg::DotnetTestLocator));
        registry.add_locator(Arc::new(netcoredbg::DotnetRunLocator));
    });
    languages.register_native_grammars([
        ("c_sharp", tree_sitter_c_sharp::LANGUAGE),
        // Named apart from "xml" so an installed XML extension keeps its own grammar.
        ("msbuild_xml", tree_sitter_xml::LANGUAGE_XML),
        ("sln", SLN),
        ("http", HTTP),
    ]);
    register(
        &languages,
        "csharp",
        vec![Arc::new(omnisharp::OmniSharpLspAdapter)],
        Some(Arc::new(csharp_tasks::CSharpContextProvider)),
    );
    // Project files, .props/.targets and .slnx; package completions and version hints
    // come from forge-dotnet.
    register(&languages, "msbuild", vec![], None);
    register(&languages, "sln", vec![], None);
    register(&languages, "http", vec![], Some(Arc::new(HttpContextProvider)));
}

/// `.http` request lines are runnables (`http-request`), so the editor shows a run button
/// on each. Forge sends the request itself when it is clicked (`forge-http`); this task
/// only exists for the button to show.
struct HttpContextProvider;

impl ContextProvider for HttpContextProvider {
    fn associated_tasks(&self, _: Option<gpui::Entity<language::Buffer>>, _: &gpui::App) -> gpui::Task<Option<task::TaskTemplates>> {
        gpui::Task::ready(Some(task::TaskTemplates(vec![task::TaskTemplate {
            label: "Send HTTP request".into(),
            command: "true".into(),
            tags: vec!["http-request".into()],
            ..task::TaskTemplate::default()
        }])))
    }
}

unsafe extern "C" {
    fn tree_sitter_sln() -> *const ();
    fn tree_sitter_http() -> *const ();
}

/// The `.sln` grammar, compiled from `grammars/sln` by build.rs.
pub const SLN: tree_sitter_language::LanguageFn = unsafe { tree_sitter_language::LanguageFn::from_raw(tree_sitter_sln) };
pub const HTTP: tree_sitter_language::LanguageFn = unsafe { tree_sitter_language::LanguageFn::from_raw(tree_sitter_http) };

fn register(
    languages: &LanguageRegistry,
    dir: &'static str,
    adapters: Vec<Arc<dyn LspAdapter>>,
    context: Option<Arc<dyn ContextProvider>>,
) {
    let config = load_config(dir);
    for adapter in adapters {
        languages.register_lsp_adapter(config.name.clone(), adapter);
    }
    languages.register_language(
        config.name.clone(),
        config.grammar.clone(),
        config.matcher.clone(),
        config.hidden,
        None,
        Arc::new(move || {
            let (config, context) = (config.clone(), context.clone());
            async move {
                Ok(LoadedLanguage { config, queries: load_queries(dir), context_provider: context, toolchain_provider: None, manifest_name: None })
            }
            .boxed()
        }),
    );
}

fn load_config(dir: &str) -> LanguageConfig {
    let file = LanguageDir::get(&format!("{dir}/config.toml"))
        .unwrap_or_else(|| panic!("missing config.toml for language {dir:?}"));
    let text = std::str::from_utf8(&file.data).expect("config.toml is not UTF-8");
    toml::from_str(text).unwrap_or_else(|err| panic!("invalid config.toml for {dir:?}: {err}"))
}

/// The language's `.scm` files, as Zed's `grammars` crate loads its own.
fn load_queries(dir: &str) -> LanguageQueries {
    LanguageQueries::from_files(LanguageDir::iter().filter_map(|path| {
        let file_name = path.strip_prefix(dir)?.strip_prefix('/')?;
        let query_file = file_name.parse::<QueryFile>().ok()?;
        let text = String::from_utf8(LanguageDir::get(&path)?.data.into_owned()).ok()?;
        Some(QueryFileContents::new(query_file, text.into()))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use language::Language;

    /// Every query has to compile against the grammar, or the language silently
    /// loses highlighting, outline, brackets, etc.
    #[test]
    fn csharp_queries_compile() {
        let language = Language::new(load_config("csharp"), Some(tree_sitter_c_sharp::LANGUAGE.into()))
            .with_queries(load_queries("csharp"))
            .expect("C# queries compile");
        assert_eq!(language.name().as_ref(), "C#");
        let grammar = language.grammar().unwrap();
        assert!(grammar.highlights_config.is_some(), "highlights loaded");
        assert!(grammar.outline_config.is_some(), "outline loaded");
        assert!(grammar.brackets_config.is_some(), "brackets loaded");
        assert!(grammar.runnable_config.is_some(), "runnables loaded");
    }

    /// File-based apps (`dotnet run app.cs`) open with `#!` and `#:` directives: they parse
    /// (the grammar is patched, see patches/tree-sitter-c-sharp) and are coloured
    /// without throwing off the code that follows.
    #[test]
    fn csharp_file_based_app_header() {
        use streaming_iterator::StreamingIterator as _;

        let source = "#!/usr/bin/env dotnet\n#:sdk Aspire.AppHost.Sdk@13.6.0\n#:package Aspire.Hosting.Redis@13.6.0\n\nusing System;\nvar builder = DistributedApplication.CreateBuilder(args);\n";
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_c_sharp::LANGUAGE.into()).unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(!tree.root_node().has_error(), "{}", tree.root_node().to_sexp());

        let highlights = String::from_utf8(LanguageDir::get("csharp/highlights.scm").unwrap().data.into_owned()).unwrap();
        let query = tree_sitter::Query::new(&tree_sitter_c_sharp::LANGUAGE.into(), &highlights).unwrap();
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut captured = Vec::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
        while let Some(m) = matches.next() {
            for capture in m.captures.iter().filter(|c| c.node.start_position().row < 3) {
                captured.push((query.capture_names()[capture.index as usize].to_string(), source[capture.node.byte_range()].to_string()));
            }
        }
        for expected in [
            ("preproc", "#!/usr/bin/env dotnet"),
            ("preproc", "#:sdk"),
            ("string", "Aspire.AppHost.Sdk@13.6.0"),
            ("preproc", "#:package"),
            ("string", "Aspire.Hosting.Redis@13.6.0"),
        ] {
            assert!(captured.iter().any(|(name, text)| name == expected.0 && text == expected.1), "{expected:?} not in {captured:?}");
        }
    }

    #[test]
    fn msbuild_and_solution_queries_compile() {
        let msbuild = Language::new(load_config("msbuild"), Some(tree_sitter_xml::LANGUAGE_XML.into()))
            .with_queries(load_queries("msbuild"))
            .expect("MSBuild queries compile");
        let grammar = msbuild.grammar().unwrap();
        assert!(grammar.highlights_config.is_some() && grammar.outline_config.is_some() && grammar.brackets_config.is_some());

        let sln = Language::new(load_config("sln"), Some(SLN.into())).with_queries(load_queries("sln")).expect("solution queries compile");
        let grammar = sln.grammar().unwrap();
        assert!(grammar.highlights_config.is_some() && grammar.outline_config.is_some());
    }

    /// `.http` files: queries compile, the grammar reads requests without errors and each
    /// request line is a runnable.
    #[test]
    fn http_language() {
        use streaming_iterator::StreamingIterator as _;

        let http = Language::new(load_config("http"), Some(HTTP.into())).with_queries(load_queries("http")).expect("HTTP queries compile");
        assert!(http.grammar().unwrap().highlights_config.is_some());
        let text = "@host = http://localhost:5000\n\n# @name login\nPOST {{host}}/login HTTP/1.1\nContent-Type: application/json\n\n{ \"user\": \"{{name}}\" }\n\n###\nGET https://example.com\n// done";
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&HTTP.into()).unwrap();
        let tree = parser.parse(text, None).unwrap();
        assert!(!tree.root_node().has_error(), "{}", tree.root_node().to_sexp());
        let source = String::from_utf8(LanguageDir::get("http/runnables.scm").unwrap().data.into_owned()).unwrap();
        let query = tree_sitter::Query::new(&HTTP.into(), &source).unwrap();
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut rows = Vec::new();
        let mut matches = cursor.matches(&query, tree.root_node(), text.as_bytes());
        while let Some(m) = matches.next() {
            rows.extend(m.captures.iter().map(|c| c.node.start_position().row));
        }
        assert_eq!(rows, [3, 9], "one run button per request line");
    }

    /// The `.sln` grammar reads a real solution without errors.
    #[test]
    fn sln_grammar_parses_solutions() {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&SLN.into()).unwrap();
        let text = include_str!("../../dotnet-model/tests/fixtures/sample.sln");
        let tree = parser.parse(text, None).unwrap();
        assert!(!tree.root_node().has_error(), "{}", tree.root_node().to_sexp());
        assert_eq!(tree.root_node().named_child_count(), 7, "header, comment, version, 3 projects and Global: {}", tree.root_node().to_sexp());
    }

    /// The runnables query marks test methods and their classes for xUnit, NUnit and MSTest,
    /// and nothing else.
    #[test]
    fn csharp_runnables_find_tests() {
        use streaming_iterator::StreamingIterator as _;

        let source = r#"
namespace Demo.Tests;

public class MathTests
{
    [Fact]
    public void Adds() { }

    [Xunit.Theory]
    [InlineData(1)]
    public void Doubles(int x) { }

    [TestCase(1, 2)]
    public void NUnitCase(int a, int b) { }

    [TestMethodAttribute]
    public void MsTest() { }

    public void Helper() { }
}

public class NotATestClass
{
    [Obsolete]
    public void Old() { }
}
"#;
        let language = Language::new(load_config("csharp"), Some(tree_sitter_c_sharp::LANGUAGE.into()))
            .with_queries(load_queries("csharp"))
            .unwrap();
        let config = language.grammar().unwrap().runnable_config.as_ref().unwrap();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_c_sharp::LANGUAGE.into()).unwrap();
        let tree = parser.parse(source, None).unwrap();

        let run_index = config.query.capture_index_for_name("run").unwrap();
        let mut found = std::collections::BTreeSet::new();
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches = cursor.matches(&config.query, tree.root_node(), source.as_bytes());
        while let Some(m) = matches.next() {
            let tag = config.query.property_settings(m.pattern_index)[0].value.clone().unwrap();
            for capture in m.captures.iter().filter(|c| c.index == run_index) {
                found.insert(format!("{tag}:{}", &source[capture.node.byte_range()]));
            }
        }
        assert_eq!(
            found.into_iter().collect::<Vec<_>>(),
            [
                "csharp-test-class:MathTests",
                "csharp-test:Adds",
                "csharp-test:Doubles",
                "csharp-test:MsTest",
                "csharp-test:NUnitCase",
            ]
        );
    }
}
