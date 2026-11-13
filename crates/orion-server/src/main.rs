//! `orion-server`: the Community node. The command line, the operator
//! subcommands and the boot live in the library (`orion::cli`), so an edition
//! binary is the same call with its own name and composition.

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // Serve with Community's composition: the channel drivers, connector
    // providers and package verifier this edition ships.
    orion::cli::main(orion::cli::Binary::orion_server(), |_config| {
        Ok(orion::extensions::CompositionRoot::community())
    })
    .await
}
