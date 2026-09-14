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
//! the schema's (`onnx/onnx.proto3`), pinned by the fixture tests.
//!
//! A count of `GraphProto.initializer` alone made the number a property of
//! the exporter rather than of the network. The same weights rewritten as
//! `Constant` node attributes compute the same function and counted zero,
//! and `models.max_parameters` — the only graph-shape ceiling admission has
//! — was bounded by that same zero. So the reader finds every value the
//! document carries, wherever it carries it: the initializers, dense and
//! sparse; what a node holds in its attributes; the bodies of `If`, `Loop`
//! and `Scan`; and a model-local function's body, which no walk over graphs
//! would ever reach.
//!
//! **A field counts when the number of values it can hold is unbounded.**
//! That is the whole rule, and it is decidable from the wire format, which
//! is what makes it safe: an operator this reader has never heard of cannot
//! carry anything past it. A singular scalar (`axis = 1`, `alpha = 1.0`) is
//! bounded by the node count, which `nodes` already reports, so nothing can
//! hide in one and this subset does not decode them at all. A repeated
//! scalar list is unbounded and counts — that is a `Constant`'s
//! `value_floats`, and it is also every `ai.onnx.ml` model, where a
//! `LinearRegressor`'s coefficients and a `TreeEnsembleClassifier`'s whole
//! forest travel in attributes and the graph has no initializer at all. The
//! price is that a `Conv`'s `kernel_shape` and `pads` count too, which is a
//! handful of values per node and bounded by rank; the alternative was a
//! table of operators to keep up to date, and an operator missing from it
//! would be free.
//!
//! What this reader does not do: decode a tensor's payload (it counts
//! dimensions, never bytes), follow external data — a tensor stored outside
//! the file still declares its shape, so it counts the same — or read a
//! string attribute, which is `bytes` on the wire and no standard operator
//! turns into a number. Its one payload is a repeated scalar list, and an
//! `int64` list of small values costs eight bytes in memory per byte on the
//! wire; `models.max_artifact_bytes` is what bounds that, as it already
//! bounded the same amplification in a tensor's `dims`.
//!
//! Outside the count because they are outside what runs: a model's
//! `training_info`, which no forward pass reads. A function body is counted
//! once, as the document defines it, not once per call site.

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
    #[prost(message, repeated, tag = "25")]
    functions: Vec<FunctionProto>,
}

/// `OperatorSetIdProto`: one imported operator set.
#[derive(Clone, PartialEq, Message)]
struct OperatorSetIdProto {
    #[prost(string, tag = "1")]
    domain: String,
    #[prost(int64, tag = "2")]
    version: i64,
}

/// `GraphProto`: the nodes, the initializers — dense and sparse — and the
/// boundary. A subgraph is a `GraphProto` too, arriving through a node's
/// attribute, which is why the walk over this is a worklist and not a fold
/// over one graph.
#[derive(Clone, PartialEq, Message)]
struct GraphProto {
    #[prost(message, repeated, tag = "1")]
    node: Vec<NodeProto>,
    #[prost(message, repeated, tag = "5")]
    initializer: Vec<TensorProto>,
    #[prost(message, repeated, tag = "15")]
    sparse_initializer: Vec<SparseTensorProto>,
    #[prost(message, repeated, tag = "11")]
    input: Vec<ValueInfoProto>,
    #[prost(message, repeated, tag = "12")]
    output: Vec<ValueInfoProto>,
}

/// `FunctionProto`: a model-local operator's body. Its nodes belong to no
/// graph, so a walk over graphs alone would never see what they carry.
#[derive(Clone, PartialEq, Message)]
struct FunctionProto {
    #[prost(message, repeated, tag = "7")]
    node: Vec<NodeProto>,
}

/// `NodeProto`: a node is counted, and its attributes are where a value
/// that is not an initializer hides. The operator's own name is not
/// decoded: what an attribute carries counts the same whichever operator
/// declared it, so the reader needs no table of operators and cannot fall
/// behind one.
#[derive(Clone, PartialEq, Message)]
struct NodeProto {
    #[prost(message, repeated, tag = "5")]
    attribute: Vec<AttributeProto>,
}

/// `AttributeProto`: exactly the fields that can hold an unbounded number
/// of values. `f`, `i` and `s` are absent because one value per attribute
/// is already bounded by the node count; `s` and `strings` are `bytes`
/// besides, so declaring them would either copy every byte or — spelt as
/// `String` — refuse a `LabelEncoder` whose keys are not UTF-8 as though it
/// were not an ONNX model at all.
///
/// `g` is boxed. The `Vec`s already make the type finite, so this is about
/// size: an attribute that carries no subgraph, which is nearly all of
/// them, should not pay for the one that might.
#[derive(Clone, PartialEq, Message)]
struct AttributeProto {
    #[prost(message, optional, tag = "5")]
    t: Option<TensorProto>,
    #[prost(message, optional, boxed, tag = "6")]
    g: Option<Box<GraphProto>>,
    #[prost(float, repeated, tag = "7")]
    floats: Vec<f32>,
    #[prost(int64, repeated, tag = "8")]
    ints: Vec<i64>,
    #[prost(message, repeated, tag = "10")]
    tensors: Vec<TensorProto>,
    #[prost(message, repeated, tag = "11")]
    graphs: Vec<GraphProto>,
    #[prost(message, optional, tag = "22")]
    sparse_tensor: Option<SparseTensorProto>,
    #[prost(message, repeated, tag = "23")]
    sparse_tensors: Vec<SparseTensorProto>,
}

/// `TensorProto`: a tensor's shape and name — never its data.
#[derive(Clone, PartialEq, Message)]
struct TensorProto {
    #[prost(int64, repeated, tag = "1")]
    dims: Vec<i64>,
    #[prost(string, tag = "8")]
    name: String,
}

/// `SparseTensorProto`: the tensor of values it stores. `indices` is
/// addressing rather than data, and its own `dims` are the dense shape it
/// stands for rather than what it holds.
#[derive(Clone, PartialEq, Message)]
struct SparseTensorProto {
    #[prost(message, optional, tag = "1")]
    values: Option<TensorProto>,
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
    /// Every value the document carries: the sum, over each initializer and
    /// each tensor or repeated scalar list a node holds in an attribute, of
    /// what it holds — the product of a tensor's dimensions, a scalar
    /// counting one, a sparse tensor counting the values it stores, a list
    /// counting its length — through every subgraph body and every
    /// model-local function. Which carriers count, and why the rule is the
    /// carrier rather than the operator, is in the module docs.
    pub parameters: u64,
    /// Nodes in the graph, in every subgraph body it carries (`If`, `Loop`,
    /// `Scan`) and in every model-local function the document defines.
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

/// The values a tensor carries: the product of its dimensions, one for a
/// scalar, which has none. A dimension that is not a count leaves the
/// tensor at nothing, which is also what a runtime makes of it — such a
/// graph never reaches the probe.
fn tensor_values(tensor: &TensorProto) -> u64 {
    tensor.dims.iter().fold(1u64, |n, d| {
        n.saturating_mul(u64::try_from(*d).unwrap_or(0))
    })
}

/// The values a sparse tensor carries: the ones it stores.
fn sparse_values(sparse: &SparseTensorProto) -> u64 {
    sparse.values.as_ref().map_or(0, tensor_values)
}

/// The values a graph stores itself, dense and sparse. What its nodes carry
/// is the walk's business, because a node can carry a graph.
fn stored_values(graph: &GraphProto) -> u64 {
    let dense = graph.initializer.iter().fold(0u64, |sum, tensor| {
        sum.saturating_add(tensor_values(tensor))
    });
    graph.sparse_initializer.iter().fold(dense, |sum, sparse| {
        sum.saturating_add(sparse_values(sparse))
    })
}

/// The values one attribute carries, leaving aside the subgraphs it may
/// hold — those go on the worklist rather than recursing from here.
fn carried_values(attribute: &AttributeProto) -> u64 {
    let mut values = attribute.t.as_ref().map_or(0, tensor_values);
    for tensor in &attribute.tensors {
        values = values.saturating_add(tensor_values(tensor));
    }
    for sparse in attribute
        .sparse_tensor
        .iter()
        .chain(&attribute.sparse_tensors)
    {
        values = values.saturating_add(sparse_values(sparse));
    }
    values
        .saturating_add(attribute.floats.len() as u64)
        .saturating_add(attribute.ints.len() as u64)
}

/// Decode `bytes` as a `ModelProto` and read its structure. A document
/// that does not decode, or decodes to something without a graph or an IR
/// version, is not an ONNX model, and the error says which.
pub fn read_stats(bytes: &[u8]) -> Result<GraphStats, String> {
    let model = ModelProto::decode(bytes).map_err(|e| format!("not an ONNX model: {e}"))?;
    let Some(graph) = model.graph.as_ref() else {
        return Err("not an ONNX model: the document carries no graph".to_string());
    };
    if model.ir_version <= 0 {
        return Err("not an ONNX model: the document declares no IR version".to_string());
    }

    // Every node list the document defines: the graph's, every subgraph an
    // attribute holds, and every model-local function body. A worklist
    // rather than a recursion, so how deep a hostile document can drive
    // this walk is decided here and not by a dependency's decode limit —
    // though dropping the decoded tree recurses whatever this does, and
    // that limit is what keeps both honest.
    let mut parameters = stored_values(graph);
    let mut nodes = 0u64;
    let mut pending: Vec<&[NodeProto]> = vec![&graph.node];
    pending.extend(model.functions.iter().map(|f| f.node.as_slice()));
    while let Some(body) = pending.pop() {
        nodes = nodes.saturating_add(body.len() as u64);
        for node in body {
            for attribute in &node.attribute {
                parameters = parameters.saturating_add(carried_values(attribute));
                for sub in attribute.g.as_deref().into_iter().chain(&attribute.graphs) {
                    parameters = parameters.saturating_add(stored_values(sub));
                    pending.push(&sub.node);
                }
            }
        }
    }

    let opset = model
        .opset_import
        .iter()
        .find(|o| o.domain.is_empty() || o.domain == "ai.onnx")
        .map_or(0, |o| o.version);
    // The boundary is the top-level graph's alone: a subgraph's inputs are
    // bound by the node that carries it, never by a caller. An initializer
    // listed among the inputs is the pre-IR-4 spelling of a parameter.
    let initialized = |name: &str| {
        graph.initializer.iter().any(|t| t.name == name)
            || graph
                .sparse_initializer
                .iter()
                .any(|s| s.values.as_ref().is_some_and(|t| t.name == name))
    };
    let input_names = graph
        .input
        .iter()
        .filter(|input| !initialized(&input.name))
        .map(|input| input.name.clone())
        .collect();
    let output_names = graph.output.iter().map(|o| o.name.clone()).collect();

    Ok(GraphStats {
        parameters,
        nodes,
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
    ///
    /// It is also the guard on the singular scalars. The graph's one
    /// attribute is `Flatten`'s `axis`, an `INT` this subset does not
    /// decode, and 1479 is the number that says so — count a lone scalar
    /// and it becomes 1480 here and in twenty other places.
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

    /// One network, written three ways by a real ONNX writer: the weights
    /// as initializers, as `Constant` tensors, and as the `value_floats` /
    /// `value_ints` lists opset 12 added. All three answer identically, so
    /// the count has to see them all — while it read only the
    /// initializers, the second and third reported zero and
    /// `models.max_parameters` was something a re-export walked past.
    #[test]
    fn the_same_network_counts_the_same_however_its_weights_are_carried() {
        let as_init = read_stats(fixture::AS_INIT_ONNX).expect("an ONNX model");
        let as_const = read_stats(fixture::AS_CONST_ONNX).expect("an ONNX model");
        let as_list = read_stats(fixture::AS_LIST_ONNX).expect("an ONNX model");
        assert_eq!(as_init.parameters, 15);
        assert_eq!(as_const.parameters, 15);
        // Seventeen, not fifteen, and honestly so: the list form has to
        // carry the two-element shape its reshape reads, and that is data
        // the document holds like any other. This is the rule's whole
        // price, and it is bounded by rank.
        assert_eq!(as_list.parameters, 17);
        // What the rewrites actually cost is visible where it belongs.
        assert_eq!((as_init.nodes, as_const.nodes, as_list.nodes), (1, 3, 5));
        assert_eq!(as_init.input_names, as_const.input_names);
        assert_eq!(as_init.output_names, as_list.output_names);
    }

    /// An initializer listed among the graph inputs (the pre-IR-4 spelling)
    /// is a parameter, not something a caller supplies; a scalar counts
    /// one.
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
                node: vec![NodeProto::default(), NodeProto::default()],
                initializer: vec![
                    TensorProto {
                        dims: vec![3, 4],
                        name: "w".to_string(),
                    },
                    TensorProto {
                        dims: vec![],
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
                ..GraphProto::default()
            }),
            functions: vec![],
        };
        let stats = read_stats(&model.encode_to_vec()).expect("decodes");
        assert_eq!(stats.parameters, 13);
        assert_eq!(stats.nodes, 2);
        assert_eq!(stats.ir_version, 7);
        assert_eq!(stats.opset, 13);
        assert_eq!(stats.input_names, ["x"]);
        assert_eq!(stats.output_names, ["y"]);
    }

    /// Every carrier at once, for the ones a real exporter will not write
    /// but a registrant could: a tensor and a list in an attribute, a
    /// sparse initializer counted by what it stores rather than by the
    /// shape it stands for, branch bodies nested two deep, and a
    /// model-local function whose nodes are in no graph at all.
    #[test]
    fn a_value_counts_wherever_the_document_carries_it() {
        let tensor = |dims: Vec<i64>| TensorProto {
            dims,
            name: String::new(),
        };
        let holding = |attribute: AttributeProto| NodeProto {
            attribute: vec![attribute],
        };
        let inner = GraphProto {
            initializer: vec![tensor(vec![4])],
            ..GraphProto::default()
        };
        let model = ModelProto {
            ir_version: 9,
            opset_import: vec![OperatorSetIdProto {
                domain: String::new(),
                version: 17,
            }],
            graph: Some(GraphProto {
                node: vec![
                    holding(AttributeProto {
                        t: Some(tensor(vec![2, 3])),
                        ..AttributeProto::default()
                    }),
                    holding(AttributeProto {
                        floats: vec![0.0; 6],
                        ..AttributeProto::default()
                    }),
                    // A convolution's shape settings are lists too, and
                    // they count: the rule is the carrier, not the
                    // operator, and two is what that costs.
                    holding(AttributeProto {
                        ints: vec![3, 3],
                        ..AttributeProto::default()
                    }),
                    holding(AttributeProto {
                        g: Some(Box::new(GraphProto {
                            node: vec![holding(AttributeProto {
                                g: Some(Box::new(inner)),
                                ..AttributeProto::default()
                            })],
                            initializer: vec![tensor(vec![5])],
                            ..GraphProto::default()
                        })),
                        ..AttributeProto::default()
                    }),
                ],
                sparse_initializer: vec![SparseTensorProto {
                    values: Some(tensor(vec![2])),
                }],
                ..GraphProto::default()
            }),
            functions: vec![FunctionProto {
                node: vec![holding(AttributeProto {
                    t: Some(tensor(vec![7])),
                    ..AttributeProto::default()
                })],
            }],
        };
        let stats = read_stats(&model.encode_to_vec()).expect("decodes");
        // 6 tensor + 6 list + 2 shape + 2 stored non-zeros + 5 and 4 in
        // the two branch bodies + 7 in the function.
        assert_eq!(stats.parameters, 32);
        // Four here, one in the first branch body, none in the second, one
        // in the function.
        assert_eq!(stats.nodes, 6);
    }

    #[test]
    fn what_is_not_a_model_says_so() {
        let err = read_stats(b"\x00\x01not a protobuf at all").expect_err("garbage");
        assert!(err.starts_with("not an ONNX model:"), "{err}");
        let err = read_stats(b"").expect_err("empty");
        assert!(err.contains("no graph"), "{err}");
        let graph_only = ModelProto {
            ir_version: 0,
            graph: Some(GraphProto::default()),
            ..ModelProto::default()
        };
        let err = read_stats(&graph_only.encode_to_vec()).expect_err("no ir_version");
        assert!(err.contains("IR version"), "{err}");
    }
}
