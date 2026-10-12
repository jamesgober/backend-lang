//! Stack maps and landing pads, end to end against ir-lang's interpreter.
//!
//! **Stack maps.** Programs allocate GC objects, keep the references across
//! safepoints (calls to a collector, calls to functions that collect, loops
//! that carry references around a back edge), and read and write the
//! objects through derived pointers. The native side runs under a *moving*
//! collector written here: at every collection it walks the machine stack
//! frame by frame using only the compiled call-site records and frame
//! layouts, moves every object a stack map names, rewrites the slots, and
//! poisons the old copies. A reference the backend failed to record, or
//! failed to reload after the call, reads poison and the result differs; a
//! call site without a record fails the walk outright. The interpreter, for
//! its part, rejects any reference not relocated through a GC list
//! (`StaleRef`), so the programs themselves are checked too.
//!
//! **Exactness.** Every safepoint's stack map lists exactly its GC list:
//! one slot per reference, distinct slots, nothing else.
//!
//! **Landing pads.** `invoke` is lowered with a landing-pad record per call
//! site; no unwinding tables exist yet (ROADMAP), so this file carries a
//! minimal unwinder over the same records: it restores the callee-saved
//! registers each unwound frame pushed, using the frame layout, and enters
//! the landing pad with the payload in `rax`. `resume` hands the payload
//! back to it.

#![allow(clippy::unwrap_used, reason = "test failures should be loud")]

mod common;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use backend_lang::{Compiled, Options};
use common::irgen::Rng;
use common::load::{self, Conv, Hosts};
use common::{CODE_BASE, HEAP_BASE, Machine, SENTINEL, Stop};
use ir_lang::interp::{self, HostReturn, InterpError, Interpreter, Memory, Outcome, Val};
use ir_lang::{
    Attrs, BinaryOp, BlockArg, BlockCall, Call, CmpOp, ConvOp, FuncId, Linkage, MemFlags, Module,
    Overflow, Policy, Signature, Type, Value,
};

// ---------------------------------------------------------------- programs

const FIELDS: u64 = 2;

struct GcProgram {
    module: Module,
    main: FuncId,
}

/// A random GC program: objects allocated, referenced across collections
/// and calls, carried around a loop, read and written through derived
/// pointers; returns a checksum of everything it saw.
fn gc_program(seed: u64) -> GcProgram {
    let mut rng = Rng::new(seed);
    let mut m = Module::new("gc");
    let alloc = m
        .declare_function(
            "alloc",
            &Signature::new(&[Type::U64], &[Type::Ref]),
            Linkage::Import,
        )
        .unwrap();
    m.set_attrs(alloc, Attrs::NOGC).unwrap();
    let collect = m
        .declare_function("collect", &Signature::new(&[], &[]), Linkage::Import)
        .unwrap();
    // touch(r, k): collect with r live, then r.field0 += k; returns the old
    // field.
    let touch = m
        .declare_function(
            "touch",
            &Signature::new(&[Type::Ref, Type::I64], &[Type::I64]),
            Linkage::Export,
        )
        .unwrap();
    {
        let mut b = m.build(touch).unwrap();
        let (r, k) = (b.param(0).unwrap(), b.param(1).unwrap());
        let c = b.call(Call::direct(collect, &[]).with_gc(&[r])).unwrap();
        let r2 = b.results(c).first().unwrap();
        let p = b
            .convert(ConvOp::RefToPtr, Policy::NONE, r2, Type::Ptr)
            .unwrap();
        let old = b.load(Type::I64, p, MemFlags::new(8)).unwrap();
        let new = b.add(old, k, Overflow::Wrap).unwrap();
        let _ = b.store(new, p, MemFlags::new(8)).unwrap();
        b.ret(&[old]).unwrap();
    }
    let n = 1 + rng.below(6) as usize;
    let main = m
        .declare_function(
            "main",
            &Signature::new(&[Type::I64], &[Type::I64]),
            Linkage::Export,
        )
        .unwrap();
    let mut b = m.build(main).unwrap();
    let seed_v = b.param(0).unwrap();
    let size = b.iconst(Type::U64, (8 * FIELDS) as i128).unwrap();
    let mut refs: Vec<Value> = Vec::new();
    for i in 0..n {
        let c = b.call(Call::direct(alloc, &[size])).unwrap();
        let r = b.results(c).first().unwrap();
        let p = b
            .convert(ConvOp::RefToPtr, Policy::NONE, r, Type::Ptr)
            .unwrap();
        let k = b.iconst(Type::I64, (i as i128 + 1) * 1000).unwrap();
        let v = b.add(seed_v, k, Overflow::Wrap).unwrap();
        let _ = b.store(v, p, MemFlags::new(8)).unwrap();
        let eight = b.iconst(Type::U64, 8).unwrap();
        let p1 = b.ptr_offset(p, eight).unwrap();
        let _ = b.store(k, p1, MemFlags::new(8)).unwrap();
        refs.push(r);
    }
    // A loop carrying the references (as block parameters) around its
    // back edge, collecting inside.
    let mut header_types = vec![Type::I32, Type::I64];
    header_types.extend(std::iter::repeat_n(Type::Ref, n));
    let header = b.create_block(&header_types).unwrap();
    let body = b.create_block(&[]).unwrap();
    let mut exit_types = vec![Type::I64];
    exit_types.extend(std::iter::repeat_n(Type::Ref, n));
    let exit = b.create_block(&exit_types).unwrap();
    let zero32 = b.iconst(Type::I32, 0).unwrap();
    let zero = b.iconst(Type::I64, 0).unwrap();
    let mut args = vec![zero32, zero];
    args.extend(&refs);
    b.jump(header, &args).unwrap();
    b.switch_to(header).unwrap();
    let i = b.block_param(header, 0).unwrap();
    let acc = b.block_param(header, 1).unwrap();
    let mut cur: Vec<Value> = (0..n)
        .map(|k| b.block_param(header, k + 2).unwrap())
        .collect();
    let trips = b.iconst(Type::I32, 1 + rng.below(5) as i128).unwrap();
    let more = b.compare(CmpOp::Lt, i, trips).unwrap();
    let mut out = vec![acc];
    out.extend(&cur);
    b.branch(more, body, &[], exit, &out).unwrap();
    b.switch_to(body).unwrap();
    let mut acc = acc;
    for _ in 0..1 + rng.below(6) {
        match rng.below(4) {
            0 => {
                // A collection with every reference live.
                let c = b.call(Call::direct(collect, &[]).with_gc(&cur)).unwrap();
                cur = b.results(c).iter().collect();
            }
            1 => {
                // A call that collects, with one reference as an argument
                // and all of them in the caller's list.
                let j = rng.below(n as u64) as usize;
                let k = b.iconst(Type::I64, rng.below(100) as i128).unwrap();
                let c = b
                    .call(Call::direct(touch, &[cur[j], k]).with_gc(&cur))
                    .unwrap();
                let res: Vec<Value> = b.results(c).iter().collect();
                acc = b.add(acc, res[0], Overflow::Wrap).unwrap();
                cur = res[1..].to_vec();
            }
            2 => {
                // Read a field through a fresh derived pointer.
                let j = rng.below(n as u64) as usize;
                let p = b
                    .convert(ConvOp::RefToPtr, Policy::NONE, cur[j], Type::Ptr)
                    .unwrap();
                let off = b
                    .iconst(Type::U64, (8 * rng.below(FIELDS)) as i128)
                    .unwrap();
                let a = b.ptr_offset(p, off).unwrap();
                let v = b.load(Type::I64, a, MemFlags::new(8)).unwrap();
                acc = b.binary(BinaryOp::Xor, Policy::NONE, acc, v).unwrap();
            }
            _ => {
                // Write one.
                let j = rng.below(n as u64) as usize;
                let p = b
                    .convert(ConvOp::RefToPtr, Policy::NONE, cur[j], Type::Ptr)
                    .unwrap();
                let _ = b.store(acc, p, MemFlags::new(8)).unwrap();
            }
        }
    }
    let one = b.iconst(Type::I32, 1).unwrap();
    let i2 = b.add(i, one, Overflow::Wrap).unwrap();
    let mut back = vec![i2, acc];
    back.extend(&cur);
    b.jump(header, &back).unwrap();
    b.switch_to(exit).unwrap();
    let mut total = b.block_param(exit, 0).unwrap();
    for k in 0..n {
        let r = b.block_param(exit, k + 1).unwrap();
        let p = b
            .convert(ConvOp::RefToPtr, Policy::NONE, r, Type::Ptr)
            .unwrap();
        for f in 0..FIELDS {
            let off = b.iconst(Type::U64, (8 * f) as i128).unwrap();
            let a = b.ptr_offset(p, off).unwrap();
            let v = b.load(Type::I64, a, MemFlags::new(8)).unwrap();
            let rot = b
                .iconst(Type::I64, (k as i128 * 3 + f as i128) & 63)
                .unwrap();
            let v = b
                .binary(BinaryOp::Shl, Policy::shift(ir_lang::Shift::Mask), v, rot)
                .unwrap();
            total = b.add(total, v, Overflow::Wrap).unwrap();
        }
    }
    b.ret(&[total]).unwrap();
    GcProgram { module: m, main }
}

// ------------------------------------------------------- reference side

struct IrHost;

impl interp::Host for IrHost {
    fn call(
        &mut self,
        name: &str,
        args: &[Val],
        mem: &mut Memory,
    ) -> Result<HostReturn, InterpError> {
        match name {
            "alloc" => Ok(HostReturn::Values(vec![mem.alloc_object(args[0].bits())?])),
            "collect" => Ok(HostReturn::Values(vec![])),
            "thrower" => {
                let x = args[0].bits();
                if x & 1 == 1 {
                    Ok(HostReturn::Unwind(Val::ptr(0x1000 + x)))
                } else {
                    Ok(HostReturn::Values(vec![Val::i64(x as i64 + 1)]))
                }
            }
            other => Err(InterpError::Host(format!("unknown import {other}"))),
        }
    }
}

fn reference(m: &Module, f: FuncId, args: &[Val]) -> Outcome {
    let mut vm = Interpreter::new(m, IrHost).unwrap();
    vm.call(f, args).unwrap()
}

// ----------------------------------------------------------- native side

/// The function a code address is in, and its record of the call returning
/// there.
fn frame_at(
    c: &Compiled,
    ret: u64,
) -> Option<(
    &backend_lang::x86_64::EmittedFunction,
    Option<&backend_lang::x86_64::CallSiteRecord>,
)> {
    let off = ret.checked_sub(CODE_BASE)? as usize;
    let f = c
        .functions()
        .iter()
        .find(|f| off > f.offset() && off <= f.offset() + f.size())?;
    let rec = f.call_sites().iter().find(|s| s.return_offset() == off);
    Some((f, rec))
}

/// A moving collector over the heap region.
struct Heap {
    next: u64,
    objects: BTreeMap<u64, u64>,
    collections: u32,
    roots_seen: u32,
}

impl Heap {
    fn new() -> Heap {
        Heap {
            next: HEAP_BASE + 16,
            objects: BTreeMap::new(),
            collections: 0,
            roots_seen: 0,
        }
    }

    fn alloc(&mut self, size: u64) -> u64 {
        let at = self.next;
        self.next += (size + 15) & !15;
        let _ = self.objects.insert(at, size);
        at
    }

    /// Walks every frame from `rsp` (pointing at a return address), moving
    /// each object a stack map names and rewriting its slot.
    fn collect(&mut self, c: &Compiled, m: &mut Machine) -> Result<(), String> {
        self.collections += 1;
        let mut moved: BTreeMap<u64, u64> = BTreeMap::new();
        let mut rsp = m.regs[4];
        loop {
            let ret = m.read(rsp, 8)?;
            if ret == SENTINEL {
                break;
            }
            let (f, rec) = frame_at(c, ret)
                .ok_or_else(|| format!("return address {ret:#x} is in no function"))?;
            let rec = rec.ok_or_else(|| {
                format!(
                    "the call returning to {ret:#x} in {} has no stack map",
                    f.name()
                )
            })?;
            let body = rsp + 8;
            for &slot in rec.gc_slots() {
                let at = (body as i64 + i64::from(slot)) as u64;
                let old = m.read(at, 8)?;
                if old == 0 {
                    continue;
                }
                self.roots_seen += 1;
                let new = match moved.get(&old) {
                    Some(&n) => n,
                    None => {
                        let size = *self
                            .objects
                            .get(&old)
                            .ok_or_else(|| format!("slot holds {old:#x}, not an object"))?;
                        let n = self.alloc(size);
                        for k in (0..size).step_by(8) {
                            let v = m.read(old + k, 8)?;
                            m.write(n + k, 8, v)?;
                            m.write(old + k, 8, 0xEEEE_EEEE_EEEE_EEEE)?;
                        }
                        let _ = self.objects.remove(&old);
                        let _ = moved.insert(old, n);
                        n
                    }
                };
                m.write(at, 8, new)?;
            }
            rsp = body + u64::from(f.frame().cfa_offset()) - 8;
        }
        Ok(())
    }
}

/// Unwinds from the frame whose return address is at `rsp`, restoring each
/// unwound frame's callee-saved registers, to the first landing pad.
fn unwind(c: &Compiled, m: &mut Machine, payload: u64) -> Result<Option<Stop>, String> {
    let mut rsp = m.regs[4];
    loop {
        let ret = m.read(rsp, 8)?;
        if ret == SENTINEL {
            m.regs[4] = rsp + 8;
            return Ok(Some(Stop::Unwound(payload)));
        }
        let (f, rec) =
            frame_at(c, ret).ok_or_else(|| format!("return address {ret:#x} is in no function"))?;
        let body = rsp + 8;
        if let Some(pad) = rec.and_then(|r| r.landing_pad()) {
            m.regs[4] = body;
            m.regs[0] = payload;
            m.redirect = Some(pad);
            return Ok(None);
        }
        let frame = f.frame();
        let cfa = body + u64::from(frame.cfa_offset());
        let mut at = cfa - 16;
        if frame.frame_pointer() {
            m.regs[5] = m.read(at, 8)?;
            at -= 8;
        }
        for r in frame.pushed() {
            m.regs[r.index()] = m.read(at, 8)?;
            at -= 8;
        }
        rsp = cfa - 8;
    }
}

fn native(
    c: &Compiled,
    conv: Conv,
    func: &str,
    args: &[u64],
    seed: u64,
) -> (Result<Stop, String>, u64, Heap) {
    let img = load::load(c).unwrap();
    let mut m = load::machine(&img);
    let heap = Rc::new(RefCell::new(Heap::new()));
    let mut hosts = Hosts::new(&img, conv);
    {
        let heap = Rc::clone(&heap);
        hosts.on("alloc", move |m, conv| {
            let size = m.regs[conv.args()[0]];
            m.regs[0] = heap.borrow_mut().alloc(size);
            Ok(None)
        });
    }
    {
        let heap = Rc::clone(&heap);
        hosts.on("collect", move |m, _| {
            heap.borrow_mut().collect(c, m)?;
            Ok(None)
        });
    }
    hosts.on("thrower", move |m, conv| {
        let x = m.regs[conv.args()[0]];
        if x & 1 == 1 {
            unwind(c, m, 0x1000 + x)
        } else {
            m.regs[0] = x + 1;
            Ok(None)
        }
    });
    hosts.on("__lexersketch_resume", move |m, conv| {
        let payload = m.regs[conv.args()[0]];
        unwind(c, m, payload)
    });
    let stop = load::call(&img, &mut m, conv, func, args, &mut hosts, seed);
    let rax = m.regs[0];
    drop(hosts);
    let heap = Rc::try_unwrap(heap).ok().unwrap().into_inner();
    (stop, rax, heap)
}

fn options(conv: Conv) -> Options {
    match conv {
        Conv::SysV => Options::sysv(),
        Conv::Win64 => Options::win64(),
    }
}

// ----------------------------------------------------------------- tests

#[test]
fn test_moving_collector_sees_every_reference_and_relocation_is_honoured() {
    let mut collections = 0;
    let mut roots = 0;
    for seed in 0..200u64 {
        let p = gc_program(seed);
        p.module
            .validate()
            .unwrap_or_else(|e| panic!("seed {seed}: {e}\n{}", p.module));
        let arg = (seed as i64).wrapping_mul(7919);
        let want = match reference(&p.module, p.main, &[Val::i64(arg)]) {
            Outcome::Return(v) => v[0].bits(),
            other => panic!("seed {seed}: {other:?}"),
        };
        for conv in [Conv::SysV, Conv::Win64] {
            let opts = options(conv).with_frame_pointer(seed % 3 != 0);
            let c = backend_lang::compile(&p.module, &opts).unwrap();
            let (stop, rax, heap) = native(&c, conv, "main", &[arg as u64], seed);
            assert_eq!(
                stop,
                Ok(Stop::Returned),
                "seed {seed} {conv:?}\n{}",
                p.module
            );
            assert_eq!(rax, want, "seed {seed} {conv:?}\n{}", p.module);
            collections += heap.collections;
            roots += heap.roots_seen;
        }
    }
    assert!(
        collections > 300 && roots > 1000,
        "{collections} collections, {roots} roots"
    );
}

#[test]
fn test_stack_maps_are_exactly_the_gc_lists() {
    for seed in 0..100u64 {
        let p = gc_program(seed);
        // Each safepoint call in the IR, in order, with its GC-list length.
        let mut lists: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for f in p.module.functions() {
            let Some(body) = p.module.function(f) else {
                continue;
            };
            let name = p.module.func_decl(f).unwrap().name().to_string();
            let mut v = Vec::new();
            for b in body.blocks() {
                for i in body.insts(b) {
                    if let Some(ir_lang::InstData::Call(call)) = body.inst(i) {
                        let nogc = match call.target {
                            ir_lang::CallTarget::Direct(t) => {
                                p.module.func_decl(t).unwrap().attrs().contains(Attrs::NOGC)
                            }
                            _ => false,
                        };
                        if !nogc {
                            v.push(call.gc.len());
                        }
                    }
                }
            }
            let _ = lists.insert(name, v);
        }
        let c = backend_lang::compile(&p.module, &Options::sysv()).unwrap();
        for f in c.functions() {
            let want = &lists[f.name()];
            let got: Vec<usize> = f.call_sites().iter().map(|s| s.gc_slots().len()).collect();
            let mut want_sorted = want.clone();
            want_sorted.sort_unstable();
            let mut got_sorted = got.clone();
            got_sorted.sort_unstable();
            assert_eq!(got_sorted, want_sorted, "seed {seed}: {}", f.name());
            for s in f.call_sites() {
                let mut slots = s.gc_slots().to_vec();
                slots.sort_unstable();
                slots.dedup();
                assert_eq!(slots.len(), s.gc_slots().len(), "distinct slots");
                for &o in s.gc_slots() {
                    assert!(
                        o >= 0 && (o as u32) < f.frame().cfa_offset(),
                        "slot inside the frame"
                    );
                    assert_eq!(o % 8, 0, "aligned");
                }
            }
        }
    }
}

/// `f(x)`: invoke thrower(x); normal: result * 2; landing pad: the payload
/// plus 5 and the relocated reference's field.
fn invoke_module(cleanup: bool) -> (Module, FuncId) {
    let mut m = Module::new("eh");
    let thrower = m
        .declare_function(
            "thrower",
            &Signature::new(&[Type::I64], &[Type::I64]),
            Linkage::Import,
        )
        .unwrap();
    let alloc = m
        .declare_function(
            "alloc",
            &Signature::new(&[Type::U64], &[Type::Ref]),
            Linkage::Import,
        )
        .unwrap();
    m.set_attrs(alloc, Attrs::NOGC).unwrap();
    // inner(x, r): invoke thrower with r in the GC list; the landing pad
    // writes through the relocated reference, then resumes (cleanup) or
    // returns.
    let inner = m
        .declare_function(
            "inner",
            &Signature::new(&[Type::I64, Type::Ref], &[Type::I64]),
            Linkage::Export,
        )
        .unwrap();
    {
        let mut b = m.build(inner).unwrap();
        let (x, r) = (b.param(0).unwrap(), b.param(1).unwrap());
        let ok = b.create_block(&[Type::I64, Type::Ref]).unwrap();
        let pad = b.create_block(&[Type::Ptr, Type::Ref]).unwrap();
        b.invoke(
            Call::direct(thrower, &[x]).with_gc(&[r]),
            BlockCall::with_args(ok, &[BlockArg::Result(0), BlockArg::Result(1)]),
            BlockCall::with_args(pad, &[BlockArg::Exception, BlockArg::Result(1)]),
        )
        .unwrap();
        b.switch_to(ok).unwrap();
        let v = b.block_param(ok, 0).unwrap();
        let two = b.iconst(Type::I64, 2).unwrap();
        let d = b.mul(v, two, Overflow::Wrap).unwrap();
        b.ret(&[d]).unwrap();
        b.switch_to(pad).unwrap();
        let exn = b.block_param(pad, 0).unwrap();
        let r2 = b.block_param(pad, 1).unwrap();
        let p = b
            .convert(ConvOp::RefToPtr, Policy::NONE, r2, Type::Ptr)
            .unwrap();
        let e = b
            .convert(ConvOp::PtrToInt, Policy::NONE, exn, Type::I64)
            .unwrap();
        let _ = b.store(e, p, MemFlags::new(8)).unwrap();
        if cleanup {
            b.resume(exn).unwrap();
        } else {
            let five = b.iconst(Type::I64, 5).unwrap();
            let s = b.add(e, five, Overflow::Wrap).unwrap();
            b.ret(&[s]).unwrap();
        }
    }
    // outer(x): allocate, call inner through invoke, return what came back
    // or what the landing pad saw plus the object's field.
    let outer = m
        .declare_function(
            "outer",
            &Signature::new(&[Type::I64], &[Type::I64]),
            Linkage::Export,
        )
        .unwrap();
    {
        let mut b = m.build(outer).unwrap();
        let x = b.param(0).unwrap();
        let size = b.iconst(Type::U64, 8).unwrap();
        let c = b.call(Call::direct(alloc, &[size])).unwrap();
        let r = b.results(c).first().unwrap();
        let p = b
            .convert(ConvOp::RefToPtr, Policy::NONE, r, Type::Ptr)
            .unwrap();
        let k = b.iconst(Type::I64, 1000).unwrap();
        let _ = b.store(k, p, MemFlags::new(8)).unwrap();
        let ok = b.create_block(&[Type::I64, Type::Ref]).unwrap();
        let pad = b.create_block(&[Type::Ptr, Type::Ref]).unwrap();
        b.invoke(
            Call::direct(inner, &[x, r]).with_gc(&[r]),
            BlockCall::with_args(ok, &[BlockArg::Result(0), BlockArg::Result(1)]),
            BlockCall::with_args(pad, &[BlockArg::Exception, BlockArg::Result(1)]),
        )
        .unwrap();
        for (blk, from_pad) in [(ok, false), (pad, true)] {
            b.switch_to(blk).unwrap();
            let a = b.block_param(blk, 0).unwrap();
            let r2 = b.block_param(blk, 1).unwrap();
            let a = if from_pad {
                b.convert(ConvOp::PtrToInt, Policy::NONE, a, Type::I64)
                    .unwrap()
            } else {
                a
            };
            let p = b
                .convert(ConvOp::RefToPtr, Policy::NONE, r2, Type::Ptr)
                .unwrap();
            let field = b.load(Type::I64, p, MemFlags::new(8)).unwrap();
            let s = b.add(a, field, Overflow::Wrap).unwrap();
            b.ret(&[s]).unwrap();
        }
    }
    (m, outer)
}

#[test]
fn test_invoke_landing_pads_and_resume_through_a_minimal_unwinder() {
    for cleanup in [false, true] {
        let (m, outer) = invoke_module(cleanup);
        m.validate().unwrap_or_else(|e| panic!("{e}\n{m}"));
        for x in [0u64, 1, 2, 7, 40, 41] {
            let want = match reference(&m, outer, &[Val::i64(x as i64)]) {
                Outcome::Return(v) => v[0].bits(),
                other => panic!("{other:?}"),
            };
            for conv in [Conv::SysV, Conv::Win64] {
                for fp in [true, false] {
                    let c =
                        backend_lang::compile(&m, &options(conv).with_frame_pointer(fp)).unwrap();
                    let (stop, rax, _) = native(&c, conv, "outer", &[x], x);
                    assert_eq!(
                        stop,
                        Ok(Stop::Returned),
                        "x {x} cleanup {cleanup} {conv:?} fp {fp}"
                    );
                    assert_eq!(rax, want, "x {x} cleanup {cleanup} {conv:?} fp {fp}");
                }
            }
        }
    }
}

#[test]
fn test_unwinding_out_of_the_entry_matches_the_interpreter() {
    let (m, _) = invoke_module(true);
    let inner = m
        .functions()
        .find(|&f| m.func_decl(f).unwrap().name() == "inner")
        .unwrap();
    // inner resumes: with no landing pad above it, the run unwinds out.
    let alloc_free_ref = Val::new(Type::Ref, 0);
    let _ = alloc_free_ref;
    let mut vm = Interpreter::new(&m, IrHost).unwrap();
    let r = vm.memory_mut().alloc_object(8).unwrap();
    let want = vm.call(inner, &[Val::i64(3), r]).unwrap();
    assert_eq!(want, Outcome::Unwind(Val::ptr(0x1003)));
    let c = backend_lang::compile(&m, &Options::sysv()).unwrap();
    let (stop, _, _) = native_with_object(&c, "inner", 3);
    assert_eq!(stop, Ok(Stop::Unwound(0x1003)));
}

fn native_with_object(c: &Compiled, func: &str, x: u64) -> (Result<Stop, String>, u64, Heap) {
    // The object the caller would pass: allocated up front in the heap.
    let obj = HEAP_BASE + 8;
    let (stop, rax, heap) = native(c, Conv::SysV, func, &[x, obj], 9);
    (stop, rax, heap)
}

#[test]
fn test_call_sites_without_safepoints_have_no_stack_map() {
    // A nogc call records nothing; a safepoint with an empty list records
    // an empty map.
    let mut m = Module::new("m");
    let quiet = m
        .declare_function("quiet", &Signature::new(&[], &[]), Linkage::Import)
        .unwrap();
    m.set_attrs(quiet, Attrs::NOGC).unwrap();
    let loud = m
        .declare_function("loud", &Signature::new(&[], &[]), Linkage::Import)
        .unwrap();
    let f = m
        .declare_function("f", &Signature::new(&[], &[]), Linkage::Export)
        .unwrap();
    let mut b = m.build(f).unwrap();
    let _ = b.call(Call::direct(quiet, &[])).unwrap();
    let _ = b.call(Call::direct(loud, &[])).unwrap();
    b.ret(&[]).unwrap();
    let c = backend_lang::compile(&m, &Options::sysv()).unwrap();
    let sites = c.functions()[0].call_sites();
    assert_eq!(sites.len(), 1);
    assert!(sites[0].gc_slots().is_empty());
}

// ------------------------------------------- statepoints by ir-lang itself

/// [`gc_program`]'s shape built the way a front end lowers locals: every
/// reference and the accumulator are variables (ir-lang's variables API
/// places the loop's block parameters), and no call carries a GC list.
/// `insert_statepoints` then adds the lists, relocates the references live
/// across each safepoint, and repairs SSA with block parameters where the
/// relocated versions meet.
fn gc_program_vars(seed: u64) -> (GcProgram, ir_lang::StatepointStats) {
    let mut rng = Rng::new(seed);
    let mut m = Module::new("gcv");
    let alloc = m
        .declare_function(
            "alloc",
            &Signature::new(&[Type::U64], &[Type::Ref]),
            Linkage::Import,
        )
        .unwrap();
    m.set_attrs(alloc, Attrs::NOGC).unwrap();
    let collect = m
        .declare_function("collect", &Signature::new(&[], &[]), Linkage::Import)
        .unwrap();
    // touch(r, k): collect with r live, then r.field0 += k; returns the old
    // field. No GC list: insert_statepoints relocates r.
    let touch = m
        .declare_function(
            "touch",
            &Signature::new(&[Type::Ref, Type::I64], &[Type::I64]),
            Linkage::Export,
        )
        .unwrap();
    let mut stats = ir_lang::StatepointStats::default();
    {
        let mut b = m.build(touch).unwrap();
        let (r, k) = (b.param(0).unwrap(), b.param(1).unwrap());
        let _ = b.call(Call::direct(collect, &[])).unwrap();
        let p = b
            .convert(ConvOp::RefToPtr, Policy::NONE, r, Type::Ptr)
            .unwrap();
        let old = b.load(Type::I64, p, MemFlags::new(8)).unwrap();
        let new = b.add(old, k, Overflow::Wrap).unwrap();
        let _ = b.store(new, p, MemFlags::new(8)).unwrap();
        b.ret(&[old]).unwrap();
        let s = b.insert_statepoints().unwrap();
        stats.safepoints += s.safepoints;
        stats.relocations += s.relocations;
        stats.params += s.params;
    }
    let n = 1 + rng.below(6) as usize;
    let main = m
        .declare_function(
            "main",
            &Signature::new(&[Type::I64], &[Type::I64]),
            Linkage::Export,
        )
        .unwrap();
    let mut b = m.build(main).unwrap();
    let seed_v = b.param(0).unwrap();
    let size = b.iconst(Type::U64, (8 * FIELDS) as i128).unwrap();
    let refs: Vec<ir_lang::Variable> = (0..n).map(|_| b.declare_var(Type::Ref).unwrap()).collect();
    let acc = b.declare_var(Type::I64).unwrap();
    let i = b.declare_var(Type::I32).unwrap();
    for (k, &var) in refs.iter().enumerate() {
        let c = b.call(Call::direct(alloc, &[size])).unwrap();
        let r = b.results(c).first().unwrap();
        let p = b
            .convert(ConvOp::RefToPtr, Policy::NONE, r, Type::Ptr)
            .unwrap();
        let kk = b.iconst(Type::I64, (k as i128 + 1) * 1000).unwrap();
        let v = b.add(seed_v, kk, Overflow::Wrap).unwrap();
        let _ = b.store(v, p, MemFlags::new(8)).unwrap();
        let eight = b.iconst(Type::U64, 8).unwrap();
        let p1 = b.ptr_offset(p, eight).unwrap();
        let _ = b.store(kk, p1, MemFlags::new(8)).unwrap();
        b.def_var(var, r).unwrap();
    }
    let zero = b.iconst(Type::I64, 0).unwrap();
    b.def_var(acc, zero).unwrap();
    let zero32 = b.iconst(Type::I32, 0).unwrap();
    b.def_var(i, zero32).unwrap();
    let (head, body, exit) = (
        b.create_block(&[]).unwrap(),
        b.create_block(&[]).unwrap(),
        b.create_block(&[]).unwrap(),
    );
    b.jump(head, &[]).unwrap();
    b.switch_to(head).unwrap();
    let iv = b.use_var(i).unwrap();
    let trips = b.iconst(Type::I32, 1 + rng.below(5) as i128).unwrap();
    let more = b.compare(CmpOp::Lt, iv, trips).unwrap();
    b.branch(more, body, &[], exit, &[]).unwrap();
    b.switch_to(body).unwrap();
    b.seal_block(body).unwrap();
    for _ in 0..1 + rng.below(6) {
        match rng.below(5) {
            0 => {
                let _ = b.call(Call::direct(collect, &[])).unwrap();
            }
            1 => {
                let j = rng.below(n as u64) as usize;
                let r = b.use_var(refs[j]).unwrap();
                let k = b.iconst(Type::I64, rng.below(100) as i128).unwrap();
                let c = b.call(Call::direct(touch, &[r, k])).unwrap();
                let res = b.results(c).first().unwrap();
                let a = b.use_var(acc).unwrap();
                let a2 = b.add(a, res, Overflow::Wrap).unwrap();
                b.def_var(acc, a2).unwrap();
            }
            2 => {
                // Swap two references: their variables carry other objects
                // around the loop from here on.
                let (j, k) = (rng.below(n as u64) as usize, rng.below(n as u64) as usize);
                let (rj, rk) = (b.use_var(refs[j]).unwrap(), b.use_var(refs[k]).unwrap());
                b.def_var(refs[j], rk).unwrap();
                b.def_var(refs[k], rj).unwrap();
            }
            3 => {
                let j = rng.below(n as u64) as usize;
                let r = b.use_var(refs[j]).unwrap();
                let p = b
                    .convert(ConvOp::RefToPtr, Policy::NONE, r, Type::Ptr)
                    .unwrap();
                let off = b
                    .iconst(Type::U64, (8 * rng.below(FIELDS)) as i128)
                    .unwrap();
                let a = b.ptr_offset(p, off).unwrap();
                let v = b.load(Type::I64, a, MemFlags::new(8)).unwrap();
                let x = b.use_var(acc).unwrap();
                let x2 = b.binary(BinaryOp::Xor, Policy::NONE, x, v).unwrap();
                b.def_var(acc, x2).unwrap();
            }
            _ => {
                let j = rng.below(n as u64) as usize;
                let r = b.use_var(refs[j]).unwrap();
                let p = b
                    .convert(ConvOp::RefToPtr, Policy::NONE, r, Type::Ptr)
                    .unwrap();
                let x = b.use_var(acc).unwrap();
                let _ = b.store(x, p, MemFlags::new(8)).unwrap();
            }
        }
    }
    let iv = b.use_var(i).unwrap();
    let one = b.iconst(Type::I32, 1).unwrap();
    let i2 = b.add(iv, one, Overflow::Wrap).unwrap();
    b.def_var(i, i2).unwrap();
    b.jump(head, &[]).unwrap();
    b.seal_block(head).unwrap();
    b.switch_to(exit).unwrap();
    b.seal_block(exit).unwrap();
    let mut total = b.use_var(acc).unwrap();
    for (k, &var) in refs.iter().enumerate() {
        let r = b.use_var(var).unwrap();
        let p = b
            .convert(ConvOp::RefToPtr, Policy::NONE, r, Type::Ptr)
            .unwrap();
        for f in 0..FIELDS {
            let off = b.iconst(Type::U64, (8 * f) as i128).unwrap();
            let a = b.ptr_offset(p, off).unwrap();
            let v = b.load(Type::I64, a, MemFlags::new(8)).unwrap();
            let rot = b
                .iconst(Type::I64, (k as i128 * 3 + f as i128) & 63)
                .unwrap();
            let v = b
                .binary(BinaryOp::Shl, Policy::shift(ir_lang::Shift::Mask), v, rot)
                .unwrap();
            total = b.add(total, v, Overflow::Wrap).unwrap();
        }
    }
    b.ret(&[total]).unwrap();
    b.seal_all_blocks().unwrap();
    let s = b.insert_statepoints().unwrap();
    stats.safepoints += s.safepoints;
    stats.relocations += s.relocations;
    stats.params += s.params;
    (GcProgram { module: m, main }, stats)
}

#[test]
fn test_statepoints_inserted_by_ir_lang_survive_a_moving_collector() {
    let (mut relocations, mut params, mut collections) = (0, 0, 0);
    for seed in 0..120u64 {
        let (p, stats) = gc_program_vars(seed);
        p.module
            .validate()
            .unwrap_or_else(|e| panic!("seed {seed}: {e}\n{}", p.module));
        relocations += stats.relocations;
        params += stats.params;
        let arg = (seed as i64).wrapping_mul(6007);
        // The interpreter rejects any reference used after a safepoint
        // without being relocated through its list.
        let want = match reference(&p.module, p.main, &[Val::i64(arg)]) {
            Outcome::Return(v) => v[0].bits(),
            other => panic!("seed {seed}: {other:?}"),
        };
        for conv in [Conv::SysV, Conv::Win64] {
            let opts = options(conv).with_frame_pointer(seed % 2 == 0);
            let c = backend_lang::compile(&p.module, &opts).unwrap();
            let (stop, rax, heap) = native(&c, conv, "main", &[arg as u64], seed);
            assert_eq!(
                stop,
                Ok(Stop::Returned),
                "seed {seed} {conv:?}\n{}",
                p.module
            );
            assert_eq!(rax, want, "seed {seed} {conv:?}\n{}", p.module);
            collections += heap.collections;
        }
        on_hardware(&p, arg, want, seed);
    }
    assert!(
        relocations > 500 && params > 50 && collections > 200,
        "{relocations} relocations, {params} parameters, {collections} collections"
    );
}

/// [`invoke_module`]'s `inner` without GC lists: `insert_statepoints` adds
/// the reference to the invoke's list and passes the relocated version to
/// both successors as a result (`resN`).
fn invoke_module_auto(cleanup: bool) -> (Module, FuncId) {
    let mut m = Module::new("eh_auto");
    let thrower = m
        .declare_function(
            "thrower",
            &Signature::new(&[Type::I64], &[Type::I64]),
            Linkage::Import,
        )
        .unwrap();
    let alloc = m
        .declare_function(
            "alloc",
            &Signature::new(&[Type::U64], &[Type::Ref]),
            Linkage::Import,
        )
        .unwrap();
    m.set_attrs(alloc, Attrs::NOGC).unwrap();
    let f = m
        .declare_function(
            "outer",
            &Signature::new(&[Type::I64], &[Type::I64]),
            Linkage::Export,
        )
        .unwrap();
    let mut b = m.build(f).unwrap();
    let x = b.param(0).unwrap();
    let size = b.iconst(Type::U64, 8).unwrap();
    let c = b.call(Call::direct(alloc, &[size])).unwrap();
    let r = b.results(c).first().unwrap();
    let p = b
        .convert(ConvOp::RefToPtr, Policy::NONE, r, Type::Ptr)
        .unwrap();
    let k = b.iconst(Type::I64, 1000).unwrap();
    let _ = b.store(k, p, MemFlags::new(8)).unwrap();
    let ok = b.create_block(&[Type::I64]).unwrap();
    let pad = b.create_block(&[Type::Ptr]).unwrap();
    b.invoke(
        Call::direct(thrower, &[x]),
        BlockCall::with_args(ok, &[BlockArg::Result(0)]),
        BlockCall::with_args(pad, &[BlockArg::Exception]),
    )
    .unwrap();
    b.switch_to(ok).unwrap();
    let v = b.block_param(ok, 0).unwrap();
    let p = b
        .convert(ConvOp::RefToPtr, Policy::NONE, r, Type::Ptr)
        .unwrap();
    let field = b.load(Type::I64, p, MemFlags::new(8)).unwrap();
    let s = b.add(v, field, Overflow::Wrap).unwrap();
    b.ret(&[s]).unwrap();
    b.switch_to(pad).unwrap();
    let exn = b.block_param(pad, 0).unwrap();
    let p = b
        .convert(ConvOp::RefToPtr, Policy::NONE, r, Type::Ptr)
        .unwrap();
    let e = b
        .convert(ConvOp::PtrToInt, Policy::NONE, exn, Type::I64)
        .unwrap();
    let field = b.load(Type::I64, p, MemFlags::new(8)).unwrap();
    if cleanup {
        let _ = b.store(e, p, MemFlags::new(8)).unwrap();
        b.resume(exn).unwrap();
    } else {
        let s = b.add(e, field, Overflow::Wrap).unwrap();
        b.ret(&[s]).unwrap();
    }
    let stats = b.insert_statepoints().unwrap();
    assert_eq!((stats.safepoints, stats.relocations), (1, 1));
    drop(b);
    (m, f)
}

#[test]
fn test_inserted_statepoints_on_invokes_reach_both_edges() {
    for cleanup in [false, true] {
        let (m, outer) = invoke_module_auto(cleanup);
        m.validate().unwrap_or_else(|e| panic!("{e}\n{m}"));
        for x in [0u64, 1, 2, 7, 40, 41] {
            let want = reference(&m, outer, &[Val::i64(x as i64)]);
            for conv in [Conv::SysV, Conv::Win64] {
                for fp in [true, false] {
                    let c =
                        backend_lang::compile(&m, &options(conv).with_frame_pointer(fp)).unwrap();
                    let (stop, rax, _) = native(&c, conv, "outer", &[x], x);
                    match &want {
                        Outcome::Return(v) => {
                            assert_eq!(stop, Ok(Stop::Returned), "x {x} {conv:?}");
                            assert_eq!(rax, v[0].bits(), "x {x} cleanup {cleanup} {conv:?}");
                        }
                        Outcome::Unwind(p) => {
                            assert_eq!(stop, Ok(Stop::Unwound(p.bits())), "x {x} {conv:?}")
                        }
                        other => panic!("{other:?}"),
                    }
                }
            }
        }
    }
}

/// The statepoint programs on the processor: `alloc` from a native arena,
/// `collect` a no-op (a non-moving collector), so the root-slot stores and
/// reloads at every safepoint run on real hardware.
#[cfg(all(target_arch = "x86_64", any(target_os = "linux", windows)))]
fn on_hardware(p: &GcProgram, arg: i64, want: u64, seed: u64) {
    use common::native::{Native, Ran};

    fn bump(size: u64) -> u64 {
        // Leaked: a test process allocates a few kilobytes in all.
        let words = vec![0u64; (size as usize).div_ceil(8).max(1)].into_boxed_slice();
        Box::leak(words).as_mut_ptr() as u64
    }
    extern "sysv64" fn alloc_sysv(size: u64) -> u64 {
        bump(size)
    }
    extern "win64" fn alloc_win64(size: u64) -> u64 {
        bump(size)
    }
    extern "sysv64" fn collect_sysv() {}
    extern "win64" fn collect_win64() {}

    let conv = if cfg!(windows) {
        Conv::Win64
    } else {
        Conv::SysV
    };
    let (alloc, collect) = match conv {
        Conv::SysV => (
            alloc_sysv as extern "sysv64" fn(u64) -> u64 as usize as u64,
            collect_sysv as extern "sysv64" fn() as usize as u64,
        ),
        Conv::Win64 => (
            alloc_win64 as extern "win64" fn(u64) -> u64 as usize as u64,
            collect_win64 as extern "win64" fn() as usize as u64,
        ),
    };
    let c = backend_lang::compile(&p.module, &Options::host()).unwrap();
    let n = Native::load_with(&c, conv, &[("alloc", alloc), ("collect", collect)]).unwrap();
    match n.call("main", &[arg as u64]).unwrap() {
        Ran::Returned(r, _) => assert_eq!(r, want, "native seed {seed}\n{}", p.module),
        other => panic!("native seed {seed}: {other:?}"),
    }
}

#[cfg(not(all(target_arch = "x86_64", any(target_os = "linux", windows))))]
fn on_hardware(_: &GcProgram, _: i64, _: u64, _: u64) {}

#[allow(dead_code, reason = "keeps the value helpers in scope")]
fn _anchor(v: Value) -> Value {
    v
}
