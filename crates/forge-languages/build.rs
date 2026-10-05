//! Compiles the tree-sitter grammars Forge carries itself (generated C parsers under
//! `grammars/<name>/src`).

fn main() {
    for name in ["sln", "http"] {
        let dir = std::path::Path::new("grammars").join(name).join("src");
        println!("cargo:rerun-if-changed={}", dir.join("parser.c").display());
        cc::Build::new()
            .include(&dir)
            .file(dir.join("parser.c"))
            .flag_if_supported("-Wno-unused-parameter")
            .flag_if_supported("-Wno-unused-but-set-variable")
            .warnings(false)
            .compile(&format!("tree-sitter-{name}"));
    }
}
