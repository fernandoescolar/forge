# tree-sitter-c-sharp, patched for Forge

This is [tree-sitter-c-sharp](https://github.com/tree-sitter/tree-sitter-c-sharp) 0.23.5 as
published on crates.io, with one change: it parses the `#:` directives of .NET 10 file-based
apps (`#:sdk`, `#:package`, `#:property`, `#:project`…). Upstream doesn't yet
([tree-sitter-c-sharp#431](https://github.com/tree-sitter/tree-sitter-c-sharp/issues/431)), and
without it the top of an `apphost.cs` parses as an error that runs into the code below it.

The change is a patch to `grammar.js` (`0001-file-based-app-directives.patch`, marked `Forge:`): an `ignored_directive` rule, allowed at the
top of a file after the optional `#!` line, as Roslyn allows them, with a `directive_name`
(`#:package`) and a `directive_argument` (`Aspire.Hosting@13.6.0`). `scripts/patch-grammars.sh` (run by `scripts/apply-zed-patches.sh`) downloads the crate,
applies the patches and regenerates the parser into `vendor/tree-sitter-c-sharp` with a
tree-sitter CLI that writes the ABI Forge's tree-sitter reads (15); the root `Cargo.toml`
uses that copy through `[patch.crates-io]`. The generated parser is some 25 MB, so it isn't
kept in the repository.

To change the grammar: edit `vendor/tree-sitter-c-sharp/grammar.js`, regenerate there
(`npx tree-sitter-cli@0.25.10 generate --abi 15`), test, then write the patch against the
pristine crate (`diff -u <crate>/grammar.js vendor/tree-sitter-c-sharp/grammar.js`).

Drop it when upstream parses the directives: remove the `[patch.crates-io]` line, this
folder and the script, and update `crates/forge-languages/src/csharp/highlights.scm` to the
upstream node names.
