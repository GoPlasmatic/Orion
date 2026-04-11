use serde_json::Value;

// Standard Orion benchmark fixtures — same files as ../Orion/tests/benchmark/fixtures/
const SIMPLE_WORKFLOW: &str = include_str!("fixtures/bench_simple_log.json");
const COMPLEX_WORKFLOW: &str = include_str!("fixtures/bench_complex_ecommerce.json");
const MULTI_WORKFLOWS: &str = include_str!("fixtures/bench_multi_rules.json");

const SIMPLE_PAYLOAD: &str = include_str!("fixtures/simple_payload.json");
const COMPLEX_PAYLOAD: &str = include_str!("fixtures/complex_payload.json");

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum Scenario {
    /// Single log task — baseline pipeline overhead
    Simple,
    /// 4-task ecommerce workflow — conditional + enrichment
    Complex,
    /// 12 workflows on same channel — fan-out / rule evaluation
    Multi,
    /// Run all scenarios sequentially
    All,
}

pub struct ScenarioConfig {
    pub name: &'static str,
    pub description: &'static str,
    pub workflow_json: &'static str,
    pub is_import: bool,
    pub channel: &'static str,
    pub payload_json: &'static str,
}

const SIMPLE_CONFIG: ScenarioConfig = ScenarioConfig {
    name: "simple",
    description: "Simple workflow (1 log task)",
    workflow_json: SIMPLE_WORKFLOW,
    is_import: false,
    channel: "bench",
    payload_json: SIMPLE_PAYLOAD,
};

const COMPLEX_CONFIG: ScenarioConfig = ScenarioConfig {
    name: "complex",
    description: "Complex workflow (4 tasks)",
    workflow_json: COMPLEX_WORKFLOW,
    is_import: false,
    channel: "orders",
    payload_json: COMPLEX_PAYLOAD,
};

const MULTI_CONFIG: ScenarioConfig = ScenarioConfig {
    name: "multi",
    description: "Multi-workflow channel (12 workflows)",
    workflow_json: MULTI_WORKFLOWS,
    is_import: true,
    channel: "bench",
    payload_json: SIMPLE_PAYLOAD,
};

pub fn get_scenarios(scenario: &Scenario) -> Vec<&'static ScenarioConfig> {
    match scenario {
        Scenario::Simple => vec![&SIMPLE_CONFIG],
        Scenario::Complex => vec![&COMPLEX_CONFIG],
        Scenario::Multi => vec![&MULTI_CONFIG],
        Scenario::All => vec![&SIMPLE_CONFIG, &COMPLEX_CONFIG, &MULTI_CONFIG],
    }
}

pub fn parse_payload(json_str: &str) -> Value {
    serde_json::from_str(json_str).expect("embedded fixture JSON is valid")
}
