# Faster Builds with cargo-slicer

[cargo-slicer](https://github.com/nickel-org/cargo-slicer) is a `RUSTC_WRAPPER` that stubs unreachable library functions at the MIR level, skipping LLVM codegen for code the final binary never calls.

## Expected Savings

Based on benchmarks from similar Rust workspaces:

| Mode | Typical savings |
|------|----------------|
| syn pre-analysis | 9–15% |
| MIR-precise | 20–30% |

Savings scale with codebase size and the ratio of library code to binary code.

## CI Integration

The workflow `.github/workflows/rust-ci-fast.yml` runs an accelerated release build alongside the standard CI. It does not gate merges and uses a resilient two-path strategy:

- **Fast path**: install `cargo-slicer` with MIR-precise support and run the sliced build.
- **Fallback path**: if `rustc-driver` install fails (nightly API drift), run a plain `cargo +nightly build --release`.

## Local Usage

```bash
# One-time install
cargo install cargo-slicer
rustup component add rust-src rustc-dev llvm-tools-preview --toolchain nightly
cargo +nightly install cargo-slicer --profile release-rustc \
  --bin cargo-slicer-rustc --bin cargo_slicer_dispatch \
  --features rustc-driver

# Build with syn pre-analysis (from rust/ directory)
cargo-slicer pre-analyze
CARGO_SLICER_VIRTUAL=1 CARGO_SLICER_CODEGEN_FILTER=1 \
  RUSTC_WRAPPER=$(which cargo_slicer_dispatch) \
  cargo +nightly build --release
```

## How It Works

1. **Pre-analysis** scans workspace sources via `syn` to build a cross-crate call graph.
2. **Cross-crate BFS** from `main()` identifies which public library functions are actually reachable.
3. **MIR stubbing** replaces unreachable bodies with `Unreachable` terminators — the mono collector finds no callees and prunes entire codegen subtrees.

No source files are modified. The output binary is functionally identical.
