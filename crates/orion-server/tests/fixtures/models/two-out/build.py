# Regenerates the two-out fixture beside this file: one graph with two
# outputs of the same dtype and shape — `double = x * 2` and
# `shift = x + 100` over an `x` of [1, 2] — so that two manifests may
# declare the same pair in either order and both be valid for it. The
# weights are constants, so the bytes — and therefore the sha256 digest —
# are reproducible.
#
#   pip install numpy onnx
#   python3 build.py
#
# The tests read `two-out.onnx`, `order-a.json` and `order-b.json` from this
# directory and compute the digest at run time, so a regenerated artifact
# needs no other edit.
import numpy as np, onnx
from onnx import helper, TensorProto, numpy_helper
X = helper.make_tensor_value_info("x", TensorProto.FLOAT, [1, 2])
D = helper.make_tensor_value_info("double", TensorProto.FLOAT, [1, 2])
S = helper.make_tensor_value_info("shift", TensorProto.FLOAT, [1, 2])
two = numpy_helper.from_array(np.array([2.0, 2.0], np.float32), "two")
hund = numpy_helper.from_array(np.array([100.0, 100.0], np.float32), "hund")
nodes = [helper.make_node("Mul", ["x", "two"], ["double"]),
         helper.make_node("Add", ["x", "hund"], ["shift"])]
graph = helper.make_graph(nodes, "two_out", [X], [D, S], initializer=[two, hund])
model = helper.make_model(graph, opset_imports=[helper.make_opsetid("", 17)], producer_name="orion-fixture")
model.ir_version = 9
onnx.checker.check_model(model)
onnx.save(model, "two-out.onnx")
print("bytes", len(model.SerializeToString()))
