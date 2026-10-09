//! Instruction selection, construct by construct: switches as jump tables
//! and compare trees, tail calls with stack arguments, the atomic subset,
//! over-aligned stack slots, block memory operations at every length up to
//! a few words, and what is refused (each with its reason). Every program
//! that compiles is also run against ir-lang's interpreter.

#![allow(clippy::unwrap_used, reason = "test failures should be loud")]

mod common;

use backend_lang::isel::IselError;
use backend_lang::{CompileError, Options};
use common::Stop;
use common::load::{self, Conv, Hosts};
use ir_lang::interp::{self, Outcome, Val};
use ir_lang::{
    BinaryOp, BlockCall, Call, CallConv, FuncId, Linkage, MemFlags, MemOrder, Module, Overflow,
    PointerWidth, Policy, RmwOp, Signature, SwitchCase, Type, Value,
};

/// Runs `f` of `m` with `args` in the test interpreter under both
/// conventions and checks it against ir-lang's interpreter.
fn agree(m: &Module, f: FuncId, args: &[Val]) {
    let want = interp::run(m, f, args).unwrap();
    let name = m.func_decl(f).unwrap().name().to_string();
    let raw: Vec<u64> = args.iter().map(|v| v.bits()).collect();
    let tag = m.func_signature(f).unwrap().conv;
    for (conv, opts) in [
        (Conv::SysV, Options::sysv()),
        (Conv::Win64, Options::win64()),
    ] {
        // An explicit tag fixes the convention whatever the platform.
        let conv = match tag {
            CallConv::SysV64 => Conv::SysV,
            CallConv::Win64 => Conv::Win64,
            _ => conv,
        };
        let c = backend_lang::compile(m, &opts).unwrap();
        let img = load::load(&c).unwrap();
        let mut mach = load::machine(&img);
        let mut hosts = Hosts::new(&img, conv);
        let stop = load::call(&img, &mut mach, conv, &name, &raw, &mut hosts, 3).unwrap();
        match (&want, stop) {
            (Outcome::Return(v), Stop::Returned) => {
                if let Some(x) = v.first() {
                    let w = x.ty().int_bits().unwrap_or(64);
                    let mask = if w == 64 { u64::MAX } else { (1 << w) - 1 };
                    assert_eq!(mach.regs[0] & mask, x.bits(), "{conv:?} {args:?}\n{m}");
                }
            }
            (Outcome::Trap(c), Stop::Trap(d)) => assert_eq!(u64::from(*c), d),
            (w, s) => panic!("{conv:?}: expected {w:?}, got {s:?}\n{m}"),
        }
    }
}

/// `f(x: ty) -> u64`: a switch over `cases`, each case returning its index.
fn switch_module(ty: Type, cases: &[u64]) -> (Module, FuncId) {
    let mut m = Module::new("sw");
    let f = m
        .declare_function("f", &Signature::new(&[ty], &[Type::U64]), Linkage::Export)
        .unwrap();
    let mut b = m.build(f).unwrap();
    let x = b.param(0).unwrap();
    let ret = b.create_block(&[Type::U64]).unwrap();
    let dflt = b.create_block(&[]).unwrap();
    let mut sc = Vec::new();
    for (i, &k) in cases.iter().enumerate() {
        let blk = b.create_block(&[]).unwrap();
        sc.push(SwitchCase {
            value: k,
            dest: BlockCall::new(blk, &[]),
        });
        let _ = i;
    }
    let blocks: Vec<_> = sc.iter().map(|c| c.dest.block).collect();
    b.switch(x, sc, BlockCall::new(dflt, &[])).unwrap();
    for (i, blk) in blocks.into_iter().enumerate() {
        b.switch_to(blk).unwrap();
        let v = b.iconst(Type::U64, i as i128 + 1).unwrap();
        b.jump(ret, &[v]).unwrap();
    }
    b.switch_to(dflt).unwrap();
    let z = b.iconst(Type::U64, 0).unwrap();
    b.jump(ret, &[z]).unwrap();
    b.switch_to(ret).unwrap();
    let r = b.block_param(ret, 0).unwrap();
    b.ret(&[r]).unwrap();
    (m, f)
}

fn uses_table(m: &Module, f: FuncId) -> bool {
    backend_lang::isel::select(m, f, &Options::sysv())
        .unwrap()
        .to_string()
        .contains("jmptbl")
}

#[test]
fn test_switch_dense_becomes_a_table_sparse_a_tree() {
    let dense: Vec<u64> = (10..30).collect();
    let (m, f) = switch_module(Type::I32, &dense);
    assert!(uses_table(&m, f));
    for x in [
        0i64,
        9,
        10,
        11,
        17,
        29,
        30,
        31,
        -1,
        i64::from(i32::MIN),
        i64::from(i32::MAX),
    ] {
        agree(&m, f, &[Val::i32(x as i32)]);
    }
    let sparse = [1u64, 100, 10_000, 1_000_000, 0xFFFF_FFFF, 7, 8];
    let (m, f) = switch_module(Type::U32, &sparse);
    assert!(!uses_table(&m, f));
    for x in [
        0u64,
        1,
        7,
        8,
        9,
        100,
        101,
        10_000,
        1_000_000,
        0xFFFF_FFFF,
        0xFFFF_FFFE,
    ] {
        agree(&m, f, &[Val::u32(x as u32)]);
    }
    // Wide values on both sides of the sign bit, through a 64-bit tree.
    let wide = [0u64, u64::MAX, 1 << 63, (1 << 63) - 1, 1 << 40, 3];
    let (m, f) = switch_module(Type::I64, &wide);
    for &x in &wide {
        agree(&m, f, &[Val::new(Type::I64, x)]);
        agree(&m, f, &[Val::new(Type::I64, x.wrapping_add(1))]);
    }
    // A dense run starting far from zero, as a table: the bias is a
    // register when it does not fit 32 bits.
    let base = 0x1234_5678_9000u64;
    let far: Vec<u64> = (0..12).map(|k| base + k).collect();
    let (m, f) = switch_module(Type::U64, &far);
    assert!(uses_table(&m, f));
    for x in [base - 1, base, base + 5, base + 11, base + 12, 0, u64::MAX] {
        agree(&m, f, &[Val::u64(x)]);
    }
    // Narrow types: the raw bits are the case values.
    let (m, f) = switch_module(Type::I8, &[0x80, 0xFF, 0, 1, 2, 3, 4]);
    for x in [-128i8, -1, 0, 1, 4, 5, 127] {
        agree(&m, f, &[Val::i8(x)]);
    }
}

#[test]
fn test_switch_density_threshold_is_configurable() {
    let (m, f) = switch_module(Type::I32, &(0..20).collect::<Vec<_>>());
    let never = Options::sysv().with_jump_tables(u32::MAX, 1);
    let mir = backend_lang::isel::select(&m, f, &never).unwrap();
    assert!(!mir.to_string().contains("jmptbl"));
}

/// `f(a..g)` tail-calls `g` with its seven arguments rotated: under SysV
/// the seventh is a stack argument, written into f's own incoming area.
#[test]
fn test_tail_calls_with_stack_arguments() {
    for conv in [CallConv::SysV64, CallConv::Win64] {
        let mut m = Module::new("tc");
        let sig = Signature::new(&[Type::I64; 7], &[Type::I64]).with_conv(conv);
        let g = m.declare_function("g", &sig, Linkage::Export).unwrap();
        let f = m.declare_function("f", &sig, Linkage::Export).unwrap();
        let mut b = m.build(g).unwrap();
        let mut acc = b.param(0).unwrap();
        for i in 1..7 {
            let p = b.param(i).unwrap();
            let k = b.iconst(Type::I64, 10).unwrap();
            let t = b.mul(acc, k, Overflow::Wrap).unwrap();
            acc = b.add(t, p, Overflow::Wrap).unwrap();
        }
        b.ret(&[acc]).unwrap();
        let mut b = m.build(f).unwrap();
        let ps: Vec<Value> = (0..7).map(|i| b.param(i).unwrap()).collect();
        let mut rot = ps.clone();
        rot.rotate_left(2);
        b.tail_call(Call::direct(g, &rot)).unwrap();
        let args: Vec<Val> = (1..=7).map(Val::i64).collect();
        agree(&m, f, &args);
    }
}

#[test]
fn test_atomic_subset() {
    let mut m = Module::new("at");
    let f = m
        .declare_function(
            "f",
            &Signature::new(&[Type::U32, Type::I64], &[Type::I64]),
            Linkage::Export,
        )
        .unwrap();
    let mut b = m.build(f).unwrap();
    let (x, y) = (b.param(0).unwrap(), b.param(1).unwrap());
    let s32 = b.stack_slot(4, 4).unwrap();
    let s64 = b.stack_slot(8, 8).unwrap();
    let (p32, p64) = (b.stack_addr(s32).unwrap(), b.stack_addr(s64).unwrap());
    let _ = b.atomic_store(x, p32, MemOrder::SeqCst).unwrap();
    let _ = b.atomic_store(y, p64, MemOrder::Release).unwrap();
    let _ = b.fence(MemOrder::SeqCst).unwrap();
    let _ = b.fence(MemOrder::Acquire).unwrap();
    let k = b.iconst(Type::U32, 77).unwrap();
    let old = b.atomic_rmw(RmwOp::Xchg, p32, k, MemOrder::AcqRel).unwrap();
    let now = b.atomic_load(Type::U32, p32, MemOrder::Acquire).unwrap();
    let y2 = b.atomic_load(Type::I64, p64, MemOrder::SeqCst).unwrap();
    let old64 = b
        .convert(ir_lang::ConvOp::Zext, Policy::NONE, old, Type::U64)
        .unwrap();
    let old64 = b
        .convert(ir_lang::ConvOp::Bitcast, Policy::NONE, old64, Type::I64)
        .unwrap();
    let now64 = b
        .convert(ir_lang::ConvOp::Zext, Policy::NONE, now, Type::U64)
        .unwrap();
    let now64 = b
        .convert(ir_lang::ConvOp::Bitcast, Policy::NONE, now64, Type::I64)
        .unwrap();
    let t = b.add(old64, now64, Overflow::Wrap).unwrap();
    let r = b.add(t, y2, Overflow::Wrap).unwrap();
    b.ret(&[r]).unwrap();
    agree(&m, f, &[Val::u32(5), Val::i64(1000)]);
    agree(&m, f, &[Val::u32(u32::MAX), Val::i64(-1)]);
}

#[test]
fn test_over_aligned_slots_are_aligned_at_run_time() {
    let mut m = Module::new("al");
    let f = m
        .declare_function(
            "f",
            &Signature::new(&[Type::I64], &[Type::U64]),
            Linkage::Export,
        )
        .unwrap();
    let mut b = m.build(f).unwrap();
    let x = b.param(0).unwrap();
    let s = b.stack_slot(32, 64).unwrap();
    let p = b.stack_addr(s).unwrap();
    let _ = b.store(x, p, MemFlags::new(64)).unwrap();
    let a = b
        .convert(ir_lang::ConvOp::PtrToInt, Policy::NONE, p, Type::U64)
        .unwrap();
    let m63 = b.iconst(Type::U64, 63).unwrap();
    let low = b.binary(BinaryOp::And, Policy::NONE, a, m63).unwrap();
    let v = b.load(Type::I64, p, MemFlags::new(64)).unwrap();
    let v = b
        .convert(ir_lang::ConvOp::Bitcast, Policy::NONE, v, Type::U64)
        .unwrap();
    let r = b.add(low, v, Overflow::Wrap).unwrap();
    b.ret(&[r]).unwrap();
    // low is 0 when aligned, so the result is x.
    agree(&m, f, &[Val::i64(12345)]);
}

/// `f(n)`: fill a buffer, memmove it onto itself shifted by `k`, memcpy it
/// to a second buffer, and return a checksum.
fn block_memory(k: u64) -> (Module, FuncId) {
    let mut m = Module::new("bm");
    let f = m
        .declare_function(
            "f",
            &Signature::new(&[Type::U64, Type::U8], &[Type::U64]),
            Linkage::Export,
        )
        .unwrap();
    let mut b = m.build(f).unwrap();
    let (n, byte) = (b.param(0).unwrap(), b.param(1).unwrap());
    let a = b.stack_slot(96, 8).unwrap();
    let c = b.stack_slot(96, 8).unwrap();
    let (pa, pc) = (b.stack_addr(a).unwrap(), b.stack_addr(c).unwrap());
    let zero = b.iconst(Type::U8, 0).unwrap();
    let full = b.iconst(Type::U64, 96).unwrap();
    let _ = b.memset(pa, zero, full, MemFlags::new(8)).unwrap();
    let _ = b.memset(pc, zero, full, MemFlags::new(8)).unwrap();
    // A pattern: a[i] = i for i < 64.
    for i in 0..64 {
        let off = b.iconst(Type::U64, i).unwrap();
        let at = b.ptr_offset(pa, off).unwrap();
        let v = b.iconst(Type::U8, i * 3 + 1).unwrap();
        let _ = b.store(v, at, MemFlags::new(1)).unwrap();
    }
    let _ = b.memset(pa, byte, n, MemFlags::new(1)).unwrap();
    let koff = b.iconst(Type::U64, k as i128).unwrap();
    let shifted = b.ptr_offset(pa, koff).unwrap();
    let _ = b.memmove(shifted, pa, n, MemFlags::new(1)).unwrap();
    let _ = b.memmove(pa, shifted, n, MemFlags::new(1)).unwrap();
    let _ = b.memcpy(pc, pa, n, MemFlags::new(1)).unwrap();
    let mut acc = b.iconst(Type::U64, 0).unwrap();
    for i in 0..12 {
        let off = b.iconst(Type::U64, i * 8).unwrap();
        let at = b.ptr_offset(pc, off).unwrap();
        let v = b.load(Type::U64, at, MemFlags::new(8)).unwrap();
        let sh = b.iconst(Type::U64, i).unwrap();
        let v = b
            .binary(BinaryOp::Shl, Policy::shift(ir_lang::Shift::Mask), v, sh)
            .unwrap();
        acc = b.binary(BinaryOp::Xor, Policy::NONE, acc, v).unwrap();
    }
    b.ret(&[acc]).unwrap();
    (m, f)
}

#[test]
fn test_block_memory_at_every_length() {
    for k in [0u64, 1, 7, 8, 9, 16] {
        let (m, f) = block_memory(k);
        for n in 0..=(96 - k).min(80) {
            agree(&m, f, &[Val::u64(n), Val::u8(0xA0 + n as u8)]);
        }
    }
}

fn refused(m: &Module) -> &'static str {
    match backend_lang::compile(m, &Options::sysv()) {
        Err(CompileError::Select {
            error: IselError::Unsupported { what, .. },
            ..
        }) => what,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn test_refusals_name_what_they_wait_on() {
    // Floating point.
    let mut m = Module::new("x");
    let f = m
        .declare_function(
            "f",
            &Signature::new(&[Type::F64], &[Type::F64]),
            Linkage::Export,
        )
        .unwrap();
    let mut b = m.build(f).unwrap();
    let x = b.param(0).unwrap();
    b.ret(&[x]).unwrap();
    assert!(refused(&m).contains("P11"));
    // Lock-prefixed read-modify-write.
    let mut m = Module::new("x");
    let f = m
        .declare_function(
            "f",
            &Signature::new(&[Type::I64], &[Type::I64]),
            Linkage::Export,
        )
        .unwrap();
    let mut b = m.build(f).unwrap();
    let x = b.param(0).unwrap();
    let s = b.stack_slot(8, 8).unwrap();
    let p = b.stack_addr(s).unwrap();
    let r = b.atomic_rmw(RmwOp::Add, p, x, MemOrder::SeqCst).unwrap();
    b.ret(&[r]).unwrap();
    assert!(refused(&m).contains("lock"));
    // cmpxchg.
    let mut m = Module::new("x");
    let f = m
        .declare_function(
            "f",
            &Signature::new(&[Type::I64], &[Type::I64]),
            Linkage::Export,
        )
        .unwrap();
    let mut b = m.build(f).unwrap();
    let x = b.param(0).unwrap();
    let s = b.stack_slot(8, 8).unwrap();
    let p = b.stack_addr(s).unwrap();
    let i = b
        .cmpxchg(p, x, x, MemOrder::SeqCst, MemOrder::SeqCst)
        .unwrap();
    let old = b.results(i).first().unwrap();
    b.ret(&[old]).unwrap();
    assert!(refused(&m).contains("cmpxchg"));
    // AAPCS64.
    let mut m = Module::new("x");
    let _ = m
        .declare_function(
            "f",
            &Signature::new(&[], &[]).with_conv(CallConv::Aapcs64),
            Linkage::Export,
        )
        .unwrap();
    let f = m.functions().next().unwrap();
    m.build(f).unwrap().ret(&[]).unwrap();
    assert!(refused(&m).contains("AArch64"));
    // 32-bit pointers.
    let mut m = Module::with_pointer_width("x", PointerWidth::P32);
    let f = m
        .declare_function("f", &Signature::new(&[], &[]), Linkage::Export)
        .unwrap();
    m.build(f).unwrap().ret(&[]).unwrap();
    assert!(refused(&m).contains("32-bit"));
    // Three results under SysV (two return registers).
    let mut m = Module::new("x");
    let f = m
        .declare_function("f", &Signature::new(&[], &[Type::I64; 3]), Linkage::Export)
        .unwrap();
    let mut b = m.build(f).unwrap();
    let k = b.iconst(Type::I64, 1).unwrap();
    b.ret(&[k, k, k]).unwrap();
    assert!(refused(&m).contains("return registers"));
    // A tail call needing more stack than the caller received.
    let mut m = Module::new("x");
    let g = m
        .declare_function("g", &Signature::new(&[Type::I64; 9], &[]), Linkage::Import)
        .unwrap();
    let f = m
        .declare_function("f", &Signature::new(&[], &[]), Linkage::Export)
        .unwrap();
    let mut b = m.build(f).unwrap();
    let k = b.iconst(Type::I64, 1).unwrap();
    b.tail_call(Call::direct(g, &[k; 9])).unwrap();
    assert!(refused(&m).contains("stack-argument space"));
}

#[test]
fn test_imports_and_runtime_symbols_are_relocations() {
    let mut m = Module::new("imp");
    let puts = m
        .declare_function(
            "puts",
            &Signature::new(&[Type::Ptr], &[Type::I32]),
            Linkage::Import,
        )
        .unwrap();
    let msg = m
        .declare_global("msg", ir_lang::Global::bytes(b"hi\0", 1))
        .unwrap();
    let f = m
        .declare_function("f", &Signature::new(&[], &[Type::I32]), Linkage::Export)
        .unwrap();
    let mut b = m.build(f).unwrap();
    let p = b.global_addr(msg).unwrap();
    let c = b.call(Call::direct(puts, &[p])).unwrap();
    let r = b.results(c).first().unwrap();
    b.ret(&[r]).unwrap();
    let c = backend_lang::compile(&m, &Options::sysv()).unwrap();
    assert_eq!(c.imports(), ["puts"]);
    let kinds: Vec<_> = c
        .relocations()
        .iter()
        .map(|r| (r.symbol.as_str(), r.kind))
        .collect();
    use backend_lang::x86_64::RelocKind;
    assert!(kinds.contains(&("puts", RelocKind::Call32)));
    assert!(kinds.contains(&("msg", RelocKind::Pc32)));
    assert!(c.exports().iter().any(|e| e == "f"));
}
