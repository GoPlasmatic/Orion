use super::boxed;

/// Format definition files to the house style (like `cargo fmt`).
///
/// Every `.json` under each PATH is rewritten in place — entities, shared
/// documents, `*.case.json` files and fixtures alike. There is one style
/// and nothing to configure: known keys of known shapes in canonical
/// order, unary JSONLogic nodes always on one line, leaf nodes on one
/// line when they fit in 100 columns, everything deeper broken one
/// argument per line. Values, number spellings and the order of unknown
/// keys are never changed, and the output is re-parsed and compared with
/// the input before anything is written.
#[derive(clap::Args)]
pub struct FmtCmd {
    /// Files or directories. Default: the current directory.
    #[arg(default_value = ".")]
    pub paths: Vec<String>,
    /// Write nothing; print a diff for each file that is not formatted
    /// and exit 1 if there is one.
    #[arg(long)]
    pub check: bool,
    /// Format one document from stdin to stdout. PATH is ignored.
    #[arg(long, conflicts_with = "check")]
    pub stdin: bool,
}

impl FmtCmd {
    pub fn run(&self) -> anyhow::Result<i32> {
        run_fmt(&self.paths, self.check, self.stdin).map_err(boxed)
    }
}

/// `fmt [PATH]... [--check] [--stdin]`: rewrite definition files to the house
/// style, the way `cargo fmt` rewrites Rust.
///
/// Returns the process exit code rather than an error, because the three
/// outcomes are not one failure: `0` clean or written, `1` `--check` found a
/// file it would rewrite, `2` a file could not be read, parsed or written.
/// Every file is attempted — one unparseable fixture must not leave the rest
/// of the tree unformatted — and the code reflects the worst outcome.
///
/// Diffs and errors go to stderr, the one-line summary to stdout, the same
/// split `lint` makes so a script can grep the verdict.
pub(crate) fn run_fmt(
    paths: &[String],
    check: bool,
    stdin: bool,
) -> Result<i32, Box<dyn std::error::Error>> {
    use orion_authoring::definitions::fmt::{FmtError, Outcome, format_str};

    if stdin {
        return run_fmt_stdin();
    }

    let mut files: Vec<std::path::PathBuf> = Vec::new();
    let mut errors = 0usize;
    for path in paths {
        let path = std::path::Path::new(path);
        if path.is_dir() {
            match orion_authoring::definitions::json_files(path) {
                Ok(found) => files.extend(found),
                Err(e) => {
                    eprintln!("error: {e}");
                    errors += 1;
                }
            }
        } else if path.is_file() {
            // Named explicitly, so taken as given whatever its extension.
            files.push(path.to_path_buf());
        } else {
            eprintln!("error: '{}' is not a file or directory", path.display());
            errors += 1;
        }
    }

    // A set declaring a range this binary is outside of is not formatted at
    // all: the style tables differ between versions, so formatting with the
    // wrong binary is itself the failure. Only a directory's files are
    // consulted — `fmt examples/` spans many sets, each checked on its own.
    for path in paths {
        let path = std::path::Path::new(path);
        if !path.is_dir() {
            continue;
        }
        for file in orion_authoring::definitions::json_files(path).unwrap_or_default() {
            let Some(doc) = std::fs::read_to_string(&file)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .filter(orion_authoring::definitions::SharedDefinitions::is_package_declaration)
            else {
                continue;
            };
            let decl = orion_authoring::definitions::PackageDecl {
                origin: file.display().to_string(),
                name: None,
                requires_orion: doc["package"]["requires"]["orion"]
                    .as_str()
                    .map(str::to_string),
            };
            if let Err(e) = decl.check_this_binary() {
                eprintln!("error: {e} — nothing was formatted");
                return Ok(2);
            }
        }
    }

    let mut changed = 0usize;
    let mut unchanged = 0usize;
    for file in &files {
        let shown = file.display();
        let text = match std::fs::read(file) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(e) => {
                    eprintln!(
                        "error: {shown}: not valid UTF-8 at byte {}",
                        e.utf8_error().valid_up_to()
                    );
                    errors += 1;
                    continue;
                }
            },
            Err(e) => {
                eprintln!("error: {shown}: {e}");
                errors += 1;
                continue;
            }
        };
        match format_str(&text, &shown.to_string()) {
            Ok(Outcome::Unchanged) => unchanged += 1,
            Ok(Outcome::Changed(formatted)) => {
                changed += 1;
                if check {
                    eprint!("{}", unified_diff(&shown.to_string(), &text, &formatted));
                } else if let Err(e) = write_atomically(file, &formatted) {
                    eprintln!("error: {shown}: {e}");
                    errors += 1;
                }
            }
            Err(FmtError::Parse(e)) => {
                eprintln!("error: {shown}: {e}");
                errors += 1;
            }
            Err(e) => {
                eprintln!("error: {e}");
                errors += 1;
            }
        }
    }

    let verb = if check {
        "would be reformatted"
    } else {
        "reformatted"
    };
    println!(
        "{changed} file(s) {verb}, {unchanged} unchanged{}",
        if errors > 0 {
            format!(", {errors} error(s)")
        } else {
            String::new()
        }
    );
    Ok(if errors > 0 {
        2
    } else if check && changed > 0 {
        1
    } else {
        0
    })
}

/// `fmt --stdin`: one document in, its formatted form out. Nothing reaches
/// stdout on failure, so an editor that replaces the buffer with the output
/// never replaces it with an error message.
fn run_fmt_stdin() -> Result<i32, Box<dyn std::error::Error>> {
    use orion_authoring::definitions::fmt::{Outcome, format_str};
    use std::io::{Read, Write};

    let mut text = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut text) {
        eprintln!("error: <stdin>: {e}");
        return Ok(2);
    }
    match format_str(&text, "<stdin>") {
        Ok(Outcome::Unchanged) => {
            std::io::stdout().write_all(text.as_bytes())?;
            Ok(0)
        }
        Ok(Outcome::Changed(formatted)) => {
            std::io::stdout().write_all(formatted.as_bytes())?;
            Ok(0)
        }
        Err(e) => {
            eprintln!("error: <stdin>: {e}");
            Ok(2)
        }
    }
}

/// A unified diff in the shape `git diff` prints, so the `--check` output
/// reads in a terminal and applies with `patch -p1`.
pub(super) fn unified_diff(path: &str, before: &str, after: &str) -> String {
    // An absolute path would print as `a//Users/…`; `git diff` headers are
    // relative, and so are these.
    let path = path.trim_start_matches('/');
    similar::TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

/// Replace `path`'s contents without a window in which it is half-written:
/// write a sibling temp file, copy the original's permissions onto it, and
/// rename it over the original. On any failure the temp file is removed and
/// the original is untouched.
///
/// A symlink is resolved first, because renaming over the link's own path
/// would replace the link with a regular file rather than update its target.
pub(super) fn write_atomically(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    let target = path.canonicalize()?;
    let dir = target
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = dir.join(format!(".{name}.fmt-tmp-{}", std::process::id()));

    let attempt = (|| {
        std::fs::write(&tmp, content)?;
        let permissions = std::fs::metadata(&target)?.permissions();
        std::fs::set_permissions(&tmp, permissions)?;
        std::fs::rename(&tmp, &target)
    })();
    if attempt.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    attempt
}
