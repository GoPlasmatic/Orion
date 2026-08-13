use clap::{Args, CommandFactory};
use clap_complete::{Shell, generate};

#[derive(Args)]
#[command(after_help = crate::help::COMPLETIONS)]
pub struct CompletionsCmd {
    /// Shell to generate completions for (bash, zsh, fish, powershell, elvish)
    shell: Shell,
}

impl CompletionsCmd {
    pub fn run(&self) {
        let mut cmd = crate::Cli::command();
        generate(self.shell, &mut cmd, "orion-cli", &mut std::io::stdout());
    }
}
