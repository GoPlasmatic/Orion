# Regenerates the weights fixture beside this file: one network — a single
# `Gemm` of `W[4, 3]` and `b[3]` over an `x` of [1, 4], 15 parameters with
# fixed random weights (seed 7) — written twice.
#
#   as-init.onnx    the weights as initializers, what every exporter emits
#   as-const.onnx   the same arrays as `Constant` node attributes
#   as-list.onnx    the same numbers as `value_floats` / `value_ints` lists,
#                   the spelling opset 12 added and the one every
#                   `ai.onnx.ml` model uses for all of its parameters
#
# The three graphs compute the same function and answer identically. They
# exist so the reader's parameter count can be asserted over all of them: a
# count that reads only the initializers reports 15 for the first and 0 for
# the other two, which is what made `models.max_parameters` evadable. The
# third reads 17 rather than 15, because the reshape it needs carries its
# two-element shape as data too.
#
#   pip install numpy onnx
#   python3 build.py
#
# The tests read both artifacts and both manifests from this directory and
# compute the digests at run time, so a regenerated pair needs no other edit.
import numpy as np, onnx
from onnx import helper, TensorProto, numpy_helper
rng = np.random.default_rng(7)
W = (rng.standard_normal((4, 3)) * 0.1).astype(np.float32); b = np.zeros(3, np.float32)
X = helper.make_tensor_value_info("x", TensorProto.FLOAT, [1, 4])
Y = helper.make_tensor_value_info("y", TensorProto.FLOAT, [1, 3])
gemm = helper.make_node("Gemm", ["x", "W", "b"], ["y"])
graphs = {
    "as-init": helper.make_graph([gemm], "as_init", [X], [Y],
        initializer=[numpy_helper.from_array(W, "W"), numpy_helper.from_array(b, "b")]),
    "as-const": helper.make_graph(
        [helper.make_node("Constant", [], ["W"], value=numpy_helper.from_array(W, "W")),
         helper.make_node("Constant", [], ["b"], value=numpy_helper.from_array(b, "b")),
         gemm], "as_const", [X], [Y]),
    "as-list": helper.make_graph(
        [helper.make_node("Constant", [], ["flat"], value_floats=W.reshape(-1).tolist()),
         helper.make_node("Constant", [], ["shape"], value_ints=[4, 3]),
         helper.make_node("Reshape", ["flat", "shape"], ["W"]),
         helper.make_node("Constant", [], ["b"], value_floats=b.tolist()),
         gemm], "as_list", [X], [Y]),
}
for name, graph in graphs.items():
    model = helper.make_model(graph, opset_imports=[helper.make_opsetid("", 17)], producer_name="orion-fixture")
    model.ir_version = 9
    onnx.checker.check_model(model)
    onnx.save(model, f"{name}.onnx")
    print(name, "bytes", len(model.SerializeToString()))
