"""Makes the shipped ecapa.onnx (~21 MB) from the full one (~83 MB): every Conv weight is stored as int8 (one scale per
output channel) with a DequantizeLinear in front of its Conv, so the maths stays float.

Why not onnxruntime's quantize_dynamic: it turns the Convs into ConvInteger, which the app's onnxruntime.dll cannot run
("Could not find an implementation for ConvInteger"), and all of ECAPA's weights are in Convs, so quantizing only
MatMul/Gemm saves nothing. DequantizeLinear is a standard op every build has.

Checked with eval_int8.py (40 unseen test speakers, 400 chunks): full 0.983, this 0.980, same answer 0.998, fingerprint
similarity mean 0.9992 (min 0.9979).

usage: python compress_ecapa.py <full ecapa.onnx> <out ecapa.onnx>
"""
import os
import sys

import numpy as np
import onnx
from onnx import helper, numpy_helper


def main():
    src, dst = sys.argv[1], sys.argv[2]
    m = onnx.load(src)
    inits = {i.name: i for i in m.graph.initializer}
    dq = []
    for n in m.graph.node:
        if n.op_type != "Conv" or len(n.input) < 2 or n.input[1] not in inits:
            continue
        name = n.input[1]
        w = numpy_helper.to_array(inits[name]).astype(np.float32)
        oc = w.shape[0]
        amax = np.abs(w.reshape(oc, -1)).max(axis=1)
        scale = np.where(amax > 0, amax / 127.0, 1.0).astype(np.float32)
        q = np.clip(np.round(w / scale.reshape((oc,) + (1,) * (w.ndim - 1))), -127, 127).astype(np.int8)
        m.graph.initializer.remove(inits.pop(name))
        m.graph.initializer.extend([
            numpy_helper.from_array(q, name + "_q"),
            numpy_helper.from_array(scale, name + "_s"),
            numpy_helper.from_array(np.zeros(oc, np.int8), name + "_z"),
        ])
        dq.append(helper.make_node("DequantizeLinear", [name + "_q", name + "_s", name + "_z"], [name], axis=0,
                                   name=name + "_dq"))
    # Only initializers feed these, so putting them first keeps the graph in order.
    rest = list(m.graph.node)
    del m.graph.node[:]
    m.graph.node.extend(dq + rest)
    onnx.checker.check_model(m)
    onnx.save(m, dst)
    print(f"{len(dq)} conv weights; {os.path.getsize(src) // 1_000_000} MB -> {os.path.getsize(dst) // 1_000_000} MB")


if __name__ == "__main__":
    main()
