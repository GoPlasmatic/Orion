# Regenerates the dynamic fixture beside this file: a graph with a variable
# axis, which is what an exporter emits for a batch dimension, a sequence
# length or a variable image size.
#
# `scale.onnx` is `y = x * 2` over an `x` of [N, 3] — N declared symbolic in
# the ONNX file itself, so the graph genuinely accepts any N and the fixture
# is not merely a manifest claim. A manifest declares the same axis as the
# name "N", the probe runs it at whatever `probe_dims` says, and a call
# brings whatever it brings.
#
#   pip install numpy onnx
#   python3 build.py
#
# The tests read `scale.onnx` and `model.json` from this directory and
# compute the digest at run time, so a regenerated artifact needs no other
# edit.
import numpy as np, onnx
from onnx import helper, TensorProto, numpy_helper
X = helper.make_tensor_value_info("x", TensorProto.FLOAT, ["N", 3])
Y = helper.make_tensor_value_info("y", TensorProto.FLOAT, ["N", 3])
two = numpy_helper.from_array(np.array([2.0, 2.0, 2.0], np.float32), "two")
graph = helper.make_graph([helper.make_node("Mul", ["x", "two"], ["y"])],
                          "scale", [X], [Y], initializer=[two])
model = helper.make_model(graph, opset_imports=[helper.make_opsetid("", 17)], producer_name="orion-fixture")
model.ir_version = 9
onnx.checker.check_model(model)
onnx.save(model, "scale.onnx")
print("bytes", len(model.SerializeToString()))
