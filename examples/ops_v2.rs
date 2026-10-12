//! OPS v2 through the Tier-1 path: a signed integer `pow` (a `check`, whose
//! error edge receives code 6 for a negative exponent and 1 on overflow), a
//! saturating shift, and a float `pow` that calls the runtime's `ls_pow`.
//! Compiles the module for System V and prints the runtime symbols it
//! imports and each function's disassembly.
//!
//! `cargo run --example ops_v2`

use backend_lang::Options;
use ir_lang::{
    BinaryOp, BlockArg, BlockCall, ConvOp, InstData, Linkage, Module, Overflow, Policy, Shift,
    Signature, Type,
};
use isa_lang::x86_64::decode_all;

fn module() -> Result<Module, Box<dyn std::error::Error>> {
    let mut m = Module::new("ops_v2");

    // ipow(x, e) = x ** e, or -(error code) when it fails.
    let ipow = m.declare_function(
        "ipow",
        &Signature::new(&[Type::I64, Type::I64], &[Type::I64]),
        Linkage::Export,
    )?;
    let mut b = m.build(ipow)?;
    let (x, e) = (b.param(0)?, b.param(1)?);
    let ok = b.create_block(&[Type::I64])?;
    let err = b.create_block(&[Type::U32])?;
    let pow = InstData::Binary {
        op: BinaryOp::Pow,
        policy: Policy::overflow(Overflow::Error),
        args: [x, e],
    };
    b.check(
        pow,
        BlockCall::with_args(ok, &[BlockArg::Result(0)]),
        BlockCall::with_args(err, &[BlockArg::ErrorCode]),
    )?;
    b.switch_to(ok)?;
    let r = b.block_param(ok, 0)?;
    b.ret(&[r])?;
    b.switch_to(err)?;
    let code = b.block_param(err, 0)?;
    let code = b.convert(ConvOp::Zext, Policy::NONE, code, Type::I64)?;
    let zero = b.iconst(Type::I64, 0)?;
    let neg = b.sub(zero, code, Overflow::Wrap)?;
    b.ret(&[neg])?;

    // shl_sat(x, n) = x << n, zero once n reaches the width.
    let shl = m.declare_function(
        "shl_sat",
        &Signature::new(&[Type::U32, Type::U32], &[Type::U32]),
        Linkage::Export,
    )?;
    let mut b = m.build(shl)?;
    let (x, n) = (b.param(0)?, b.param(1)?);
    let s = b.binary(BinaryOp::Shl, Policy::shift(Shift::Saturate), x, n)?;
    b.ret(&[s])?;

    // sqrt_bits(x) = bits(from_bits(x) ** 0.5): floats travel as bits.
    let fpow = m.declare_function(
        "sqrt_bits",
        &Signature::new(&[Type::U64], &[Type::U64]),
        Linkage::Export,
    )?;
    let mut b = m.build(fpow)?;
    let x = b.param(0)?;
    let fx = b.convert(ConvOp::Bitcast, Policy::NONE, x, Type::F64)?;
    let half = b.f64const(0.5)?;
    let p = b.binary(BinaryOp::Pow, Policy::NONE, fx, half)?;
    let r = b.convert(ConvOp::Bitcast, Policy::NONE, p, Type::U64)?;
    b.ret(&[r])?;
    Ok(m)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let m = module()?;
    let c = backend_lang::compile(&m, &Options::sysv())?;
    // The float pow is the runtime's: it must be ir-lang's ls_pow.
    println!("imports: {:?}", c.imports());
    for f in c.functions() {
        println!("\n{} ({} bytes):", f.name(), f.size());
        let code = &c.code()[f.offset()..f.offset() + f.size()];
        for (at, d) in decode_all(code).filter_map(Result::ok) {
            println!("  {at:4x}  {}", d.inst());
        }
    }
    Ok(())
}
