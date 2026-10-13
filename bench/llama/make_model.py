#!/usr/bin/env python3
# Copyright 2026 Vizentro Labs
# SPDX-License-Identifier: Apache-2.0
"""Generate a Llama-architecture GGUF with random weights.

Used when no real model can be downloaded (the benchmark VM has no access to
Hugging Face). The tensor shapes, layout and tokenizer are those of a real
model (TinyLlama-1.1B by default), so llama.cpp runs the identical compute and
memory access pattern per token; only the output text is meaningless.
Quantize the result with llama-quantize, e.g. to Q4_K_M, for realistic sizes.

usage: make_model.py VOCAB_GGUF OUT_GGUF [--layers 22] [--hidden 2048]
       [--ffn 5632] [--heads 32] [--kv-heads 4] [--seed 1]

VOCAB_GGUF is llama.cpp's models/ggml-vocab-llama-spm.gguf (Llama SPM
tokenizer, 32000 tokens), which is in the llama.cpp repository.
"""
import argparse
import sys

import numpy as np
import gguf


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("vocab")
    ap.add_argument("out")
    ap.add_argument("--layers", type=int, default=22)
    ap.add_argument("--hidden", type=int, default=2048)
    ap.add_argument("--ffn", type=int, default=5632)
    ap.add_argument("--heads", type=int, default=32)
    ap.add_argument("--kv-heads", type=int, default=4)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--name", default="random-tinyllama-1.1b")
    a = ap.parse_args()

    r = gguf.GGUFReader(a.vocab)

    def field(name):
        f = r.fields[name]
        return f

    def strings(name):
        f = field(name)
        return [bytes(f.parts[i]).decode("utf-8", "replace") for i in f.data]

    def nums(name):
        f = field(name)
        return [f.parts[i][0] for i in f.data]

    tokens = strings("tokenizer.ggml.tokens")
    scores = [float(x) for x in nums("tokenizer.ggml.scores")]
    types = [int(x) for x in nums("tokenizer.ggml.token_type")]
    vocab = len(tokens)

    w = gguf.GGUFWriter(a.out, "llama")
    w.add_name(a.name)
    w.add_context_length(2048)
    w.add_embedding_length(a.hidden)
    w.add_block_count(a.layers)
    w.add_feed_forward_length(a.ffn)
    w.add_head_count(a.heads)
    w.add_head_count_kv(a.kv_heads)
    w.add_rope_dimension_count(a.hidden // a.heads)
    w.add_layer_norm_rms_eps(1e-5)
    w.add_vocab_size(vocab)
    w.add_file_type(gguf.LlamaFileType.MOSTLY_F16)
    w.add_tokenizer_model("llama")
    w.add_tokenizer_pre("default")
    w.add_token_list(tokens)
    w.add_token_scores(scores)
    w.add_token_types(types)
    w.add_bos_token_id(1)
    w.add_eos_token_id(2)
    w.add_unk_token_id(0)
    w.add_add_bos_token(True)
    w.add_add_eos_token(False)

    rng = np.random.default_rng(a.seed)
    head_dim = a.hidden // a.heads
    kv_dim = head_dim * a.kv_heads

    def mat(name, rows, cols, scale):
        # ggml shapes are [ne0=cols, ne1=rows]; numpy is row-major (rows, cols).
        t = (rng.standard_normal((rows, cols), dtype=np.float32) * scale).astype(np.float16)
        w.add_tensor(name, t)

    def vec(name, n):
        w.add_tensor(name, np.ones(n, dtype=np.float32))

    s = 0.02
    mat("token_embd.weight", vocab, a.hidden, s)
    for i in range(a.layers):
        p = f"blk.{i}."
        vec(p + "attn_norm.weight", a.hidden)
        mat(p + "attn_q.weight", a.hidden, a.hidden, s)
        mat(p + "attn_k.weight", kv_dim, a.hidden, s)
        mat(p + "attn_v.weight", kv_dim, a.hidden, s)
        mat(p + "attn_output.weight", a.hidden, a.hidden, s)
        vec(p + "ffn_norm.weight", a.hidden)
        mat(p + "ffn_gate.weight", a.ffn, a.hidden, s)
        mat(p + "ffn_up.weight", a.ffn, a.hidden, s)
        mat(p + "ffn_down.weight", a.hidden, a.ffn, s)
    vec("output_norm.weight", a.hidden)
    mat("output.weight", vocab, a.hidden, s)

    w.write_header_to_file()
    w.write_kv_data_to_file()
    w.write_tensors_to_file(progress=False)
    w.close()
    print(f"wrote {a.out}: {a.layers} layers, hidden {a.hidden}, ffn {a.ffn}, vocab {vocab}", file=sys.stderr)


if __name__ == "__main__":
    main()
