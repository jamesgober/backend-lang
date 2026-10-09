//! Differential testing on the real processor: random ir-lang programs
//! compiled by backend-lang run natively in this process (x86-64 Linux and
//! Windows), under both SysV and Win64, against ir-lang's interpreter. The
//! test interpreter (`differential.rs`) checks the same programs on every
//! platform; this checks that the interpreter and the hardware agree.

#![cfg(all(target_arch = "x86_64", any(target_os = "linux", windows)))]
#![allow(clippy::unwrap_used, reason = "test failures should be loud")]

mod common;

use backend_lang::Options;
use common::irgen::{self, GenOptions, Rng};
use common::load::Conv;
use common::native::{Native, Ran};
use ir_lang::Type;
use ir_lang::interp::{self, Outcome};

fn mask(ty: Type, v: u64) -> u64 {
    match ty {
        Type::Bool => v & 1,
        t => match t.int_bits() {
            Some(64) | None => v,
            Some(w) => v & ((1u64 << w) - 1),
        },
    }
}

fn sweep(seeds: core::ops::Range<u64>, opts: GenOptions) {
    let mut ran = 0;
    for seed in seeds {
        let prog = irgen::generate(seed, &opts);
        let mut rng = Rng::new(seed ^ 0xA5A5);
        let (vals, regs) = irgen::args(&mut rng, &prog.params);
        let want = match interp::run(&prog.module, prog.entry, &vals).unwrap() {
            Outcome::Return(v) => Ran::Returned(
                v.first().map_or(0, |x| x.bits()),
                v.get(1).map_or(0, |x| x.bits()),
            ),
            Outcome::Trap(c) => Ran::Trap(u64::from(c)),
            other => panic!("{other:?}"),
        };
        let name = prog
            .module
            .func_decl(prog.entry)
            .unwrap()
            .name()
            .to_string();
        for (conv, options) in [
            (Conv::SysV, Options::sysv()),
            (Conv::Win64, Options::win64()),
        ] {
            // Win64 returns one value in registers.
            if conv == Conv::Win64 && prog.returns.len() > 1 {
                continue;
            }
            let options = options.with_frame_pointer(seed % 2 == 1);
            let c = backend_lang::compile(&prog.module, &options).unwrap();
            let n = Native::load(&c, conv).unwrap();
            let got = match n.call(&name, &regs).unwrap() {
                Ran::Returned(a, b) => Ran::Returned(
                    mask(prog.returns[0], a),
                    prog.returns.get(1).map_or(0, |&t| mask(t, b)),
                ),
                t => t,
            };
            assert_eq!(got, want, "seed {seed} {conv:?}\n{}", prog.module);
            ran += 1;
        }
    }
    assert!(ran > 0);
}

fn count(default: u64) -> u64 {
    std::env::var("BACKEND_DIFF_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
fn native_matches_the_interpreter() {
    sweep(10_000..10_000 + count(300), GenOptions::default());
}

#[test]
fn native_two_results_and_high_pressure() {
    sweep(
        11_000..11_000 + count(100),
        GenOptions {
            returns: 2,
            ..GenOptions::default()
        },
    );
    sweep(
        12_000..12_000 + count(60),
        GenOptions {
            statements: 160,
            functions: 4,
            traps: false,
            ..GenOptions::default()
        },
    );
}
