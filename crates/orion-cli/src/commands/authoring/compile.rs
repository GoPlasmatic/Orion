use super::{boxed, load_and_gate};

/// Compile a definition set into a deployment directory.
///
/// The authoring conveniences a set may use — `$from` for a shared value,
/// `use` for a task fragment — resolve when the set is loaded, and the
/// admin API loads no set. This is the step between the two: it runs
/// every gate `lint <dir>` runs, then writes `deployment.json`,
/// `definitions/<hex>.json` and `connections/<id>.json`, which
/// `orion-cli deployments apply -d` deploys.
#[derive(clap::Args)]
pub struct CompileCmd {
    /// Directory of definitions to compile.
    pub dir: String,
    /// The directory to write.
    #[arg(short, long)]
    pub out: Option<String>,
    /// What to write.
    #[arg(long, value_enum, default_value = "deployment")]
    pub format: CompileFormat,
    /// Exit non-zero on advisory findings too, not just errors.
    #[arg(long)]
    pub deny_warnings: bool,
    /// Directory of plugin manifests (`plugin.toml`) beyond the set's
    /// own tree. A manifest in the set compiles into the deployment with
    /// its component inlined. Repeatable.
    #[arg(long = "plugin-dir", value_name = "DIR")]
    pub plugin_dirs: Vec<String>,
    /// The target node's capabilities, as `orion-cli deployments
    /// capabilities -o json` prints them: validates channels of an edition's
    /// protocols (e.g. `kafka`) against that node's drivers instead of
    /// refusing them as unknown
    #[arg(long, value_name = "FILE")]
    pub capabilities: Option<String>,
}

impl CompileCmd {
    pub fn run(&self) -> anyhow::Result<i32> {
        run_compile(CompileRequest {
            dir: &self.dir,
            output: self.out.as_deref(),
            format: self.format,
            deny_warnings: self.deny_warnings,
            plugin_dirs: &self.plugin_dirs,
            capabilities: self.capabilities.as_deref(),
        })
        .map_err(boxed)?;
        Ok(0)
    }
}

/// What `compile` writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum CompileFormat {
    /// A revision-free v2 deployment directory: `deployment.json`,
    /// `definitions/<hex>.json` and `connections/<id>.json`, the shape
    /// `orion-cli deployments apply -d` deploys.
    Deployment,
}

/// `compile <dir>`: gate a definition set, then write it out in a form a
/// node deploys.
///
/// The authoring conveniences a set may use — `$from`, `use` — resolve when
/// the set is *loaded*, and the admin API loads no set, so this is the step
/// between the two.
///
/// Gate first, emit second, and the gate is `lint <dir>`'s own
/// ([`load_and_gate`]) rather than a second one written beside it: a compile
/// that emitted a set its own linter rejects is how a deployment comes to
/// fail at apply having passed CI.
pub(crate) struct CompileRequest<'a> {
    pub(crate) dir: &'a str,
    pub(crate) output: Option<&'a str>,
    pub(crate) format: CompileFormat,
    pub(crate) deny_warnings: bool,
    /// Directories of plugin manifests beyond the set's own tree.
    pub(crate) plugin_dirs: &'a [String],
    /// `--capabilities`: the target node's drivers, for an edition's channels.
    pub(crate) capabilities: Option<&'a str>,
}

pub(crate) fn run_compile(req: CompileRequest<'_>) -> Result<(), Box<dyn std::error::Error>> {
    // A deployment's members are addressed by id: a member is its kind and
    // id, and the target matches a recompiled one to what it deploys by them.
    // A deployment is the whole of what the target serves, so nothing it
    // references may live outside it: the boundary is empty.
    let (set, _) = load_and_gate(
        req.dir,
        orion_authoring::definitions::Boundary {
            channel_drivers: super::capabilities_drivers(req.capabilities)?,
            ..Default::default()
        },
        true,
        req.deny_warnings,
        req.plugin_dirs,
    )?;
    match req.format {
        CompileFormat::Deployment => {
            emit_deployment(&set, require_output(req.output, "--format deployment")?)
        }
    }
}

/// Write the set as a revision-free deployment directory
/// ([`orion_authoring::definitions::deployment`]).
///
/// The files a previous compile left in `definitions/` and `connections/`
/// that this one does not write are removed: they are named by digest and
/// id, so a stale one is a definition the set no longer has, and a deployer
/// that read the directory would upload it for nothing.
fn emit_deployment(
    set: &orion_authoring::definitions::DefinitionSet,
    out: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let deployment = match orion_authoring::definitions::deployment::build(set) {
        Ok(deployment) => deployment,
        Err(errors) => {
            for error in &errors {
                eprintln!("error: {error}");
            }
            return Err(format!(
                "{} error(s) compiling the deployment; nothing was written",
                errors.len()
            )
            .into());
        }
    };
    for note in &deployment.notes {
        eprintln!("{note}");
    }
    let root = std::path::Path::new(out);
    for sub in ["definitions", "connections"] {
        let dir = root.join(sub);
        std::fs::create_dir_all(&dir).map_err(|e| format!("create '{}': {e}", dir.display()))?;
        for entry in
            std::fs::read_dir(&dir).map_err(|e| format!("read '{}': {e}", dir.display()))?
        {
            let path = entry?.path();
            let relative = format!(
                "{sub}/{}",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            if path.extension().is_some_and(|ext| ext == "json")
                && !deployment.files.contains_key(&relative)
            {
                std::fs::remove_file(&path)
                    .map_err(|e| format!("remove stale '{}': {e}", path.display()))?;
            }
        }
    }
    for (relative, bytes) in &deployment.files {
        let path = root.join(relative);
        std::fs::write(&path, bytes).map_err(|e| format!("write '{}': {e}", path.display()))?;
    }
    let counts = deployment.counts;
    println!(
        "wrote a deployment of {} workflow(s), {} channel(s), {} plugin(s) and {} \
         connection(s) to {out}",
        counts.workflows, counts.channels, counts.plugins, counts.connections
    );
    Ok(())
}

/// A deployment is several files, so there is nowhere for a stdout default
/// to put them.
fn require_output<'a>(
    output: Option<&'a str>,
    what: &str,
) -> Result<&'a str, Box<dyn std::error::Error>> {
    output.ok_or_else(|| format!("-o <DIR> is required for {what}").into())
}
