//! Reading assemblies with `ilspycmd`, the command-line ILSpy (`dotnet tool install -g
//! ilspycmd`): the types in an assembly (`-l cise`), a type as C# (`-t`), and the IL of the
//! assembly (`-il`), from which one type's part is cut out.

use std::path::{Path, PathBuf};

/// A type `ilspycmd -l` lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeEntry {
    /// `Class`, `Interface`, `Struct`, `Enum` or `Delegate`.
    pub kind: String,
    /// `Namespace.Type` (nested types as `Outer+Inner`).
    pub name: String,
}

impl TypeEntry {
    pub fn namespace(&self) -> &str {
        self.name.rsplit_once('.').map_or("", |(namespace, _)| namespace)
    }

    pub fn short_name(&self) -> &str {
        self.name.rsplit_once('.').map_or(&self.name, |(_, name)| name)
    }
}

/// The arguments that list the types of `assembly`.
pub fn list_args(assembly: &Path) -> Vec<String> {
    vec!["--disable-updatecheck".into(), "-l".into(), "cisde".into(), assembly.to_string_lossy().into_owned()]
}

/// The arguments that decompile `type_name` of `assembly` to C#.
pub fn csharp_args(assembly: &Path, type_name: &str) -> Vec<String> {
    vec!["--disable-updatecheck".into(), "-t".into(), type_name.into(), assembly.to_string_lossy().into_owned()]
}

/// The arguments that disassemble `assembly` to IL (all of it; see [`il_of_type`]).
pub fn il_args(assembly: &Path) -> Vec<String> {
    vec!["--disable-updatecheck".into(), "-il".into(), assembly.to_string_lossy().into_owned()]
}

/// Parses `ilspycmd -l` output: `Class Calc.MathOps`, one per line. The compiler's
/// `<Module>` and generated types (`<>c`, `<Main>$`) are left out.
pub fn parse_type_list(output: &str) -> Vec<TypeEntry> {
    let mut types: Vec<TypeEntry> = output
        .lines()
        .filter_map(|line| {
            let (kind, name) = line.trim().split_once(' ')?;
            let kind = kind.trim();
            let name = name.trim();
            let known = matches!(kind, "Class" | "Interface" | "Struct" | "Enum" | "Delegate");
            (known && !name.contains('<')).then(|| TypeEntry { kind: kind.into(), name: name.into() })
        })
        .collect();
    types.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    types
}

/// The IL of one type in the IL of a whole assembly: from its `.class` line to its
/// `} // end of class` line.
pub fn il_of_type(il: &str, type_name: &str) -> Option<String> {
    let lines: Vec<&str> = il.lines().collect();
    let start = lines.iter().position(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with(".class") && (trimmed.ends_with(&format!(" {type_name}")) || trimmed.ends_with(&format!(" '{type_name}'")))
    })?;
    let end_marker = format!("// end of class {type_name}");
    let end = lines[start..].iter().position(|line| line.trim_end().ends_with(&end_marker)).map(|i| start + i)?;
    Some(lines[start..=end].join("\n") + "\n")
}

/// The assemblies a project builds into: the `.dll` files of its output folder for
/// `configuration` (`bin/Debug/net8.0`), its own first. `None` when it hasn't been built.
pub fn output_assemblies(project: &Path, configuration: &str, framework: Option<&str>) -> Option<(PathBuf, Vec<PathBuf>)> {
    let bin = project.parent()?.join("bin").join(configuration);
    let dir = match framework {
        Some(framework) if bin.join(framework).is_dir() => bin.join(framework),
        _ => std::fs::read_dir(&bin).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).max()?,
    };
    let own = project.file_stem()?.to_string_lossy().to_lowercase();
    let mut assemblies: Vec<PathBuf> = std::fs::read_dir(&dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dll"))).collect();
    assemblies.sort_by_key(|p| {
        let stem = p.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
        (stem != own, stem)
    });
    Some((dir, assemblies))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_types_without_generated_ones() {
        let output = "Class <Module>\nClass Calc.MathOps\nInterface Calc.IShape\nStruct Calc.Point\nClass Calc.Program+<>c\nEnum Calc.Color\nDelegate Calc.Handler\n";
        let types = parse_type_list(output);
        assert_eq!(types.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["Calc.Color", "Calc.Handler", "Calc.IShape", "Calc.MathOps", "Calc.Point"]);
        assert_eq!(types[2].kind, "Interface");
        assert_eq!((types[3].namespace(), types[3].short_name()), ("Calc", "MathOps"));
    }

    #[test]
    fn cuts_one_type_out_of_the_assemblys_il() {
        let il = "// IL code: Calc\n.class public auto ansi abstract beforefieldinit Calc.Shape\n\textends [System.Runtime]System.Object\n{\n} // end of class Calc.Shape\n\n.class public auto ansi beforefieldinit Calc.Circle\n\textends Calc.Shape\n{\n\t.method public hidebysig virtual instance float64 Area () cil managed\n\t{\n\t} // end of method Circle::Area\n} // end of class Calc.Circle\n";
        let circle = il_of_type(il, "Calc.Circle").unwrap();
        assert!(circle.starts_with(".class public auto ansi beforefieldinit Calc.Circle\n"));
        assert!(circle.trim_end().ends_with("} // end of class Calc.Circle"));
        assert!(!circle.contains("end of class Calc.Shape"), "not the other type");
        assert_eq!(il_of_type(il, "Calc.Missing"), None);
    }

    #[test]
    fn finds_the_build_output() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("App/App.csproj");
        std::fs::create_dir_all(dir.path().join("App/bin/Debug/net8.0")).unwrap();
        std::fs::write(&project, "<Project />").unwrap();
        assert_eq!(output_assemblies(&project, "Release", None), None, "not built in Release");
        for name in ["Zeta.dll", "App.dll", "Alpha.dll", "App.pdb"] {
            std::fs::write(dir.path().join("App/bin/Debug/net8.0").join(name), b"").unwrap();
        }
        let (folder, assemblies) = output_assemblies(&project, "Debug", Some("net8.0")).unwrap();
        assert!(folder.ends_with("bin/Debug/net8.0"));
        let names: Vec<String> = assemblies.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["App.dll", "Alpha.dll", "Zeta.dll"], "the project's own first");
    }
}
