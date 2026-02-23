use serde_json::Value;
use tabled::{Table, settings::Style};

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
    Yaml,
}

impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutputFormat::Table => write!(f, "table"),
            OutputFormat::Json => write!(f, "json"),
            OutputFormat::Yaml => write!(f, "yaml"),
        }
    }
}

pub fn print_table<T: tabled::Tabled>(rows: Vec<T>) {
    let table = Table::new(rows).with(Style::rounded()).to_string();
    println!("{table}");
}

pub fn print_value(format: &OutputFormat, value: &Value) -> anyhow::Result<()> {
    match format {
        OutputFormat::Table | OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(value)?);
        }
        OutputFormat::Yaml => {
            print!("{}", serde_yaml::to_string(value)?);
        }
    }
    Ok(())
}
