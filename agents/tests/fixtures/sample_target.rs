// Tiny native binary used as a fixture for the ghidra tool tests.
//
// Built once per `cargo test` run via the `build.rs`-equivalent in the
// test module. The resulting `.exe` (or ELF binary on Unix) is a real
// native executable with real symbols (`main`, `target_function`,
// `indirect_target`) — exactly what the ghidra tool would point at
// in real use. The agent doesn't care that the source is Rust; it
// only cares that there's a binary to analyze.

#![allow(dead_code)]

/// Entry point referenced by every binary analyzer as the first
/// function to look at.
fn main() {
    let _ = indirect_target(7);
}

/// A function the ghidra tool can be asked to decompile. Uses
/// `#[no_mangle]` + `extern "C"` so the symbol survives linkage
/// with the same name on every platform.
#[no_mangle]
pub extern "C" fn target_function(n: i32) -> i32 {
    let mut sum = 0;
    for i in 0..n {
        sum += i;
    }
    sum
}

/// A second function — exercises "multiple functions" actions and
/// lets us verify cross-references between symbols.
#[no_mangle]
pub extern "C" fn indirect_target(n: i32) -> i32 {
    target_function(n) + 1
}