# Regenerates the c4-tiny fixture beside this file: a two-layer Connect-4
# policy network over a [1, 2, 6, 7] board producing a [1, 7] policy, with
# fixed random weights (seed 7) so the bytes — and therefore the sha256
# digest the model tests claim for them — are reproducible.
#
#   pip install numpy onnx
#   python3 build.py
#
# The tests read `c4-tiny.onnx` and `model.json` from this directory and
# compute the digest at run time, so a regenerated artifact needs no other
# edit.
import numpy as np, onnx
from onnx import helper, TensorProto, numpy_helper
rng = np.random.default_rng(7)
W1 = (rng.standard_normal((84, 16)) * 0.1).astype(np.float32); b1 = np.zeros(16, np.float32)
W2 = (rng.standard_normal((16, 7)) * 0.1).astype(np.float32);  b2 = np.zeros(7, np.float32)
X = helper.make_tensor_value_info("board", TensorProto.FLOAT, [1, 2, 6, 7])
Y = helper.make_tensor_value_info("policy", TensorProto.FLOAT, [1, 7])
nodes = [helper.make_node("Flatten", ["board"], ["flat"], axis=1),
         helper.make_node("Gemm", ["flat", "W1", "b1"], ["h"]),
         helper.make_node("Relu", ["h"], ["hr"]),
         helper.make_node("Gemm", ["hr", "W2", "b2"], ["policy"])]
inits = [numpy_helper.from_array(W1, "W1"), numpy_helper.from_array(b1, "b1"),
         numpy_helper.from_array(W2, "W2"), numpy_helper.from_array(b2, "b2")]
graph = helper.make_graph(nodes, "c4_tiny", [X], [Y], initializer=inits)
model = helper.make_model(graph, opset_imports=[helper.make_opsetid("", 17)], producer_name="orion-fixture")
model.ir_version = 9
onnx.checker.check_model(model)
onnx.save(model, "c4-tiny.onnx")
print("params", sum(int(np.prod(t.dims)) for t in inits), "bytes", len(model.SerializeToString()))
