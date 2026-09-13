//! A minimal, runtime-independent reader of an ONNX model's structure: what
//! admission records as a model's stats, and the names it checks a manifest
//! against, before any runtime has parsed the graph.
//!
//! The numbers a stat carries — parameters, nodes, IR version, opset — are
//! read from the protobuf directly rather than asked of a runtime, so they
//! are the same whichever runtime the node has and stay the same when the
//! runtime changes: a parameter count is what a model is compared on, and a
//! number that moved with a dependency upgrade would be a number nobody
//! could rely on. The messages below are a hand-written subset of
//! `onnx.proto3` holding exactly the fields this reader needs; protobuf
//! decoding skips every field a message does not declare, so a subset is a
//! valid decoder for any model the full schema describes. Field numbers are
//! the schema's (`onnx/onnx.proto3`), pinned by the fixture test.
//!
//! What this reader does not do: decode initializer payloads (it counts
//! dimensions, never bytes), follow external data, or read attributes. The
//! last is why a `Constant` node's tensor is not a parameter here — it
//! travels in an attribute this subset leaves undecoded — and why the count
//! is defined as *the initializers'* parameters, which is what every
//! exporter puts the trained weights in.

use prost::Message;

/// `ModelProto`, the fields admission reads.
#[derive(Clone, PartialEq, Message)]
struct ModelProto {
    #[prost(int64, tag = "1")]
    ir_version: i64,
    #[prost(message, repeated, tag = "8")]
    opset_import: Vec<OperatorSetIdProto>,
    #[prost(message, optional, tag = "7")]
    graph: Option<GraphProto>,
}

/// `OperatorSetIdProto`: one imported operator set.
#[derive(Clone, PartialEq, Message)]
struct OperatorSetIdProto {
    #[prost(string, tag = "1")]
    domain: String,
    #[prost(int64, tag = "2")]
    version: i64,
}

/// `GraphProto`: the nodes, the initializers and the boundary.
#[derive(Clone, PartialEq, Message)]
struct GraphProto {
    #[prost(message, repeated, tag = "1")]
    node: Vec<NodeProto>,
    #[prost(message, repeated, tag = "5")]
    initializer: Vec<TensorProto>,
    #[prost(message, repeated, tag = "11")]
    input: Vec<ValueInfoProto>,
    #[prost(message, repeated, tag = "12")]
    output: Vec<ValueInfoProto>,
}

/// `NodeProto`: enough to count nodes and tell a `Constant` apart.
#[derive(Clone, PartialEq, Message)]
struct NodeProto {
    #[prost(string, repeated, tag = "2")]
    output: Vec<String>,
    #[prost(string, tag = "4")]
    op_type: String,
}

/// `TensorProto`: an initializer's shape and name — never its data.
#[derive(Clone, PartialEq, Message)]
struct TensorProto {
    #[prost(int64, repeated, tag = "1")]
    dims: Vec<i64>,
    #[prost(int32, tag = "2")]
    data_type: i32,
    #[prost(string, tag = "8")]
    name: String,
}

/// `ValueInfoProto`: a graph input or output, by name.
#[derive(Clone, PartialEq, Message)]
struct ValueInfoProto {
    #[prost(string, tag = "1")]
    name: String,
}

/// What [`read_stats`] learns about a model without running it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphStats {
    /// The sum, over every initializer, of the product of its dimensions
    /// (a scalar counts one). The tensor a `Constant` node carries in its
    /// attribute is not counted — see the module docs.
    pub parameters: u64,
    /// Nodes in the top-level graph. Subgraphs (`If`, `Loop`, `Scan`
    /// bodies) are attributes, so their nodes are not counted.
    pub nodes: u64,
    /// `ModelProto.ir_version`.
    pub ir_version: i64,
    /// The version of the default operator domain (`""`, or its alias
    /// `ai.onnx`), `0` when the model imports none.
    pub opset: i64,
    /// The graph's inputs that are not also initializers — what a caller
    /// must supply — in graph order.
    pub input_names: Vec<String>,
    /// The graph's outputs, in graph order.
    pub output_names: Vec<String>,
}

/// Decode `bytes` as a `ModelProto` and read its structure. A document
/// that does not decode, or decodes to something without a graph or an IR
/// version, is not an ONNX model, and the error says which.
pub fn read_stats(bytes: &[u8]) -> Result<GraphStats, String> {
    let model = ModelProto::decode(bytes).map_err(|e| format!("not an ONNX model: {e}"))?;
    let Some(graph) = model.graph else {
        return Err("not an ONNX model: the document carries no graph".to_string());
    };
    if model.ir_version <= 0 {
        return Err("not an ONNX model: the document declares no IR version".to_string());
    }

    let parameters = graph.initializer.iter().fold(0u64, |sum, tensor| {
        let count = tensor.dims.iter().fold(1u64, |n, d| {
            n.saturating_mul(u64::try_from(*d).unwrap_or(0))
        });
        sum.saturating_add(count)
    });
    let opset = model
        .opset_import
        .iter()
        .find(|o| o.domain.is_empty() || o.domain == "ai.onnx")
        .map_or(0, |o| o.version);
    let input_names = graph
        .input
        .iter()
        .filter(|input| !graph.initializer.iter().any(|t| t.name == input.name))
        .map(|input| input.name.clone())
        .collect();
    let output_names = graph.output.iter().map(|o| o.name.clone()).collect();

    Ok(GraphStats {
        parameters,
        nodes: graph.node.len() as u64,
        ir_version: model.ir_version,
        opset,
        input_names,
        output_names,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::fixture;

    /// The numbers `build.py` prints for the fixture, read back from the
    /// bytes: `W1[84,16] + b1[16] + W2[16,7] + b2[7]`, four nodes, IR 9,
    /// opset 17.
    #[test]
    fn the_fixture_reads_back_as_built() {
        let stats = read_stats(fixture::ONNX).expect("the fixture is an ONNX model");
        assert_eq!(
            stats,
            GraphStats {
                parameters: 1479,
                nodes: 4,
                ir_version: 9,
                opset: 17,
                input_names: vec!["board".to_string()],
                output_names: vec!["policy".to_string()],
            }
        );
    }

    /// An initializer listed among the graph inputs (the pre-IR-4 spelling)
    /// is a parameter, not something a caller supplies; a scalar counts
    /// one; a `Constant` node counts as a node and nothing else.
    #[test]
    fn initializers_are_parameters_and_not_inputs() {
        let model = ModelProto {
            ir_version: 7,
            opset_import: vec![
                OperatorSetIdProto {
                    domain: "com.microsoft".to_string(),
                    version: 1,
                },
                OperatorSetIdProto {
                    domain: String::new(),
                    version: 13,
                },
            ],
            graph: Some(GraphProto {
                node: vec![
                    NodeProto {
                        output: vec!["c".to_string()],
                        op_type: "Constant".to_string(),
                    },
                    NodeProto {
                        output: vec!["y".to_string()],
                        op_type: "Add".to_string(),
                    },
                ],
                initializer: vec![
                    TensorProto {
                        dims: vec![3, 4],
                        data_type: 1,
                        name: "w".to_string(),
                    },
                    TensorProto {
                        dims: vec![],
                        data_type: 1,
                        name: "scale".to_string(),
                    },
                ],
                input: vec![
                    ValueInfoProto {
                        name: "x".to_string(),
                    },
                    ValueInfoProto {
                        name: "w".to_string(),
                    },
                ],
                output: vec![ValueInfoProto {
                    name: "y".to_string(),
                }],
            }),
        };
        let stats = read_stats(&model.encode_to_vec()).expect("decodes");
        assert_eq!(stats.parameters, 13);
        assert_eq!(stats.nodes, 2);
        assert_eq!(stats.ir_version, 7);
        assert_eq!(stats.opset, 13);
        assert_eq!(stats.input_names, ["x"]);
        assert_eq!(stats.output_names, ["y"]);
    }

    #[test]
    fn what_is_not_a_model_says_so() {
        let err = read_stats(b"\x00\x01not a protobuf at all").expect_err("garbage");
        assert!(err.starts_with("not an ONNX model:"), "{err}");
        let err = read_stats(b"").expect_err("empty");
        assert!(err.contains("no graph"), "{err}");
        let graph_only = ModelProto {
            ir_version: 0,
            opset_import: vec![],
            graph: Some(GraphProto::default()),
        };
        let err = read_stats(&graph_only.encode_to_vec()).expect_err("no ir_version");
        assert!(err.contains("IR version"), "{err}");
    }
}
