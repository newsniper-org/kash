//! Backend-comparison benchmark.
//!
//! Drives `kash_core::eval::Evaluator<B>` against three backends —
//! `BTreeBackend`, `LightnhtBackend<QLMHasher>`, and
//! `LightnhtBackend<SwarQLMHasher>` — over three workloads that
//! exercise the engine's three biggest tables independently:
//!
//! - **scope variables** (`Frame::bindings`): assign N keys, then
//!   look up each K times via `$VAR`-style expansion;
//! - **functions** (`Evaluator::functions`): define N zero-body
//!   functions, then call each K times;
//! - **aliases** (`Evaluator::aliases`): register N aliases, then
//!   dispatch each K times.
//!
//! The test is `#[ignore]` by default — `cargo test` is dev-mode by
//! default, which would produce misleading numbers. Run with
//! `cargo test --release -p kash-core --test bench_backends -- --ignored
//! --nocapture` to see the table.
//!
//! Output goes through `eprintln!` so it lands on stderr regardless
//! of the test harness's stdout capture.

#![cfg(feature = "std")]

use std::time::{Duration, Instant};

use kash_core::ast::Program;
use kash_core::eval::Evaluator;
use kash_core::parser::parse;
use kash_gadt::{BTreeBackend, MapBackend};
use lightnht::{LightnhtBackend, QLMHasher, SwarQLMHasher};

const N_KEYS: usize = 100;
const REPETITIONS: usize = 20;
const ITERS: usize = 50;

fn build_var_lookup_script() -> String {
    // 100 assigns + 20 passes × 100 `: $VAR` lookups = 2000 lookups
    // worth of scope-table descents per script invocation.
    let mut s = String::new();
    for i in 0..N_KEYS {
        s.push_str(&format!("X{i}={i}\n"));
    }
    for _ in 0..REPETITIONS {
        for i in 0..N_KEYS {
            s.push_str(&format!(": $X{i}\n"));
        }
    }
    s
}

fn build_function_call_script() -> String {
    let mut s = String::new();
    for i in 0..N_KEYS {
        s.push_str(&format!("f{i}() {{ :; }}\n"));
    }
    for _ in 0..REPETITIONS {
        for i in 0..N_KEYS {
            s.push_str(&format!("f{i}\n"));
        }
    }
    s
}

fn build_alias_dispatch_script() -> String {
    let mut s = String::new();
    for i in 0..N_KEYS {
        s.push_str(&format!("alias a{i}=':'\n"));
    }
    for _ in 0..REPETITIONS {
        for i in 0..N_KEYS {
            s.push_str(&format!("a{i}\n"));
        }
    }
    s
}

fn bench<B>(prog: &Program) -> Duration
where
    B: MapBackend,
{
    let start = Instant::now();
    for _ in 0..ITERS {
        let mut ev: Evaluator<B> = Evaluator::new();
        ev.eval_program(prog).expect("eval succeeds");
        let _ = ev.take_output();
    }
    start.elapsed()
}

fn ns_per_op(dur: Duration, ops_per_iter: usize) -> u64 {
    let total_ops = (ITERS * ops_per_iter) as u128;
    (dur.as_nanos() / total_ops) as u64
}

#[test]
#[ignore = "release-mode timing benchmark; opt in with --ignored"]
fn compare_backends_per_table() {
    let var_src = build_var_lookup_script();
    let fn_src = build_function_call_script();
    let alias_src = build_alias_dispatch_script();

    let var_prog = parse(&var_src).expect("parse var script");
    let fn_prog = parse(&fn_src).expect("parse fn script");
    let alias_prog = parse(&alias_src).expect("parse alias script");

    // ops_per_iter for each workload — what's interesting on the
    // map-backend hot path. For var lookup: every expansion is one
    // `Scope::get`. For function call: every call is one
    // `functions::get` (the body itself is a `:` no-op). For alias
    // dispatch: every call is one `aliases::get` plus the resolved
    // builtin call.
    let var_ops = REPETITIONS * N_KEYS;
    let fn_ops = REPETITIONS * N_KEYS;
    let alias_ops = REPETITIONS * N_KEYS;

    eprintln!();
    eprintln!(
        "backend-comparison bench — N_KEYS={N_KEYS}, REPETITIONS={REPETITIONS}, ITERS={ITERS}"
    );
    eprintln!(
        "                        | var lookup     | function call  | alias dispatch"
    );
    eprintln!(
        "------------------------|----------------|----------------|---------------"
    );

    let btree_var = bench::<BTreeBackend>(&var_prog);
    let btree_fn = bench::<BTreeBackend>(&fn_prog);
    let btree_alias = bench::<BTreeBackend>(&alias_prog);
    eprintln!(
        " BTreeBackend           | {:>8} ns/op | {:>8} ns/op | {:>8} ns/op",
        ns_per_op(btree_var, var_ops),
        ns_per_op(btree_fn, fn_ops),
        ns_per_op(btree_alias, alias_ops),
    );

    let qlm_var = bench::<LightnhtBackend<QLMHasher>>(&var_prog);
    let qlm_fn = bench::<LightnhtBackend<QLMHasher>>(&fn_prog);
    let qlm_alias = bench::<LightnhtBackend<QLMHasher>>(&alias_prog);
    eprintln!(
        " Lightnht<QLM>          | {:>8} ns/op | {:>8} ns/op | {:>8} ns/op",
        ns_per_op(qlm_var, var_ops),
        ns_per_op(qlm_fn, fn_ops),
        ns_per_op(qlm_alias, alias_ops),
    );

    let swar_var = bench::<LightnhtBackend<SwarQLMHasher>>(&var_prog);
    let swar_fn = bench::<LightnhtBackend<SwarQLMHasher>>(&fn_prog);
    let swar_alias = bench::<LightnhtBackend<SwarQLMHasher>>(&alias_prog);
    eprintln!(
        " Lightnht<SwarQLM>      | {:>8} ns/op | {:>8} ns/op | {:>8} ns/op",
        ns_per_op(swar_var, var_ops),
        ns_per_op(swar_fn, fn_ops),
        ns_per_op(swar_alias, alias_ops),
    );
    eprintln!();
}
