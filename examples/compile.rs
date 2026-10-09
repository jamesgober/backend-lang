//! The Tier-1 path: an ir-lang module in, x86-64 machine code out.
//!
//! Builds a module with a loop (sum of squares), a checked division whose
//! error edge returns a sentinel, and a caller; compiles it for System V;
//! and prints each function's allocated machine IR and its disassembly.
//!
//! `cargo run --example compile`

use backend_lang::Options;
use ir_lang::{
    BinaryOp, BlockArg, BlockCall, CmpOp, DivZero, InstData, Linkage, Module, Overflow, Policy,
    Signature, Type,
};
use isa_lang::x86_64::decode_all;

fn module() -> Result<Module, Box<dyn std::error::Error>> {
    let mut m = Module::new("demo");
    let i64s = |n| vec![Type::I64; n];

    // sum_sq(n) = 0^2 + 1^2 + ... + (n-1)^2
    let sum_sq = m.declare_function(
        "sum_sq",
        &Signature::new(&i64s(1), &i64s(1)),
        Linkage::Export,
    )?;
    let mut b = m.build(sum_sq)?;
    let n = b.param(0)?;
    let head = b.create_block(&[Type::I64, Type::I64])?;
    let body = b.create_block(&[])?;
    let exit = b.create_block(&[Type::I64])?;
    let zero = b.iconst(Type::I64, 0)?;
    b.jump(head, &[zero, zero])?;
    b.switch_to(head)?;
    let (i, acc) = (b.block_param(head, 0)?, b.block_param(head, 1)?);
    let more = b.compare(CmpOp::Lt, i, n)?;
    b.branch(more, body, &[], exit, &[acc])?;
    b.switch_to(body)?;
    let sq = b.mul(i, i, Overflow::Wrap)?;
    let acc2 = b.add(acc, sq, Overflow::Wrap)?;
    let one = b.iconst(Type::I64, 1)?;
    let i2 = b.add(i, one, Overflow::Wrap)?;
    b.jump(head, &[i2, acc2])?;
    b.switch_to(exit)?;
    let r = b.block_param(exit, 0)?;
    b.ret(&[r])?;

    // safe_div(a, b) = a / b, or -1 when b is zero (the check's error edge).
    let safe_div = m.declare_function(
        "safe_div",
        &Signature::new(&i64s(2), &i64s(1)),
        Linkage::Export,
    )?;
    let mut b = m.build(safe_div)?;
    let (x, y) = (b.param(0)?, b.param(1)?);
    let ok = b.create_block(&[Type::I64])?;
    let err = b.create_block(&[Type::U32])?;
    let div = InstData::Binary {
        op: BinaryOp::Div,
        policy: Policy::overflow(Overflow::Wrap).with_div_zero(DivZero::Error),
        args: [x, y],
    };
    b.check(
        div,
        BlockCall::with_args(ok, &[BlockArg::Result(0)]),
        BlockCall::with_args(err, &[BlockArg::ErrorCode]),
    )?;
    b.switch_to(ok)?;
    let q = b.block_param(ok, 0)?;
    b.ret(&[q])?;
    b.switch_to(err)?;
    let minus_one = b.iconst(Type::I64, -1)?;
    b.ret(&[minus_one])?;

    // main() = sum_sq(10) + safe_div(100, 7)
    let main = m.declare_function("main", &Signature::new(&[], &i64s(1)), Linkage::Export)?;
    let mut b = m.build(main)?;
    let ten = b.iconst(Type::I64, 10)?;
    let c1 = b.call(ir_lang::Call::direct(sum_sq, &[ten]))?;
    let s = b.results(c1).first().ok_or("no result")?;
    let (h, d) = (b.iconst(Type::I64, 100)?, b.iconst(Type::I64, 7)?);
    let c2 = b.call(ir_lang::Call::direct(safe_div, &[h, d]))?;
    let q = b.results(c2).first().ok_or("no result")?;
    let total = b.add(s, q, Overflow::Wrap)?;
    b.ret(&[total])?;
    Ok(m)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let m = module()?;
    let options = Options::sysv();
    for f in m.functions() {
        let mir = backend_lang::compile_function(&m, f, &options)?;
        println!("{mir}");
    }
    let compiled = backend_lang::compile(&m, &options)?;
    for f in compiled.functions() {
        println!("{} (offset {}, {} bytes):", f.name(), f.offset(), f.size());
        let code = &compiled.code()[f.offset()..f.offset() + f.size()];
        for item in decode_all(code) {
            match item {
                Ok((at, d)) => println!("  {:4x}  {}", f.offset() + at, d.inst()),
                Err(e) => println!("  <{e}>"),
            }
        }
    }
    println!("{} bytes of code", compiled.code().len());
    Ok(())
}
