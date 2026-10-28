//! Language, category, zone, test and manifest classification — all name-based and cheap.
//! Content-based refinement (shebangs, `@generated` markers, binary sniffing) lives in
//! `content`.

use crate::model::{Category, Flags, Kind, NodeId, Tree, Zone};

/// How to recognise comments and strings when stripping comments.
#[derive(Clone, Copy, Debug)]
pub struct Syntax {
    pub line: &'static [&'static str],
    pub block: Option<(&'static str, &'static str)>,
    pub nested: bool,
    pub strings: Strings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strings {
    /// `"…"`; `'x'` is a short char literal (C, C++, Java, C#…).
    C,
    /// Like C, plus raw strings `r"…"`/`r#"…"#`; `'a` may be a lifetime.
    Rust,
    /// Like C, plus backtick raw strings.
    Go,
    /// `'…'`, `"…"`, and backtick template literals.
    Js,
    /// `'…'`, `"…"`, and triple-quoted strings.
    Python,
    /// `'…'` and `"…"` (shell, Ruby, PHP, Lua, SQL…).
    Script,
    /// No string awareness.
    None,
}

/// Which import extractor applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImportLang {
    Rust,
    Python,
    Js,
    Go,
    Jvm,
    CFamily,
    /// `import "a/b.proto";` style: Protobuf, Mojom.
    PathImport,
}

pub struct Lang {
    pub name: &'static str,
    pub exts: &'static [&'static str],
    pub files: &'static [&'static str],
    pub category: Category,
    pub syntax: Syntax,
    pub imports: Option<ImportLang>,
    pub color: &'static str,
}

const C_SYNTAX: Syntax = Syntax {
    line: &["//"],
    block: Some(("/*", "*/")),
    nested: false,
    strings: Strings::C,
};
const NESTED_C: Syntax = Syntax {
    nested: true,
    ..C_SYNTAX
};
const RUST_SYNTAX: Syntax = Syntax {
    nested: true,
    strings: Strings::Rust,
    ..C_SYNTAX
};
const GO_SYNTAX: Syntax = Syntax {
    strings: Strings::Go,
    ..C_SYNTAX
};
const JS_SYNTAX: Syntax = Syntax {
    strings: Strings::Js,
    ..C_SYNTAX
};
const CSS_SYNTAX: Syntax = Syntax {
    line: &[],
    block: Some(("/*", "*/")),
    nested: false,
    strings: Strings::Script,
};
const SCSS_SYNTAX: Syntax = Syntax {
    strings: Strings::Script,
    ..C_SYNTAX
};
const HASH: Syntax = Syntax {
    line: &["#"],
    block: None,
    nested: false,
    strings: Strings::Script,
};
const PY_SYNTAX: Syntax = Syntax {
    strings: Strings::Python,
    ..HASH
};
const HASH_PLAIN: Syntax = Syntax {
    strings: Strings::None,
    ..HASH
};
const SQL_SYNTAX: Syntax = Syntax {
    line: &["--"],
    block: Some(("/*", "*/")),
    nested: false,
    strings: Strings::Script,
};
const LUA_SYNTAX: Syntax = Syntax {
    line: &["--"],
    block: Some(("--[[", "]]")),
    nested: false,
    strings: Strings::Script,
};
const HASKELL_SYNTAX: Syntax = Syntax {
    line: &["--"],
    block: Some(("{-", "-}")),
    nested: true,
    strings: Strings::C,
};
const ML_SYNTAX: Syntax = Syntax {
    line: &[],
    block: Some(("(*", "*)")),
    nested: true,
    strings: Strings::C,
};
const PERCENT: Syntax = Syntax {
    line: &["%"],
    block: None,
    nested: false,
    strings: Strings::None,
};
const SEMI: Syntax = Syntax {
    line: &[";"],
    block: None,
    nested: false,
    strings: Strings::Script,
};
const XML_SYNTAX: Syntax = Syntax {
    line: &[],
    block: Some(("<!--", "-->")),
    nested: false,
    strings: Strings::None,
};
const PHP_SYNTAX: Syntax = Syntax {
    line: &["//", "#"],
    block: Some(("/*", "*/")),
    nested: false,
    strings: Strings::Script,
};
const PS_SYNTAX: Syntax = Syntax {
    line: &["#"],
    block: Some(("<#", "#>")),
    nested: false,
    strings: Strings::Script,
};
const BATCH_SYNTAX: Syntax = Syntax {
    line: &["REM ", "rem ", "::"],
    block: None,
    nested: false,
    strings: Strings::None,
};
const NONE: Syntax = Syntax {
    line: &[],
    block: None,
    nested: false,
    strings: Strings::None,
};

macro_rules! lang {
    ($name:expr, [$($e:expr),*], [$($f:expr),*], $cat:ident, $syn:expr, $imp:expr, $color:expr) => {
        Lang {
            name: $name,
            exts: &[$($e),*],
            files: &[$($f),*],
            category: Category::$cat,
            syntax: $syn,
            imports: $imp,
            color: $color,
        }
    };
}

use ImportLang as I;

/// Language table. Order matters only for display ties. Colours follow GitHub linguist.
#[rustfmt::skip]
pub static LANGS: &[Lang] = &[
    lang!("Rust", ["rs"], [], Code, RUST_SYNTAX, Some(I::Rust), "#dea584"),
    lang!("Python", ["py", "pyi", "pyw"], [], Code, PY_SYNTAX, Some(I::Python), "#3572A5"),
    lang!("JavaScript", ["js", "mjs", "cjs", "jsx"], [], Code, JS_SYNTAX, Some(I::Js), "#f1e05a"),
    lang!("TypeScript", ["ts", "tsx", "mts", "cts"], [], Code, JS_SYNTAX, Some(I::Js), "#3178c6"),
    lang!("Vue", ["vue"], [], Code, JS_SYNTAX, Some(I::Js), "#41b883"),
    lang!("Svelte", ["svelte"], [], Code, JS_SYNTAX, Some(I::Js), "#ff3e00"),
    lang!("Go", ["go"], [], Code, GO_SYNTAX, Some(I::Go), "#00ADD8"),
    lang!("Java", ["java"], [], Code, C_SYNTAX, Some(I::Jvm), "#b07219"),
    lang!("Kotlin", ["kt", "kts"], [], Code, NESTED_C, Some(I::Jvm), "#A97BFF"),
    lang!("Scala", ["scala", "sc"], [], Code, NESTED_C, Some(I::Jvm), "#c22d40"),
    lang!("Groovy", ["groovy", "gradle"], [], Code, C_SYNTAX, Some(I::Jvm), "#4298b8"),
    lang!("C", ["c"], [], Code, C_SYNTAX, Some(I::CFamily), "#555555"),
    lang!("C++", ["cc", "cpp", "cxx", "c++", "ipp", "inl", "tcc", "tpp"], [], Code, C_SYNTAX, Some(I::CFamily), "#f34b7d"),
    lang!("C/C++ Header", ["h", "hh", "hpp", "hxx", "h++"], [], Code, C_SYNTAX, Some(I::CFamily), "#a8556f"),
    lang!("Objective-C", ["m"], [], Code, C_SYNTAX, Some(I::CFamily), "#438eff"),
    lang!("Objective-C++", ["mm"], [], Code, C_SYNTAX, Some(I::CFamily), "#6866fb"),
    lang!("CUDA", ["cu", "cuh"], [], Code, C_SYNTAX, Some(I::CFamily), "#3A4E3A"),
    lang!("C#", ["cs", "csx"], [], Code, C_SYNTAX, None, "#178600"),
    lang!("F#", ["fs", "fsi", "fsx"], [], Code, ML_SYNTAX, None, "#b845fc"),
    lang!("Swift", ["swift"], [], Code, NESTED_C, None, "#F05138"),
    lang!("Dart", ["dart"], [], Code, NESTED_C, None, "#00B4AB"),
    lang!("Ruby", ["rb", "rake", "gemspec"], ["Rakefile", "Gemfile"], Code, HASH, None, "#701516"),
    lang!("PHP", ["php"], [], Code, PHP_SYNTAX, None, "#4F5D95"),
    lang!("Perl", ["pl", "pm"], [], Code, HASH, None, "#0298c3"),
    lang!("Lua", ["lua"], [], Code, LUA_SYNTAX, None, "#000080"),
    lang!("R", ["r"], [], Code, HASH, None, "#198CE7"),
    lang!("Julia", ["jl"], [], Code, HASH, None, "#a270ba"),
    lang!("Elixir", ["ex", "exs"], [], Code, HASH, None, "#6e4a7e"),
    lang!("Erlang", ["erl", "hrl"], [], Code, PERCENT, None, "#B83998"),
    lang!("Haskell", ["hs", "lhs"], [], Code, HASKELL_SYNTAX, None, "#5e5086"),
    lang!("OCaml", ["ml", "mli"], [], Code, ML_SYNTAX, None, "#ef7a08"),
    lang!("Clojure", ["clj", "cljs", "cljc"], [], Code, SEMI, None, "#db5855"),
    lang!("Zig", ["zig"], [], Code, C_SYNTAX, None, "#ec915c"),
    lang!("Nim", ["nim"], [], Code, HASH, None, "#ffc200"),
    lang!("Shell", ["sh", "bash", "zsh", "fish", "ksh"], [], Code, HASH, None, "#89e051"),
    lang!("PowerShell", ["ps1", "psm1", "psd1"], [], Code, PS_SYNTAX, None, "#012456"),
    lang!("Batch", ["bat", "cmd"], [], Code, BATCH_SYNTAX, None, "#C1F12E"),
    lang!("SQL", ["sql"], [], Code, SQL_SYNTAX, None, "#e38c00"),
    lang!("HTML", ["html", "htm", "xhtml"], [], Code, XML_SYNTAX, None, "#e34c26"),
    lang!("CSS", ["css"], [], Code, CSS_SYNTAX, None, "#563d7c"),
    lang!("SCSS", ["scss", "sass"], [], Code, SCSS_SYNTAX, None, "#c6538c"),
    lang!("Less", ["less"], [], Code, SCSS_SYNTAX, None, "#1d365d"),
    lang!("GN", ["gn", "gni"], [], Code, HASH, None, "#4285f4"),
    lang!("Starlark", ["bzl", "star"], ["BUILD", "BUILD.bazel", "WORKSPACE", "WORKSPACE.bazel", "MODULE.bazel"], Code, HASH, None, "#76d275"),
    lang!("CMake", ["cmake"], ["CMakeLists.txt"], Code, HASH, None, "#DA3434"),
    lang!("Makefile", ["mk", "mak"], ["Makefile", "makefile", "GNUmakefile"], Code, HASH_PLAIN, None, "#427819"),
    lang!("Dockerfile", ["dockerfile"], ["Dockerfile", "Containerfile"], Code, HASH_PLAIN, None, "#384d54"),
    lang!("Protocol Buffers", ["proto"], [], Code, C_SYNTAX, Some(I::PathImport), "#5a7ab8"),
    lang!("Mojom", ["mojom"], [], Code, C_SYNTAX, Some(I::PathImport), "#7e57c2"),
    lang!("IDL", ["idl", "webidl"], [], Code, C_SYNTAX, None, "#a3522f"),
    lang!("GraphQL", ["graphql", "gql"], [], Code, HASH, None, "#e10098"),
    lang!("HCL", ["tf", "tfvars", "hcl"], [], Code, PHP_SYNTAX, None, "#844FBA"),
    lang!("Nix", ["nix"], [], Code, HASH, None, "#7e7eff"),
    lang!("Assembly", ["s", "asm", "nasm"], [], Code, SEMI, None, "#6E4C13"),
    lang!("GLSL", ["glsl", "vert", "frag", "comp", "geom"], [], Code, C_SYNTAX, None, "#5686a5"),
    lang!("HLSL", ["hlsl", "fx"], [], Code, C_SYNTAX, None, "#aace60"),
    lang!("WGSL", ["wgsl"], [], Code, C_SYNTAX, None, "#1a5e9a"),
    lang!("Solidity", ["sol"], [], Code, C_SYNTAX, None, "#AA6746"),
    lang!("Fortran", ["f", "f90", "f95", "f03", "for"], [], Code, NONE, None, "#4d41b1"),
    lang!("Visual Basic", ["vb", "vbs"], [], Code, NONE, None, "#945db7"),
    lang!("Markdown", ["md", "markdown", "mdx"], [], Docs, NONE, None, "#083fa1"),
    lang!("reStructuredText", ["rst"], [], Docs, NONE, None, "#141414"),
    lang!("AsciiDoc", ["adoc", "asciidoc"], [], Docs, NONE, None, "#73a0c5"),
    lang!("TeX", ["tex", "sty", "cls", "bib"], [], Docs, PERCENT, None, "#3D6117"),
    lang!("Text", ["txt", "text"], ["README", "LICENSE", "COPYING", "AUTHORS", "NOTICE", "CHANGELOG", "OWNERS"], Docs, NONE, None, "#9e9e9e"),
    lang!("JSON", ["json", "jsonc", "json5"], [], Config, NONE, None, "#292929"),
    lang!("YAML", ["yml", "yaml"], [], Config, HASH, None, "#cb171e"),
    lang!("TOML", ["toml"], [], Config, HASH, None, "#9c4221"),
    lang!("XML", ["xml", "xsd", "xsl", "xslt", "plist", "csproj", "vcxproj", "props", "targets", "resx", "storyboard", "xib"], [], Config, XML_SYNTAX, None, "#0060ac"),
    lang!("INI", ["ini", "cfg", "conf", "properties", "editorconfig"], [".gitignore", ".gitattributes", ".dockerignore", ".npmrc", ".env.example", "DEPS"], Config, HASH, None, "#d1dbe0"),
    lang!("CSV", ["csv", "tsv"], [], Data, NONE, None, "#237346"),
];

pub fn lang(idx: u16) -> &'static Lang {
    &LANGS[idx as usize]
}

pub fn lang_by_name(name: &str) -> Option<u16> {
    LANGS.iter().position(|l| l.name == name).map(|i| i as u16)
}

/// Lower-cased final extension, if any (`archive.tar.gz` → `gz`).
fn extension(name: &str) -> Option<String> {
    let dot = name.rfind('.')?;
    if dot == 0 || dot + 1 == name.len() {
        return None;
    }
    Some(name[dot + 1..].to_ascii_lowercase())
}

/// Language index by file name. Exact names win over extensions.
pub fn detect_lang(name: &str) -> Option<u16> {
    if let Some(i) = LANGS.iter().position(|l| l.files.contains(&name)) {
        return Some(i as u16);
    }
    if name.starts_with("Dockerfile") || name.ends_with(".Dockerfile") {
        return lang_by_name("Dockerfile");
    }
    let ext = extension(name)?;
    // `.S` (preprocessed asm) is caught by lowercasing; `.R` likewise.
    LANGS
        .iter()
        .position(|l| l.exts.contains(&ext.as_str()))
        .map(|i| i as u16)
}

/// Language from a `#!` interpreter line.
pub fn lang_from_shebang(first_line: &str) -> Option<u16> {
    let line = first_line.strip_prefix("#!")?;
    let mut parts = line.split_whitespace();
    let mut prog = parts.next()?.rsplit('/').next()?;
    if prog == "env" {
        prog = parts.find(|p| !p.starts_with('-'))?;
    }
    let prog = prog.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    let name = match prog {
        "python" => "Python",
        "node" | "deno" | "bun" => "JavaScript",
        "bash" | "sh" | "zsh" | "dash" | "ksh" | "fish" => "Shell",
        "ruby" => "Ruby",
        "perl" => "Perl",
        "php" => "PHP",
        "lua" => "Lua",
        "pwsh" => "PowerShell",
        _ => return None,
    };
    lang_by_name(name)
}

const LOCKFILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "bun.lock",
    "poetry.lock",
    "Pipfile.lock",
    "uv.lock",
    "pdm.lock",
    "composer.lock",
    "Gemfile.lock",
    "go.sum",
    "flake.lock",
    "packages.lock.json",
    "Podfile.lock",
    "mix.lock",
    "pubspec.lock",
    "gradle.lockfile",
];

fn category_from_ext(ext: &str) -> Option<Category> {
    use Category::*;
    Some(match ext {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico" | "icns" | "tif"
        | "tiff" | "psd" | "heic" | "heif" | "avif" | "raw" | "cr2" | "nef" | "xcf" | "ai"
        | "eps" => Image,
        "mp3" | "wav" | "flac" | "ogg" | "oga" | "m4a" | "aac" | "opus" | "wma" | "mid"
        | "midi" | "mp4" | "m4v" | "mkv" | "mov" | "avi" | "webm" | "wmv" | "flv" | "mpg"
        | "mpeg" | "3gp" => Media,
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "txz" | "7z" | "rar" | "zst" | "lz4"
        | "jar" | "war" | "ear" | "whl" | "egg" | "deb" | "rpm" | "dmg" | "iso" | "apk" | "aab"
        | "ipa" | "nupkg" | "crate" | "gem" | "cab" | "msi" | "pkg" | "vsix" | "xpi" | "crx" => {
            Archive
        }
        "exe" | "dll" | "so" | "dylib" | "a" | "lib" | "o" | "obj" | "class" | "pyc" | "pyo"
        | "pyd" | "wasm" | "node" | "pdb" | "ilk" | "exp" | "rlib" | "rmeta" | "bin" | "elf"
        | "ko" | "dex" | "out" | "pch" | "gch" | "nib" => Binary,
        "ttf" | "otf" | "woff" | "woff2" | "eot" | "pfb" | "ttc" => Font,
        "parquet" | "arrow" | "feather" | "avro" | "orc" | "db" | "sqlite" | "sqlite3" | "mdb"
        | "npy" | "npz" | "h5" | "hdf5" | "pkl" | "pickle" | "pt" | "pth" | "onnx"
        | "safetensors" | "gguf" | "ckpt" | "tfrecord" | "ndjson" | "jsonl" | "log" | "dat"
        | "dump" | "bak" | "xls" | "xlsx" | "ods" => Data,
        "pdf" | "doc" | "docx" | "ppt" | "pptx" | "odt" | "odp" | "rtf" | "epub" | "pages"
        | "key" | "numbers" => Docs,
        "map" => Generated,
        _ => return None,
    })
}

/// Category + language for a file name.
pub fn classify_file(name: &str) -> (Category, Option<u16>) {
    if LOCKFILES.contains(&name) {
        return (Category::Generated, detect_lang(name));
    }
    let lower = name.to_ascii_lowercase();
    if lower.contains(".min.")
        || lower.ends_with(".pb.go")
        || lower.ends_with("_pb2.py")
        || lower.ends_with(".pb.h")
        || lower.ends_with(".pb.cc")
        || lower.ends_with(".g.dart")
        || lower.contains(".generated.")
        || lower.ends_with(".designer.cs")
    {
        return (Category::Generated, detect_lang(name));
    }
    if let Some(l) = detect_lang(name) {
        return (lang(l).category, Some(l));
    }
    if let Some(ext) = extension(name)
        && let Some(cat) = category_from_ext(&ext)
    {
        return (cat, None);
    }
    (Category::Other, None)
}

/// Test-file heuristics across ecosystems (incl. Chromium's `_unittest.cc` / `_browsertest.cc`).
pub fn is_test_path(rel_path: &str, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let stem = lower.split('.').next().unwrap_or("");
    if stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("_tests")
        || stem.ends_with("_spec")
        || stem.ends_with("_unittest")
        || stem.ends_with("_browsertest")
        || stem.ends_with("_unittests")
        || lower.contains(".test.")
        || lower.contains(".spec.")
        || (name.ends_with("Test.java")
            || name.ends_with("Tests.java")
            || name.ends_with("Test.kt"))
        || (name.ends_with("Tests.cs") || name.ends_with("Test.cs"))
        || name == "conftest.py"
    {
        return true;
    }
    rel_path.split('/').any(|seg| {
        matches!(
            seg,
            "test"
                | "tests"
                | "__tests__"
                | "spec"
                | "specs"
                | "testing"
                | "testdata"
                | "test_data"
                | "fixtures"
                | "e2e"
        )
    })
}

const MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "requirements.txt",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "CMakeLists.txt",
    "Makefile",
    "meson.build",
    "BUILD.gn",
    "BUILD.bazel",
    "BUILD",
    "WORKSPACE",
    "MODULE.bazel",
    "DEPS",
    "Gemfile",
    "composer.json",
    "Dockerfile",
    "docker-compose.yml",
    "docker-compose.yaml",
    "compose.yaml",
    "tsconfig.json",
    "deno.json",
    "mix.exs",
    "build.zig",
    "Package.swift",
    "pubspec.yaml",
    "flake.nix",
    "main.rs",
    "lib.rs",
    "main.go",
    "main.py",
    "__main__.py",
    "manage.py",
    "app.py",
    "Program.cs",
    "main.c",
    "main.cc",
    "main.cpp",
    "README.md",
    "CLAUDE.md",
    "AGENTS.md",
];

pub fn is_manifest(name: &str) -> bool {
    MANIFESTS.contains(&name) || name.ends_with(".csproj") || name.ends_with(".sln")
}

/// Reason for `build/` next to a manifest: output by convention, but also where projects
/// keep hand-written build scripts (hono). Git tracking overrides it: see
/// [`settle_tracked_outputs`]. Must stay distinct from [`BUILD_OUTPUT`] (decision D-14).
pub const AMBIGUOUS_OUTPUT: &str = "build directory next to its manifest";

/// Reason for names that are output whether or not git tracks them (`target/`, `dist/`,
/// `out/`, .NET `bin/`/`obj/`): a committed one is reported as a committed artifact.
const BUILD_OUTPUT: &str = "build output next to its manifest";

/// Origin implied by a directory's name, given its siblings, with a human-readable reason.
/// Ambiguous names (`target`, `build`, `dist`, …) need a matching manifest next to them.
fn zone_for_dir(name: &str, sibling_names: &[&str]) -> Option<(Zone, &'static str)> {
    let has = |n: &str| sibling_names.contains(&n);
    Some(match name {
        ".git" | ".hg" | ".svn" | ".jj" | ".bzr" | "_darcs" => {
            (Zone::Vcs, "version-control directory")
        }
        "node_modules" | "bower_components" | "jspm_packages" | ".pnpm-store" | "Pods"
        | "Carthage" | ".bundle" | "site-packages" | "dist-packages" | ".venv" | "venv"
        | ".virtualenv" | ".conda" | ".pub-cache" | ".m2" | ".ivy2" | ".nuget" | ".rustup"
        | ".npm" | ".gem" => (Zone::Dependency, "package-manager directory"),
        "__pycache__" | ".pytest_cache" | ".mypy_cache" | ".ruff_cache" | ".tox" | ".nox"
        | ".gradle" | ".next" | ".nuxt" | ".svelte-kit" | ".parcel-cache" | ".turbo"
        | ".angular" | ".expo" | "DerivedData" | ".terraform" | ".cache" | ".eslintcache"
        | ".sass-cache" | ".dart_tool" | ".cxx" | ".build" | "zig-cache" | ".zig-cache"
        | "zig-out" | "_build" | ".stack-work" | "htmlcov" => {
            (Zone::Artifact, "tool cache / build directory")
        }
        "target" if has("Cargo.toml") || has("pom.xml") || has("build.sbt") => {
            (Zone::Artifact, BUILD_OUTPUT)
        }
        "build"
            if has("build.gradle")
                || has("build.gradle.kts")
                || has("setup.py")
                || has("pyproject.toml")
                || has("package.json") =>
        {
            (Zone::Artifact, AMBIGUOUS_OUTPUT)
        }
        "dist" if has("package.json") || has("setup.py") || has("pyproject.toml") => {
            (Zone::Artifact, BUILD_OUTPUT)
        }
        "out" if has("package.json") || has("tsconfig.json") => (Zone::Artifact, BUILD_OUTPUT),
        "bin" | "obj" if sibling_names.iter().any(|n| n.ends_with(".csproj")) => {
            (Zone::Artifact, BUILD_OUTPUT)
        }
        // `deps` is also a common module name (`src/deps/`): only next to a build file.
        "deps" if has("mix.exs") || has("rebar.config") => {
            (Zone::Dependency, "package-manager directory")
        }
        "deps"
            if has("Makefile")
                || has("CMakeLists.txt")
                || has("configure")
                || has("meson.build") =>
        {
            (Zone::Vendored, "third-party directory name")
        }
        "third_party" | "third-party" | "thirdparty" | "3rdparty" | "vendor" | "vendors"
        | "external" | "extern" => (Zone::Vendored, "third-party directory name"),
        n if n.starts_with("cmake-build-") || n.starts_with("bazel-") => {
            (Zone::Artifact, "build output directory name")
        }
        _ => return None,
    })
}

/// Marker files that classify the directory containing them.
fn zone_from_markers(dir_name: &str, child_names: &[&str]) -> Option<(Zone, &'static str)> {
    if dir_name == ".cargo" && child_names.contains(&"registry") {
        Some((Zone::Dependency, "Cargo home (registry cache)"))
    } else if child_names.contains(&"CACHEDIR.TAG") {
        Some((Zone::Artifact, "CACHEDIR.TAG marker"))
    } else if child_names.contains(&"pyvenv.cfg") {
        Some((Zone::Dependency, "pyvenv.cfg (virtualenv)"))
    } else {
        None
    }
}

/// A directory where the origin changes, and why.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ZoneRoot {
    pub node: NodeId,
    pub zone: Zone,
    pub reason: &'static str,
}

pub fn classify_tree(tree: &mut Tree) -> Vec<ZoneRoot> {
    let n = tree.len();
    let mut roots = Vec::new();
    // Zones are inherited. In pre-order a directory's zone is final before we visit it,
    // so each directory settles its own markers, then assigns its children's zones
    // (reading the sibling list once).
    if n > 0 {
        tree.get_mut(0).zone = Zone::Source;
    }
    for id in 0..n as NodeId {
        if tree.get(id).kind != Kind::Dir {
            continue;
        }
        let children: Vec<NodeId> = tree.children(id).collect();
        let (marker, assigned) = {
            let names: Vec<&str> = children.iter().map(|&c| &*tree.get(c).name).collect();
            let own = tree.get(id).zone;
            let marker = if own.is_analysed() {
                zone_from_markers(&tree.get(id).name, &names)
            } else {
                None
            };
            let zone = marker.map_or(own, |m| m.0);
            let assigned: Vec<(Zone, Option<&'static str>)> = children
                .iter()
                .map(|&c| {
                    let child = tree.get(c);
                    if zone.is_analysed()
                        && child.kind == Kind::Dir
                        && let Some((z, reason)) = zone_for_dir(&child.name, &names)
                    {
                        return (z, Some(reason));
                    }
                    // Submodules and linked worktrees have a `.git` *file* (a gitdir pointer).
                    if child.kind == Kind::File && &*child.name == ".git" {
                        return (Zone::Vcs, None);
                    }
                    (zone, None)
                })
                .collect();
            (marker, assigned)
        };
        if let Some((z, reason)) = marker {
            tree.get_mut(id).zone = z;
            roots.push(ZoneRoot {
                node: id,
                zone: z,
                reason,
            });
        }
        for (&c, &(z, reason)) in children.iter().zip(&assigned) {
            tree.get_mut(c).zone = z;
            if let Some(reason) = reason {
                roots.push(ZoneRoot {
                    node: c,
                    zone: z,
                    reason,
                });
            }
        }
    }

    for id in 0..n as NodeId {
        if tree.get(id).kind == Kind::File {
            classify_node_file(tree, id);
        }
    }
    roots
}

fn classify_node_file(tree: &mut Tree, id: NodeId) {
    let node = tree.get(id);
    let name = node.name.to_string();
    let (cat, lang) = classify_file(&name);
    let analysed = node.zone.is_analysed();
    let rel = if analysed {
        tree.rel_path(id)
    } else {
        String::new()
    };
    let node = tree.get_mut(id);
    node.category = cat;
    node.lang = lang;
    if analysed {
        if is_test_path(&rel, &name) {
            node.flags.set(Flags::TEST);
        }
        if is_manifest(&name) {
            node.flags.set(Flags::MANIFEST);
        }
    }
}

/// A `build/` directory that git tracks is hand-written code (hono's holds build scripts):
/// return it to its parent's origin. Run after git status. Other output names stay
/// artifacts when tracked, so committing them is reported.
pub fn settle_tracked_outputs(tree: &mut Tree, roots: &mut Vec<ZoneRoot>) {
    let mut keep = Vec::with_capacity(roots.len());
    for r in roots.drain(..) {
        let tracked = r.reason == AMBIGUOUS_OUTPUT
            && tree.subtree(r.node).any(|s| {
                tree.get(s).is_file() && tree.get(s).git == Some(crate::model::GitState::Tracked)
            });
        if !tracked {
            keep.push(r);
            continue;
        }
        let parent_zone = tree.get(tree.get(r.node).parent).zone;
        for s in tree.subtree(r.node) {
            if tree.get(s).zone == Zone::Artifact {
                tree.get_mut(s).zone = parent_zone;
                if tree.get(s).kind == Kind::File {
                    classify_node_file(tree, s);
                }
            }
        }
    }
    *roots = keep;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_languages() {
        let name = |f: &str| detect_lang(f).map(|i| lang(i).name);
        assert_eq!(name("main.rs"), Some("Rust"));
        assert_eq!(name("App.TSX"), Some("TypeScript"));
        assert_eq!(name("CMakeLists.txt"), Some("CMake"));
        assert_eq!(name("BUILD.gn"), Some("GN"));
        assert_eq!(name("foo.mojom"), Some("Mojom"));
        assert_eq!(name("Dockerfile.dev"), Some("Dockerfile"));
        assert_eq!(name("noext"), None);
        assert_eq!(name(".bashrc"), None);
    }

    #[test]
    fn shebangs() {
        let name = |f: &str| lang_from_shebang(f).map(|i| lang(i).name);
        assert_eq!(name("#!/usr/bin/env python3"), Some("Python"));
        assert_eq!(name("#!/bin/bash -e"), Some("Shell"));
        assert_eq!(name("#!/usr/bin/env -S node --flag"), Some("JavaScript"));
        assert_eq!(name("no shebang"), None);
    }

    #[test]
    fn categories() {
        assert_eq!(classify_file("Cargo.lock").0, Category::Generated);
        assert_eq!(classify_file("app.min.js").0, Category::Generated);
        assert_eq!(classify_file("photo.JPG").0, Category::Image);
        assert_eq!(classify_file("model.safetensors").0, Category::Data);
        assert_eq!(classify_file("lib.rs").0, Category::Code);
        assert_eq!(classify_file("README.md").0, Category::Docs);
        assert_eq!(classify_file("weird.zzz").0, Category::Other);
    }

    #[test]
    fn tests_detected() {
        assert!(is_test_path("src/foo_test.go", "foo_test.go"));
        assert!(is_test_path(
            "base/values_unittest.cc",
            "values_unittest.cc"
        ));
        assert!(is_test_path("tests/cli.rs", "cli.rs"));
        assert!(is_test_path("web/a.spec.ts", "a.spec.ts"));
        assert!(!is_test_path("src/attest.rs", "attest.rs"));
        assert!(!is_test_path("src/contest/main.rs", "main.rs"));
    }

    #[test]
    fn zones_need_context() {
        let z = |n: &str, s: &[&str]| zone_for_dir(n, s).map(|r| r.0);
        assert_eq!(
            z("target", &["Cargo.toml", "src", "target"]),
            Some(Zone::Artifact)
        );
        assert_eq!(z("target", &["src", "target"]), None);
        assert_eq!(z("build", &["BUILD.gn", "build"]), None);
        assert_eq!(z("node_modules", &[]), Some(Zone::Dependency));
        assert_eq!(z("third_party", &[]), Some(Zone::Vendored));
        assert_eq!(
            zone_from_markers("x", &["CACHEDIR.TAG"]).map(|r| r.0),
            Some(Zone::Artifact)
        );
        assert_eq!(zone_from_markers(".cargo", &["config.toml"]), None);
        // `src/deps/` is a module; `deps/` beside a Makefile is vendored (redis).
        assert_eq!(z("deps", &["deps", "lib.rs", "mod.rs"]), None);
        assert_eq!(
            z("deps", &["Makefile", "deps", "src"]),
            Some(Zone::Vendored)
        );
    }

    #[test]
    fn only_build_dirs_are_settled_by_tracking() {
        let reason = |n: &str, s: &[&str]| zone_for_dir(n, s).map(|r| r.1);
        assert_eq!(reason("build", &["package.json"]), Some(AMBIGUOUS_OUTPUT));
        for (name, manifest) in [
            ("target", "Cargo.toml"),
            ("dist", "package.json"),
            ("out", "tsconfig.json"),
            ("obj", "App.csproj"),
        ] {
            let r = reason(name, &[manifest]);
            assert!(r.is_some() && r != Some(AMBIGUOUS_OUTPUT), "{name}");
        }
    }
}
