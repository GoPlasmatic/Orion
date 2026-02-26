use clap::{Args, CommandFactory};
use clap_complete::{Shell, generate};

#[derive(Args)]
pub struct CompletionsCmd {
    /// Shell to generate completions for
    shell: Shell,
}

impl CompletionsCmd {
    pub fn run(&self) {
        let mut cmd = crate::Cli::command();
        generate(self.shell, &mut cmd, "orion-cli", &mut std::io::stdout());
    }
}
