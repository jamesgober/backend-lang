//! OPS conformance: every integer operation ir-lang 2.0 has, on every
//! integer type, under every policy it may carry (plain `wrap`/`trap`/`mask`,
//! and `error` through a `check` terminator), on the OPS edge values
//! (`0, 1, -1, MIN, MAX, MIN+1, MAX-1`, small values, shift amounts around
//! the width), compiled and run in the test interpreter (and natively on
//! x86-64 Linux and Windows) against ir-lang's interpreter.

#![allow(clippy::unwrap_used, reason = "test failures should be loud")]

mod common;

use backend_lang::Options;
use common::Stop;
use common::irgen::INTS;
use common::load::{self, Conv, Hosts};
use ir_lang::interp::{self, Outcome, Val};
use ir_lang::{
    BinaryOp, BlockArg, BlockCall, CmpOp, ConvOp, DivZero, FuncId, InstData, Linkage, Module,
    Overflow, Policy, Shift, Signature, Type, UnaryOp,
};

/// The edge values of `ty`, as raw bits.
fn edges(ty: Type) -> Vec<u64> {
    let w = ty.int_bits().unwrap();
    let mask = if w == 64 { u64::MAX } else { (1u64 << w) - 1 };
    let (min, max) = if ty.is_signed_int() {
        (1u64 << (w - 1), (1u64 << (w - 1)) - 1)
    } else {
        (0, mask)
    };
    let mut v = vec![
        0,
        1,
        2,
        3,
        7,
        mask,
        mask - 1,
        min,
        max,
        min.wrapping_add(1),
        max.wrapping_sub(1),
        u64::from(w) - 1,
        u64::from(w),
        u64::from(w) + 1,
        0x55 & mask,
    ];
    for x in &mut v {
        *x &= mask;
    }
    v.sort_unstable();
    v.dedup();
    v
}

/// The result encoding both sides agree on: the value's bits, or the error
/// code with bit 40 set (an `error` policy's edge), or a trap.
#[derive(Debug, PartialEq, Eq)]
enum Got {
    Value(u64),
    Trap(u64),
}

/// `f(a, b) -> u64`: the operation, with any error edge returning
/// `code | 1 << 40`.
fn module(
    data: impl Fn(ir_lang::Value, ir_lang::Value) -> InstData,
    a_ty: Type,
    res_ty: Type,
    checked: bool,
) -> (Module, FuncId) {
    let mut m = Module::new("ops");
    let f = m
        .declare_function(
            "f",
            &Signature::new(&[a_ty, a_ty], &[Type::U64]),
            Linkage::Export,
        )
        .unwrap();
    let mut b = m.build(f).unwrap();
    let (x, y) = (b.param(0).unwrap(), b.param(1).unwrap());
    let widen = |b: &mut ir_lang::Builder<'_>, v, ty: Type| {
        if ty == Type::U64 {
            v
        } else if ty == Type::Bool {
            b.convert(ConvOp::BoolToInt, Policy::NONE, v, Type::U64)
                .unwrap()
        } else if ty.int_bits() == Some(64) {
            b.convert(ConvOp::Bitcast, Policy::NONE, v, Type::U64)
                .unwrap()
        } else {
            // The raw bits, zero-extended: compare bit patterns.
            let unsigned = Type::int(ty.int_bits().unwrap(), false).unwrap();
            let u = if unsigned == ty {
                v
            } else {
                b.convert(ConvOp::Bitcast, Policy::NONE, v, unsigned)
                    .unwrap()
            };
            b.convert(ConvOp::Zext, Policy::NONE, u, Type::U64).unwrap()
        }
    };
    if checked {
        let ok = b.create_block(&[res_ty]).unwrap();
        let err = b.create_block(&[Type::U32]).unwrap();
        b.check(
            data(x, y),
            BlockCall::with_args(ok, &[BlockArg::Result(0)]),
            BlockCall::with_args(err, &[BlockArg::ErrorCode]),
        )
        .unwrap();
        b.switch_to(ok).unwrap();
        let r = b.block_param(ok, 0).unwrap();
        let w = widen(&mut b, r, res_ty);
        b.ret(&[w]).unwrap();
        b.switch_to(err).unwrap();
        let c = b.block_param(err, 0).unwrap();
        let c = b.convert(ConvOp::Zext, Policy::NONE, c, Type::U64).unwrap();
        let tag = b.iconst(Type::U64, 1 << 40).unwrap();
        let t = b.binary(BinaryOp::Or, Policy::NONE, c, tag).unwrap();
        b.ret(&[t]).unwrap();
    } else {
        let i = b.insert(data(x, y)).unwrap();
        let r = b.results(i).first().unwrap();
        let w = widen(&mut b, r, res_ty);
        b.ret(&[w]).unwrap();
    }
    (m, f)
}

/// Runs `m` on every pair of edge values both ways and compares.
fn compare(m: &Module, f: FuncId, a_ty: Type, label: &str, natives: &mut Vec<(Module, String)>) {
    m.validate().unwrap_or_else(|e| panic!("{label}: {e}\n{m}"));
    let vals = edges(a_ty);
    for conv in [Conv::SysV, Conv::Win64] {
        let opts = match conv {
            Conv::SysV => Options::sysv(),
            Conv::Win64 => Options::win64(),
        };
        let c = backend_lang::compile(m, &opts).unwrap_or_else(|e| panic!("{label}: {e}"));
        let img = load::load(&c).unwrap();
        for &x in &vals {
            for &y in &vals {
                let want = match interp::run(m, f, &[Val::new(a_ty, x), Val::new(a_ty, y)]).unwrap()
                {
                    Outcome::Return(v) => Got::Value(v[0].bits()),
                    Outcome::Trap(code) => Got::Trap(u64::from(code)),
                    other => panic!("{other:?}"),
                };
                // Junk above the width, as a caller may leave it.
                let w = a_ty.int_bits().unwrap();
                let junk = if w == 64 {
                    0
                } else {
                    0xA5A5_A5A5_A5A5_A5A5u64 << w
                };
                let mut mach = load::machine(&img);
                let mut hosts = Hosts::new(&img, conv);
                let got = match load::call(
                    &img,
                    &mut mach,
                    conv,
                    "f",
                    &[x | junk, y | junk],
                    &mut hosts,
                    x ^ y,
                ) {
                    Ok(Stop::Returned) => Got::Value(mach.regs[0]),
                    Ok(Stop::Trap(c)) => Got::Trap(c),
                    other => panic!("{label} {conv:?} ({x:#x}, {y:#x}): {other:?}"),
                };
                assert_eq!(got, want, "{label} {conv:?} ({x:#x}, {y:#x})\n{m}");
            }
        }
    }
    natives.push((m.clone(), label.to_string()));
}

#[cfg(all(target_arch = "x86_64", any(target_os = "linux", windows)))]
fn run_native(cases: &[(Module, String)], tys: &[Type]) {
    use common::native::{Native, Ran};
    for ((m, label), &a_ty) in cases.iter().zip(tys) {
        let f = m.functions().next().unwrap();
        let vals = edges(a_ty);
        let c = backend_lang::compile(m, &Options::host()).unwrap();
        let conv = if cfg!(windows) {
            Conv::Win64
        } else {
            Conv::SysV
        };
        let n = Native::load(&c, conv).unwrap();
        for &x in &vals {
            for &y in &vals {
                let want = match interp::run(m, f, &[Val::new(a_ty, x), Val::new(a_ty, y)]).unwrap()
                {
                    Outcome::Return(v) => Ran::Returned(v[0].bits(), 0),
                    Outcome::Trap(code) => Ran::Trap(u64::from(code)),
                    other => panic!("{other:?}"),
                };
                let got = match n.call("f", &[x, y]).unwrap() {
                    Ran::Returned(a, _) => Ran::Returned(a, 0),
                    t => t,
                };
                assert_eq!(got, want, "native {label} ({x:#x}, {y:#x})");
            }
        }
    }
}

#[cfg(not(all(target_arch = "x86_64", any(target_os = "linux", windows))))]
fn run_native(_: &[(Module, String)], _: &[Type]) {}

fn binary_policies(op: BinaryOp) -> Vec<(Policy, bool)> {
    use DivZero as D;
    use Overflow as O;
    match op {
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul => vec![
            (Policy::overflow(O::Wrap), false),
            (Policy::overflow(O::Trap), false),
            (Policy::overflow(O::Error), true),
        ],
        BinaryOp::Div | BinaryOp::FloorDiv => {
            let mut v = Vec::new();
            for o in [O::Wrap, O::Trap, O::Error] {
                for d in [D::Trap, D::Error] {
                    let checked = o == O::Error || d == D::Error;
                    v.push((Policy::overflow(o).with_div_zero(d), checked));
                }
            }
            v
        }
        BinaryOp::Rem | BinaryOp::FloorMod => vec![
            (Policy::div_zero(D::Trap), false),
            (Policy::div_zero(D::Error), true),
        ],
        BinaryOp::Shl | BinaryOp::Shr => vec![
            (Policy::shift(Shift::Mask), false),
            (Policy::shift(Shift::Error), true),
        ],
        _ => vec![(Policy::NONE, false)],
    }
}

#[test]
fn test_every_binary_operation_under_every_policy() {
    let mut cases = Vec::new();
    let mut tys = Vec::new();
    for ty in INTS {
        for op in [
            BinaryOp::Add,
            BinaryOp::Sub,
            BinaryOp::Mul,
            BinaryOp::Div,
            BinaryOp::Rem,
            BinaryOp::FloorDiv,
            BinaryOp::FloorMod,
            BinaryOp::And,
            BinaryOp::Or,
            BinaryOp::Xor,
            BinaryOp::Shl,
            BinaryOp::Shr,
            BinaryOp::Min,
            BinaryOp::Max,
        ] {
            for (policy, checked) in binary_policies(op) {
                let (m, f) = module(
                    |x, y| InstData::Binary {
                        op,
                        policy,
                        args: [x, y],
                    },
                    ty,
                    ty,
                    checked,
                );
                compare(&m, f, ty, &format!("{ty} {op} {policy}"), &mut cases);
                tys.push(ty);
            }
        }
    }
    // 8 types x (and/or/xor/min/max + add/sub/mul x 3 + div/floor_div x 6
    // + rem/floor_mod x 2 + shl/shr x 2) = 8 x 34.
    assert_eq!(cases.len(), 272);
    run_native(&cases, &tys);
}

#[test]
fn test_every_unary_operation_under_every_policy() {
    let mut cases = Vec::new();
    let mut tys = Vec::new();
    for ty in INTS {
        for op in [UnaryOp::Neg, UnaryOp::Abs, UnaryOp::Not] {
            let policies = if op == UnaryOp::Not {
                vec![(Policy::NONE, false)]
            } else {
                vec![
                    (Policy::overflow(Overflow::Wrap), false),
                    (Policy::overflow(Overflow::Trap), false),
                    (Policy::overflow(Overflow::Error), true),
                ]
            };
            for (policy, checked) in policies {
                let (m, f) = module(
                    |x, _| InstData::Unary { op, policy, arg: x },
                    ty,
                    ty,
                    checked,
                );
                compare(&m, f, ty, &format!("{ty} {op} {policy}"), &mut cases);
                tys.push(ty);
            }
        }
    }
    run_native(&cases, &tys);
}

#[test]
fn test_every_comparison_and_select() {
    let mut cases = Vec::new();
    let mut tys = Vec::new();
    for ty in INTS {
        for op in [
            CmpOp::Eq,
            CmpOp::Ne,
            CmpOp::Lt,
            CmpOp::Le,
            CmpOp::Gt,
            CmpOp::Ge,
        ] {
            let (m, f) = module(
                |x, y| InstData::Compare { op, args: [x, y] },
                ty,
                Type::Bool,
                false,
            );
            compare(&m, f, ty, &format!("{ty} {op}"), &mut cases);
            tys.push(ty);
        }
        // select (x < y) ? x : y, the compare fused into the select.
        let mut m = Module::new("sel");
        let f = m
            .declare_function(
                "f",
                &Signature::new(&[ty, ty], &[Type::U64]),
                Linkage::Export,
            )
            .unwrap();
        let mut b = m.build(f).unwrap();
        let (x, y) = (b.param(0).unwrap(), b.param(1).unwrap());
        let c = b.compare(CmpOp::Lt, x, y).unwrap();
        let s = b.select(c, x, y).unwrap();
        let u = Type::int(ty.int_bits().unwrap(), false).unwrap();
        let s = if u == ty {
            s
        } else {
            b.convert(ConvOp::Bitcast, Policy::NONE, s, u).unwrap()
        };
        let s = if u == Type::U64 {
            s
        } else {
            b.convert(ConvOp::Zext, Policy::NONE, s, Type::U64).unwrap()
        };
        b.ret(&[s]).unwrap();
        compare(&m, f, ty, &format!("{ty} select"), &mut cases);
        tys.push(ty);
    }
    run_native(&cases, &tys);
}

#[test]
fn test_every_conversion_under_every_policy() {
    let mut cases = Vec::new();
    let mut tys = Vec::new();
    for from in INTS {
        for to in INTS {
            let fw = from.int_bits().unwrap();
            let tw = to.int_bits().unwrap();
            let mut ops: Vec<(ConvOp, Policy, bool)> = Vec::new();
            if from != to {
                ops.push((ConvOp::IntCast, Policy::overflow(Overflow::Wrap), false));
                ops.push((ConvOp::IntCast, Policy::overflow(Overflow::Trap), false));
                ops.push((ConvOp::IntCast, Policy::overflow(Overflow::Error), true));
            }
            if tw > fw {
                ops.push((ConvOp::Zext, Policy::NONE, false));
                ops.push((ConvOp::Sext, Policy::NONE, false));
            }
            if tw < fw {
                ops.push((ConvOp::Narrow, Policy::NONE, false));
            }
            if tw == fw && from != to {
                ops.push((ConvOp::Bitcast, Policy::NONE, false));
            }
            for (op, policy, checked) in ops {
                let (m, f) = module(
                    |x, _| InstData::Convert {
                        op,
                        policy,
                        to,
                        arg: x,
                    },
                    from,
                    to,
                    checked,
                );
                compare(
                    &m,
                    f,
                    from,
                    &format!("{from} {op} {policy} -> {to}"),
                    &mut cases,
                );
                tys.push(from);
            }
        }
    }
    // char_from_u32 on the boundaries of the scalar values.
    let (m, f) = module(
        |x, _| InstData::Convert {
            op: ConvOp::CharFromU32,
            policy: Policy::NONE,
            to: Type::U32,
            arg: x,
        },
        Type::U32,
        Type::U32,
        true,
    );
    compare(&m, f, Type::U32, "char_from_u32", &mut cases);
    tys.push(Type::U32);
    for v in [0xD7FFu64, 0xD800, 0xDFFF, 0xE000, 0x10_FFFF, 0x11_0000] {
        let want = interp::run(&m, f, &[Val::u32(v as u32), Val::u32(0)]).unwrap();
        let ok = matches!(&want, Outcome::Return(r) if r[0].bits() == v);
        assert_eq!(ok, !(0xD800..=0xDFFF).contains(&v) && v <= 0x10_FFFF);
    }
    run_native(&cases, &tys);
}
