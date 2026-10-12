//! Hostile and huge inputs: nothing panics, every pass stays iterative, and
//! the budgets hold.
//!
//! - Any OPS v2 operation (`pow`, `shift = saturate`) on any integer type,
//!   under any policy, with any constant or run-time operands, compiles (a
//!   valid module always does) and its code terminates.
//! - The largest constant exponents unroll to a bounded sequence; run-time
//!   exponents loop at most 64 times.
//! - Huge functions: a 300,000-instruction function, 20,000 nested branches
//!   built through the variables API, a join with 2,000 parameters, and
//!   10,000 run-time `pow` loops in one function compile without recursion.
//! - The register allocator's live-set budget is checked against the live
//!   sets of the function as given; spill rounds only shrink them, so a
//!   function that fits the budget allocates however much it spills.

#![allow(clippy::unwrap_used, reason = "test failures should be loud")]

mod common;

use backend_lang::mir::{Effects, Function, Mem};
use backend_lang::regalloc::{self, Options as RaOptions, RegAllocError};
use backend_lang::x86_64::{GPR, RAX, RDI, SYSV, op};
use backend_lang::{CompileError, Options};
use common::Stop;
use common::irgen::{self, INTS, Rng};
use common::load::{self, Conv, Hosts};
use ir_lang::interp::{self, Outcome, Val};
use ir_lang::{
    BinaryOp, BlockArg, BlockCall, CmpOp, ConvOp, InstData, Linkage, Module, Overflow, Policy,
    Shift, Signature, Type,
};
use proptest::prelude::*;

/// `f(x, y) -> u64` applying `op` to `x` and `y` (or constants in their
/// place), as a `check` when the operation can fail.
fn ops_v2_module(
    op: BinaryOp,
    policy: Policy,
    ty: Type,
    kx: Option<u64>,
    ky: Option<u64>,
) -> Module {
    let mut m = Module::new("h");
    let f = m
        .declare_function(
            "f",
            &Signature::new(&[ty, ty], &[Type::U64]),
            Linkage::Export,
        )
        .unwrap();
    let mut b = m.build(f).unwrap();
    let (x, y) = (b.param(0).unwrap(), b.param(1).unwrap());
    let x = match kx {
        Some(k) => b.iconst(ty, irgen::as_i128(ty, k)).unwrap(),
        None => x,
    };
    let y = match ky {
        Some(k) => b.iconst(ty, irgen::as_i128(ty, k)).unwrap(),
        None => y,
    };
    let widen = |b: &mut ir_lang::Builder<'_>, v| {
        let u = Type::int(ty.int_bits().unwrap(), false).unwrap();
        let v = if u == ty {
            v
        } else {
            b.convert(ConvOp::Bitcast, Policy::NONE, v, u).unwrap()
        };
        if u == Type::U64 {
            v
        } else {
            b.convert(ConvOp::Zext, Policy::NONE, v, Type::U64).unwrap()
        }
    };
    if op.can_error(policy, ty) {
        let ok = b.create_block(&[ty]).unwrap();
        let err = b.create_block(&[Type::U32]).unwrap();
        b.check(
            InstData::Binary {
                op,
                policy,
                args: [x, y],
            },
            BlockCall::with_args(ok, &[BlockArg::Result(0)]),
            BlockCall::with_args(err, &[BlockArg::ErrorCode]),
        )
        .unwrap();
        b.switch_to(ok).unwrap();
        let r = b.block_param(ok, 0).unwrap();
        let w = widen(&mut b, r);
        b.ret(&[w]).unwrap();
        b.switch_to(err).unwrap();
        let c = b.block_param(err, 0).unwrap();
        let c = b.convert(ConvOp::Zext, Policy::NONE, c, Type::U64).unwrap();
        let tag = b.iconst(Type::U64, 1 << 40).unwrap();
        let t = b.binary(BinaryOp::Or, Policy::NONE, c, tag).unwrap();
        b.ret(&[t]).unwrap();
    } else {
        let r = b.binary(op, policy, x, y).unwrap();
        let w = widen(&mut b, r);
        b.ret(&[w]).unwrap();
    }
    drop(b);
    m
}

/// A run's result: the returned value or the trap code.
#[derive(Debug, PartialEq, Eq)]
enum Got {
    Value(u64),
    Trap(u64),
}

fn run_both(m: &Module, ty: Type, x: u64, y: u64) -> Result<(), String> {
    let f = m.functions().next().unwrap();
    let want = match interp::run(m, f, &[Val::new(ty, x), Val::new(ty, y)]).unwrap() {
        Outcome::Return(v) => Got::Value(v[0].bits()),
        Outcome::Trap(c) => Got::Trap(u64::from(c)),
        other => return Err(format!("{other:?}")),
    };
    let c = backend_lang::compile(m, &Options::sysv()).map_err(|e| e.to_string())?;
    let img = load::load(&c)?;
    let mut mach = load::machine(&img);
    // A step limit far above what any of these needs: the code terminates.
    mach.max_steps = 100_000;
    let mut hosts = Hosts::new(&img, Conv::SysV);
    let got = match load::call(&img, &mut mach, Conv::SysV, "f", &[x, y], &mut hosts, x ^ y)? {
        Stop::Returned => Got::Value(mach.regs[0]),
        Stop::Trap(c) => Got::Trap(c),
        other => return Err(format!("{other:?}")),
    };
    if got == want {
        Ok(())
    } else {
        Err(format!("got {got:?}, want {want:?}"))
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_any_ops_v2_operation_compiles_terminates_and_agrees(
        which in 0usize..5,
        ty_k in 0usize..8,
        x in any::<u64>(),
        y in any::<u64>(),
        const_x in any::<bool>(),
        const_y in any::<bool>(),
    ) {
        let ty = INTS[ty_k];
        let w = ty.int_bits().unwrap();
        let mask = if w == 64 { u64::MAX } else { (1u64 << w) - 1 };
        let (x, y) = (x & mask, y & mask);
        let (op, policy) = match which {
            0 => (BinaryOp::Pow, Policy::overflow(Overflow::Wrap)),
            1 => (BinaryOp::Pow, Policy::overflow(Overflow::Trap)),
            2 => (BinaryOp::Pow, Policy::overflow(Overflow::Error)),
            3 => (BinaryOp::Shl, Policy::shift(Shift::Saturate)),
            _ => (BinaryOp::Shr, Policy::shift(Shift::Saturate)),
        };
        let m = ops_v2_module(op, policy, ty, const_x.then_some(x), const_y.then_some(y));
        prop_assert!(m.validate().is_ok());
        let r = run_both(&m, ty, x, y);
        prop_assert!(r.is_ok(), "{ty} {op} {policy} ({x:#x}, {y:#x}): {r:?}\n{m}");
    }
}

#[test]
fn test_largest_constant_exponents_unroll_to_bounded_code() {
    // An all-ones exponent: 64 squarings and 63 multiplications at most.
    for ty in [Type::U64, Type::U8, Type::U32] {
        let k = u64::MAX >> (64 - ty.int_bits().unwrap());
        let m = ops_v2_module(
            BinaryOp::Pow,
            Policy::overflow(Overflow::Wrap),
            ty,
            None,
            Some(k),
        );
        let c = backend_lang::compile(&m, &Options::sysv()).unwrap();
        assert!(c.code().len() < 4096, "{ty}: {} bytes", c.code().len());
        for x in [0u64, 1, 2, 3, u64::MAX] {
            run_both(&m, ty, x, 0).unwrap();
        }
    }
}

#[test]
fn test_huge_function_compiles() {
    let (m, f) = irgen::big_function(300_000);
    let n = m.function(f).unwrap().inst_count();
    assert!(n >= 300_000);
    let c = backend_lang::compile(&m, &Options::sysv()).unwrap();
    assert!(c.function("big").is_some());
}

#[test]
fn test_deeply_nested_branches_through_the_variables_api() {
    // 20,000 nested ifs, each assigning a variable in its then-arm: a
    // dominator tree 20,000 deep and a join chain as long. Every walk is a
    // loop, so this is a matter of time, not stack.
    const DEPTH: usize = 20_000;
    let mut m = Module::new("deep");
    let f = m
        .declare_function(
            "f",
            &Signature::new(&[Type::I64], &[Type::I64]),
            Linkage::Export,
        )
        .unwrap();
    let mut b = m.build(f).unwrap();
    let x = b.param(0).unwrap();
    let acc = b.declare_var(Type::I64).unwrap();
    b.def_var(acc, x).unwrap();
    let mut joins = Vec::with_capacity(DEPTH);
    for k in 0..DEPTH {
        let (then, join) = (b.create_block(&[]).unwrap(), b.create_block(&[]).unwrap());
        let a = b.use_var(acc).unwrap();
        let lim = b.iconst(Type::I64, (k % 97) as i128).unwrap();
        let c = b.compare(CmpOp::Gt, a, lim).unwrap();
        b.branch(c, then, &[], join, &[]).unwrap();
        b.switch_to(then).unwrap();
        b.seal_block(then).unwrap();
        let one = b.iconst(Type::I64, 1).unwrap();
        let a2 = b.sub(a, one, Overflow::Wrap).unwrap();
        b.def_var(acc, a2).unwrap();
        joins.push(join);
    }
    // Close the joins innermost first: each falls through to the next one
    // out, so the then-chain nests.
    for k in (0..DEPTH).rev() {
        b.jump(joins[k], &[]).unwrap();
        b.switch_to(joins[k]).unwrap();
        b.seal_block(joins[k]).unwrap();
    }
    let a = b.use_var(acc).unwrap();
    b.ret(&[a]).unwrap();
    b.seal_all_blocks().unwrap();
    drop(b);
    m.validate().unwrap();
    let c = backend_lang::compile(&m, &Options::sysv()).unwrap();
    let img = load::load(&c).unwrap();
    for x in [0u64, 50, 200, 30_000] {
        let want = match interp::run(&m, f, &[Val::i64(x as i64)]).unwrap() {
            Outcome::Return(v) => v[0].bits(),
            other => panic!("{other:?}"),
        };
        let mut mach = load::machine(&img);
        let mut hosts = Hosts::new(&img, Conv::SysV);
        let stop = load::call(&img, &mut mach, Conv::SysV, "f", &[x], &mut hosts, x);
        assert_eq!(stop, Ok(Stop::Returned));
        assert_eq!(mach.regs[0], want, "x {x}");
    }
}

#[test]
fn test_a_join_with_two_thousand_parameters() {
    const N: usize = 2_000;
    let mut m = Module::new("wide");
    let f = m
        .declare_function(
            "f",
            &Signature::new(&[Type::I64], &[Type::I64]),
            Linkage::Export,
        )
        .unwrap();
    let mut b = m.build(f).unwrap();
    let x = b.param(0).unwrap();
    let join = b.create_block(&[Type::I64; N]).unwrap();
    let (l, r) = (b.create_block(&[]).unwrap(), b.create_block(&[]).unwrap());
    let zero = b.iconst(Type::I64, 0).unwrap();
    let c = b.compare(CmpOp::Lt, x, zero).unwrap();
    b.branch(c, l, &[], r, &[]).unwrap();
    for (blk, sign) in [(l, 1i128), (r, -1i128)] {
        b.switch_to(blk).unwrap();
        let args: Vec<_> = (0..N)
            .map(|k| {
                let kk = b.iconst(Type::I64, sign * k as i128).unwrap();
                b.add(x, kk, Overflow::Wrap).unwrap()
            })
            .collect();
        // Reversed on one side: the edges are different permutations.
        let args: Vec<_> = if sign < 0 {
            args.into_iter().rev().collect()
        } else {
            args
        };
        b.jump(join, &args).unwrap();
    }
    b.switch_to(join).unwrap();
    let mut acc = b.block_param(join, 0).unwrap();
    for k in 1..N {
        let p = b.block_param(join, k).unwrap();
        let sh = b.iconst(Type::I64, (k % 61) as i128).unwrap();
        let p = b
            .binary(BinaryOp::Shl, Policy::shift(Shift::Mask), p, sh)
            .unwrap();
        acc = b.binary(BinaryOp::Xor, Policy::NONE, acc, p).unwrap();
    }
    b.ret(&[acc]).unwrap();
    drop(b);
    let c = backend_lang::compile(&m, &Options::sysv()).unwrap();
    let img = load::load(&c).unwrap();
    for x in [5u64, (-5i64) as u64] {
        let want = match interp::run(&m, f, &[Val::i64(x as i64)]).unwrap() {
            Outcome::Return(v) => v[0].bits(),
            other => panic!("{other:?}"),
        };
        let mut mach = load::machine(&img);
        let mut hosts = Hosts::new(&img, Conv::SysV);
        let stop = load::call(&img, &mut mach, Conv::SysV, "f", &[x], &mut hosts, x);
        assert_eq!(stop, Ok(Stop::Returned));
        assert_eq!(mach.regs[0], want);
    }
}

#[test]
fn test_ten_thousand_pow_loops_in_one_function() {
    let mut m = Module::new("pows");
    let f = m
        .declare_function(
            "f",
            &Signature::new(&[Type::U64, Type::U64], &[Type::U64]),
            Linkage::Export,
        )
        .unwrap();
    let mut b = m.build(f).unwrap();
    let (x, e) = (b.param(0).unwrap(), b.param(1).unwrap());
    let mut acc = x;
    let mut rng = Rng::new(9);
    for _ in 0..10_000 {
        let k = b.iconst(Type::U64, rng.below(16) as i128).unwrap();
        let ek = b.binary(BinaryOp::Xor, Policy::NONE, e, k).unwrap();
        let p = b
            .binary(BinaryOp::Pow, Policy::overflow(Overflow::Wrap), acc, ek)
            .unwrap();
        acc = b.binary(BinaryOp::Or, Policy::NONE, p, x).unwrap();
    }
    b.ret(&[acc]).unwrap();
    drop(b);
    let c = backend_lang::compile(&m, &Options::sysv()).unwrap();
    let img = load::load(&c).unwrap();
    let want = match interp::run(&m, f, &[Val::u64(3), Val::u64(5)]).unwrap() {
        Outcome::Return(v) => v[0].bits(),
        other => panic!("{other:?}"),
    };
    let mut mach = load::machine(&img);
    let mut hosts = Hosts::new(&img, Conv::SysV);
    let stop = load::call(&img, &mut mach, Conv::SysV, "f", &[3, 5], &mut hosts, 1);
    assert_eq!(stop, Ok(Stop::Returned));
    assert_eq!(mach.regs[0], want);
}

/// `n` values computed from `rdi` in the entry, live through a chain of
/// `k` blocks, summed in the last: exactly `n * (k - 1)` live-set entries,
/// and more values than registers, so it spills.
fn live_chain(n: usize, k: usize) -> Function {
    let mut f = Function::new("chain", &SYSV);
    let blocks: Vec<_> = (0..k).map(|_| f.block().unwrap()).collect();
    f.add_param(blocks[0], RDI.into()).unwrap();
    let x = f.vreg(GPR).unwrap();
    f.push(blocks[0], op::COPY, &[x.into(), RDI.into()])
        .unwrap();
    let vals: Vec<_> = (0..n)
        .map(|i| {
            let v = f.vreg(GPR).unwrap();
            // An address with a register in it: not rematerializable, so
            // spilling stores it.
            let at = Mem::base(x).with_disp(i as i32);
            f.push(blocks[0], op::LEA64, &[v.into(), at.into()])
                .unwrap();
            v
        })
        .collect();
    for w in blocks.windows(2) {
        let to = f.block_call(w[1], &[]).unwrap();
        f.push(w[0], op::JMP, &[to.into()]).unwrap();
    }
    let last = blocks[k - 1];
    let mut acc = vals[0];
    for &v in &vals[1..] {
        let s = f.vreg(GPR).unwrap();
        f.push(last, op::ADD64, &[s.into(), acc.into(), v.into()])
            .unwrap();
        acc = s;
    }
    f.push(last, op::COPY, &[RAX.into(), acc.into()]).unwrap();
    f.push_with(last, op::RET, &[], Effects::new().uses(&[RAX]))
        .unwrap();
    f
}

#[test]
fn test_live_budget_holds_and_spill_rounds_do_not_count_against_it() {
    let (n, k) = (40, 50);
    let exact = n * (k - 1);
    let mut f = live_chain(n, k);
    let a = regalloc::allocate_with(&mut f, &RaOptions::new().with_live_budget(exact)).unwrap();
    assert!(a.spilled() > 0 && a.rounds() > 1, "{a:?}");
    let mut f = live_chain(n, k);
    assert_eq!(
        regalloc::allocate_with(&mut f, &RaOptions::new().with_live_budget(exact - 1)).unwrap_err(),
        RegAllocError::TooLarge
    );
    // Through `compile`, the budget comes from `Options::regalloc` and the
    // failure names the function.
    let (m, _) = irgen::big_function(20_000);
    let o = Options::sysv().with_regalloc(RaOptions::new().with_live_budget(10));
    match backend_lang::compile(&m, &o) {
        Err(CompileError::Allocate { function, error }) => {
            assert_eq!(error, RegAllocError::TooLarge);
            assert_eq!(function, "big");
        }
        other => panic!("{other:?}"),
    }
}
