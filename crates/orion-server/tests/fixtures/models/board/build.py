# Regenerates the board fixture beside this file: a small fully
# convolutional policy network over a board whose height and width are
# decided by the call, which is the shape the per-shape plans in
# `model/runtimes/tract.rs` exist for.
#
# `board.onnx` takes an `x` of i8 [1, 7, H, W], casts it to f32, and runs
# two 3x3 convolutions (dilation 1, then 2, padded to keep H and W) with a
# ReLU after each, then a 1x1 convolution to a [1, 5, H, W] `y`. The 3x3
# layers carry almost all of the work, so tract gets a plan per concrete
# board size; `one-by-one.onnx` is the same boundary with 1x1 layers only,
# which does not. The weights are seeded, so a rebuild writes the same
# bytes.
#
#   pip install numpy onnx
#   python3 build.py
#
# The tests read the files from this directory and compute the digests at
# run time, so a regenerated artifact needs no other edit.
import numpy as np, onnx
from onnx import helper, TensorProto, numpy_helper

def build(path, layers):
    rng = np.random.default_rng(7)
    nodes = [helper.make_node("Cast", ["x"], ["h0"], to=TensorProto.FLOAT)]
    inits, current = [], "h0"
    for i, (cin, cout, k, dilation) in enumerate(layers):
        w = (rng.standard_normal((cout, cin, k, k)) * 0.1).astype(np.float32)
        b = (rng.standard_normal(cout) * 0.1).astype(np.float32)
        inits += [numpy_helper.from_array(w, f"w{i}"), numpy_helper.from_array(b, f"b{i}")]
        pad = dilation * (k - 1) // 2
        out = "y" if i == len(layers) - 1 else f"c{i}"
        nodes.append(helper.make_node("Conv", [current, f"w{i}", f"b{i}"], [out],
                                      kernel_shape=[k, k], dilations=[dilation, dilation],
                                      pads=[pad] * 4))
        if out != "y":
            nodes.append(helper.make_node("Relu", [out], [f"r{i}"]))
            current = f"r{i}"
    X = helper.make_tensor_value_info("x", TensorProto.INT8, [1, 7, "H", "W"])
    Y = helper.make_tensor_value_info("y", TensorProto.FLOAT, [1, 5, "H", "W"])
    graph = helper.make_graph(nodes, "board", [X], [Y], initializer=inits)
    model = helper.make_model(graph, opset_imports=[helper.make_opsetid("", 17)],
                              producer_name="orion-fixture")
    model.ir_version = 9
    onnx.checker.check_model(model)
    onnx.save(model, path)
    print(path, "bytes", len(model.SerializeToString()))

build("board.onnx", [(7, 8, 3, 1), (8, 8, 3, 2), (8, 5, 1, 1)])
build("one-by-one.onnx", [(7, 8, 1, 1), (8, 8, 1, 1), (8, 5, 1, 1)])
