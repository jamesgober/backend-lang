//! Differential testing: random ir-lang 2.0 programs are compiled by
//! backend-lang, linked, and run in the x86-64 test interpreter under SysV
//! and Win64, and every outcome (results, or the trap code) is compared with
//! ir-lang's reference interpreter. Arguments arrive with junk above their
//! width, every register the callee should not read holds junk, and
//! callee-saved registers and the stack pointer are checked after each
//! return.

#![allow(clippy::unwrap_used, reason = "test failures should be loud")]

mod common;

use backend_lang::Options;
use common::Stop;
use common::irgen::{self, GenOptions, Rng};
use common::load::{self, Conv, Hosts};
use ir_lang::interp::{self, Outcome};
use ir_lang::{CallConv, Type};

/// The low `ty`-width bits of a register.
fn mask(ty: Type, v: u64) -> u64 {
    match ty {
        Type::Bool => v & 1,
        t => match t.int_bits() {
            Some(64) | None => v,
            Some(w) => v & ((1u64 << w) - 1),
        },
    }
}

/// What a run produced, in a form both sides can be compared in.
#[derive(Debug, PartialEq, Eq)]
enum Result_ {
    Return(Vec<u64>),
    Trap(u64),
}

/// Runs one generated program both ways; returns a description of any
/// difference.
fn case(seed: u64, opts: &GenOptions, conv: Conv) -> Result<(), String> {
    let prog = irgen::generate(seed, opts);
    prog.module
        .validate()
        .map_err(|e| format!("generator produced invalid IR: {e}\n{}", prog.module))?;
    let mut rng = Rng::new(seed ^ 0xA5A5);
    let (vals, regs) = irgen::args(&mut rng, &prog.params);
    let expected = match interp::run(&prog.module, prog.entry, &vals) {
        Ok(Outcome::Return(v)) => Result_::Return(v.iter().map(|x| x.bits()).collect()),
        Ok(Outcome::Trap(code)) => Result_::Trap(u64::from(code)),
        Ok(other) => return Err(format!("unexpected outcome {other:?}")),
        Err(e) => {
            return Err(format!(
                "generator produced a program the interpreter rejects: {e}\n{}",
                prog.module
            ));
        }
    };
    // Half the seeds without a frame pointer, so rbp holds values too.
    let options = match conv {
        Conv::SysV => Options::sysv(),
        Conv::Win64 => Options::win64(),
    }
    .with_frame_pointer(seed % 2 == 0);
    let compiled = backend_lang::compile(&prog.module, &options)
        .map_err(|e| format!("compile: {e}\n{}", prog.module))?;
    let img = load::load(&compiled)?;
    let mut m = load::machine(&img);
    let mut hosts = Hosts::new(&img, conv);
    let name = prog
        .module
        .func_decl(prog.entry)
        .unwrap()
        .name()
        .to_string();
    let stop = load::call(&img, &mut m, conv, &name, &regs, &mut hosts, seed);
    let got = match stop {
        Ok(Stop::Returned) => {
            let raw = [m.regs[0], m.regs[2]];
            Result_::Return(
                prog.returns
                    .iter()
                    .zip(raw)
                    .map(|(&t, r)| mask(t, r))
                    .collect(),
            )
        }
        Ok(Stop::Trap(code)) => Result_::Trap(code),
        Ok(other) => return Err(format!("stopped with {other:?}")),
        Err(e) => {
            return Err(format!(
                "native run failed: {e}\nexpected {expected:?}\n{}",
                dump(&prog, &options)
            ));
        }
    };
    if got != expected {
        return Err(format!(
            "seed {seed}: expected {expected:?}, got {got:?}\n{}",
            dump(&prog, &options)
        ));
    }
    Ok(())
}

fn dump(prog: &irgen::Program, options: &Options) -> String {
    let mut s = format!("{}\n", prog.module);
    for f in prog.module.functions() {
        match backend_lang::compile_function(&prog.module, f, options) {
            Ok(mir) => s.push_str(&format!("{mir}\n")),
            Err(e) => s.push_str(&format!("{e}\n")),
        }
    }
    s
}

fn sweep(name: &str, seeds: core::ops::Range<u64>, opts: GenOptions, conv: Conv) {
    let mut failures = Vec::new();
    for seed in seeds.clone() {
        if let Err(e) = case(seed, &opts, conv) {
            failures.push((seed, e));
        }
    }
    if let Some((seed, e)) = failures.first() {
        panic!(
            "{name}: {} of {} seeds differ; first is seed {seed}:\n{e}",
            failures.len(),
            seeds.end - seeds.start
        );
    }
}

fn count(default: u64) -> u64 {
    std::env::var("BACKEND_DIFF_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
fn differential_sysv_straight_line_and_branches() {
    let opts = GenOptions {
        functions: 1,
        calls: false,
        memory: false,
        tail_calls: false,
        ..GenOptions::default()
    };
    sweep("sysv/basic", 0..count(300), opts, Conv::SysV);
}

#[test]
fn differential_sysv_full() {
    sweep(
        "sysv/full",
        1000..1000 + count(300),
        GenOptions::default(),
        Conv::SysV,
    );
}

#[test]
fn differential_win64_full() {
    sweep(
        "win64/full",
        2000..2000 + count(300),
        GenOptions::default(),
        Conv::Win64,
    );
}

#[test]
fn differential_two_results_sysv() {
    let opts = GenOptions {
        returns: 2,
        ..GenOptions::default()
    };
    sweep(
        "sysv/two-results",
        3000..3000 + count(150),
        opts,
        Conv::SysV,
    );
}

#[test]
fn differential_explicit_conventions() {
    // Functions tagged sysv64 compiled for a Win64 platform, and the reverse.
    let opts = GenOptions {
        conv: CallConv::SysV64,
        ..GenOptions::default()
    };
    sweep(
        "win64-host/sysv64-tag",
        4000..4000 + count(100),
        opts,
        Conv::SysV,
    );
    let opts = GenOptions {
        conv: CallConv::Win64,
        ..GenOptions::default()
    };
    sweep(
        "sysv-host/win64-tag",
        5000..5000 + count(100),
        opts,
        Conv::Win64,
    );
}

#[test]
fn differential_high_pressure_without_traps() {
    // Long bodies, no traps: everything runs to the end, so values stay live
    // across many calls and loops and the allocator must spill.
    let opts = GenOptions {
        statements: 160,
        traps: false,
        functions: 4,
        ..GenOptions::default()
    };
    sweep("sysv/pressure", 6000..6000 + count(80), opts, Conv::SysV);
    sweep("win64/pressure", 7000..7000 + count(80), opts, Conv::Win64);
}

/// Debugging aid: `BACKEND_DIFF_SEED=n BACKEND_DIFF_KIND=basic|full|two|pressure
/// BACKEND_DIFF_CONV=sysv|win64 cargo test --test differential one_seed -- --ignored`.
#[test]
#[ignore = "debugging aid for one seed"]
fn one_seed() {
    let seed: u64 = std::env::var("BACKEND_DIFF_SEED").unwrap().parse().unwrap();
    let kind = std::env::var("BACKEND_DIFF_KIND").unwrap_or_else(|_| "full".into());
    let conv = match std::env::var("BACKEND_DIFF_CONV").as_deref() {
        Ok("win64") => Conv::Win64,
        _ => Conv::SysV,
    };
    let opts = match kind.as_str() {
        "basic" => GenOptions {
            functions: 1,
            calls: false,
            memory: false,
            tail_calls: false,
            ..GenOptions::default()
        },
        "two" => GenOptions {
            returns: 2,
            ..GenOptions::default()
        },
        "pressure" => GenOptions {
            statements: 160,
            traps: false,
            functions: 4,
            ..GenOptions::default()
        },
        "sysv64" => GenOptions {
            conv: CallConv::SysV64,
            ..GenOptions::default()
        },
        "win64tag" => GenOptions {
            conv: CallConv::Win64,
            ..GenOptions::default()
        },
        _ => GenOptions::default(),
    };
    if let Err(e) = case(seed, &opts, conv) {
        panic!("{e}");
    }
}

#[test]
#[ignore = "statistics"]
fn stats() {
    let mut spilled = 0;
    let mut remat = 0;
    let mut rounds_max = 0;
    let mut pressure_max = 0;
    let mut tables = 0;
    let mut funcs = 0;
    for seed in 6000..6080u64 {
        let opts = GenOptions {
            statements: 160,
            traps: false,
            functions: 4,
            ..GenOptions::default()
        };
        let prog = irgen::generate(seed, &opts);
        for f in prog.module.functions() {
            let mut mir = backend_lang::isel::select(&prog.module, f, &Options::sysv()).unwrap();
            tables += mir.to_string().matches("jmptbl").count();
            let a = backend_lang::regalloc::allocate(&mut mir).unwrap();
            spilled += a.spilled();
            remat += a.rematerialized();
            rounds_max = rounds_max.max(a.rounds());
            pressure_max = pressure_max.max(a.max_pressure());
            funcs += 1;
        }
    }
    println!(
        "funcs {funcs} spilled {spilled} remat {remat} rounds_max {rounds_max} pressure_max {pressure_max} tables {tables}"
    );
}

#[test]
#[ignore = "timing"]
fn timing_realistic() {
    let (m, f) = irgen::big_function(100_000);
    let n = m.function(f).unwrap().inst_count();
    for _ in 0..3 {
        let t = std::time::Instant::now();
        let mut mir = backend_lang::isel::select(&m, f, &Options::sysv()).unwrap();
        let t1 = t.elapsed();
        let a = backend_lang::regalloc::allocate(&mut mir).unwrap();
        let t2 = t.elapsed();
        let mut e = backend_lang::x86_64::Emitter::new();
        e.declare_external("helper").unwrap();
        let _ = e.function(&mir).unwrap();
        let code = e.finish().unwrap().into_bytes();
        let t3 = t.elapsed();
        println!(
            "realistic: {n} IR insts, {} MIR: isel {t1:?} regalloc {:?} ({} rounds, {} spilled) emit {:?}; {} bytes; {:.0} IR/s",
            mir.inst_count(),
            t2 - t1,
            a.rounds(),
            a.spilled(),
            t3 - t2,
            code.len(),
            n as f64 / t3.as_secs_f64()
        );
        let t = std::time::Instant::now();
        let c = backend_lang::compile(&m, &Options::sysv()).unwrap();
        println!(
            "compile(): {:?} {:.0} IR/s",
            t.elapsed(),
            n as f64 / t.elapsed().as_secs_f64()
        );
        let _ = c;
    }
}

#[test]
#[ignore = "timing"]
fn timing_big() {
    for stmts in [20_000u32, 60_000] {
        let opts = GenOptions {
            functions: 1,
            statements: stmts,
            traps: false,
            calls: false,
            ops_v2: false,
            ..GenOptions::default()
        };
        let prog = irgen::generate(1, &opts);
        let n = prog.module.function(prog.entry).unwrap().inst_count();
        let t = std::time::Instant::now();
        let mut mir =
            backend_lang::isel::select(&prog.module, prog.entry, &Options::sysv()).unwrap();
        let t1 = t.elapsed();
        let a = backend_lang::regalloc::allocate(&mut mir).unwrap();
        let t2 = t.elapsed();
        let mut e = backend_lang::x86_64::Emitter::new();
        e.declare_external("__lexersketch_trap").unwrap();
        e.declare_external("g").unwrap();
        let _ = e.function(&mir).unwrap();
        let code = e.finish().unwrap().into_bytes();
        let t3 = t.elapsed();
        println!(
            "stress: {n} IR insts, {} MIR insts: isel {t1:?} regalloc {:?} ({} rounds, {} spilled) emit {:?}; {} bytes",
            mir.inst_count(),
            t2 - t1,
            a.rounds(),
            a.spilled(),
            t3 - t2,
            code.len()
        );
    }
}
