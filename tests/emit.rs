//! Emission: byte-level listings of prologues, edges, and probes; whole
//! programs run in the test interpreter; random parallel copies on jump and
//! branch edges executed and checked; and every emission error.

#![allow(clippy::unwrap_used, reason = "test failures should be loud")]

mod common;
mod programs;

use std::collections::BTreeSet;

use backend_lang::mir::{Effects, Function, Mem, MirError, Operand, SlotKind, StackSlot};
use backend_lang::moves::Loc;
use backend_lang::target::{Isa, Target, TargetConfig};
use backend_lang::x86_64::{self, op, *};
use backend_lang::{PReg, RegSet};
use common::{Stop, run};
use isa_lang::x86_64::{Cond, decode_all};
use proptest::prelude::*;

fn listing(code: &[u8]) -> Vec<String> {
    decode_all(code)
        .map(|d| {
            d.map(|(_, d)| d.inst().to_string())
                .unwrap_or_else(|e| format!("(bad: {e})"))
        })
        .collect()
}

fn emit_one(f: &Function) -> Vec<u8> {
    backend_lang::emit(f).unwrap_or_else(|e| panic!("{e}\n{f}"))
}

#[test]
fn test_empty_function_with_and_without_frame_pointer() {
    let mut f = Function::new("f", &SYSV);
    let b = f.block().unwrap();
    f.push(b, op::RET, &[]).unwrap();
    assert_eq!(
        listing(&emit_one(&f)),
        ["push rbp", "mov rbp, rsp", "pop rbp", "ret"]
    );
    f.omit_frame_pointer();
    assert_eq!(listing(&emit_one(&f)), ["ret"]);
}

#[test]
fn test_red_zone_leaf_addresses_below_rsp() {
    let mut f = Function::new("leaf", &SYSV);
    f.omit_frame_pointer();
    let s = f.stack_slot(SlotKind::Local { size: 8, align: 8 }).unwrap();
    let b = f.block().unwrap();
    f.push(b, op::STORE64, &[Operand::slot(s), Operand::Imm(42)])
        .unwrap();
    f.push(b, op::MOV64, &[RAX.into(), Operand::slot(s)])
        .unwrap();
    f.push(b, op::RET, &[]).unwrap();
    assert_eq!(
        listing(&emit_one(&f)),
        [
            "mov qword ptr [rsp-0x8], 0x2a",
            "mov rax, qword ptr [rsp-0x8]",
            "ret"
        ]
    );
    // Win64 has no red zone: the same function allocates.
    let mut w = Function::new("leaf", &WIN64);
    w.omit_frame_pointer();
    let s = w.stack_slot(SlotKind::Local { size: 8, align: 8 }).unwrap();
    let b = w.block().unwrap();
    w.push(b, op::STORE64, &[Operand::slot(s), Operand::Imm(42)])
        .unwrap();
    w.push(b, op::RET, &[]).unwrap();
    assert_eq!(
        listing(&emit_one(&w)),
        [
            "sub rsp, 0x8",
            "mov qword ptr [rsp], 0x2a",
            "add rsp, 0x8",
            "ret"
        ]
    );
}

#[test]
fn test_callee_saved_registers_are_pushed_and_popped_in_reverse() {
    let mut f = Function::new("f", &SYSV);
    let b = f.block().unwrap();
    f.push(b, op::MOV64RI, &[R12.into(), Operand::Imm(1)])
        .unwrap();
    f.push(b, op::MOV64RI, &[RBX.into(), Operand::Imm(2)])
        .unwrap();
    f.push(b, op::RET, &[]).unwrap();
    assert_eq!(
        listing(&emit_one(&f)),
        [
            "push rbp",
            "mov rbp, rsp",
            "push rbx",
            "push r12",
            "mov r12d, 0x1",
            "mov ebx, 0x2",
            "pop r12",
            "pop rbx",
            "pop rbp",
            "ret"
        ]
    );
}

#[test]
fn test_mov64ri_picks_the_shortest_encoding() {
    let mut f = Function::new("f", &SYSV);
    f.omit_frame_pointer();
    let b = f.block().unwrap();
    for v in [0, 1, u32::MAX as i64, -1, 1 << 40] {
        f.push(b, op::MOV64RI, &[RAX.into(), Operand::Imm(v)])
            .unwrap();
    }
    f.push(b, op::RET, &[]).unwrap();
    assert_eq!(
        listing(&emit_one(&f)),
        [
            "mov eax, 0x0",
            "mov eax, 0x1",
            "mov eax, -0x1",
            "mov rax, -0x1",
            "mov rax, 0x10000000000",
            "ret"
        ]
    );
}

#[test]
fn test_swap_edge_is_one_xchg() {
    let mut f = Function::new("f", &SYSV);
    f.omit_frame_pointer();
    let (b0, b1) = (f.block().unwrap(), f.block().unwrap());
    f.add_param(b1, RAX.into()).unwrap();
    f.add_param(b1, RBX.into()).unwrap();
    let call = f.block_call(b1, &[RBX.into(), RAX.into()]).unwrap();
    f.push(b0, op::JMP, &[call.into()]).unwrap();
    f.push(b1, op::UD2, &[]).unwrap();
    // rbx is written (by the edge), so it is saved even without a frame
    // pointer; the jump to the next block falls through.
    assert_eq!(listing(&emit_one(&f)), ["push rbx", "xchg rax, rbx", "ud2"]);
}

#[test]
fn test_branch_edges_with_moves_get_their_own_path() {
    // Both edges carry moves: jcc on the negated condition to a stub.
    let mut f = Function::new("f", &SYSV);
    f.omit_frame_pointer();
    let (b0, b1, b2) = (f.block().unwrap(), f.block().unwrap(), f.block().unwrap());
    f.add_param(b1, RCX.into()).unwrap();
    f.add_param(b2, RDX.into()).unwrap();
    f.push(b0, op::CMP64, &[RAX.into(), Operand::Imm(0)])
        .unwrap();
    let t = f.block_call(b1, &[RAX.into()]).unwrap();
    let e = f.block_call(b2, &[RSI.into()]).unwrap();
    f.push(
        b0,
        op::JCC,
        &[x86_64::cond(Cond::E).into(), t.into(), e.into()],
    )
    .unwrap();
    f.push(b1, op::UD2, &[]).unwrap();
    f.push(b2, op::INT3, &[]).unwrap();
    f.push(b2, op::UD2, &[]).unwrap();
    assert_eq!(
        listing(&emit_one(&f)),
        [
            "cmp rax, 0x0",
            "jne $+0x7",
            "mov rcx, rax",
            "jmp $+0x7",
            "mov rdx, rsi",
            "jmp $+0x4",
            "ud2",
            "int3",
            "ud2"
        ]
    );
}

#[test]
fn test_programs_run_in_the_interpreter() {
    let code = emit_one(&programs::fib().unwrap());
    let (_, stop) = run(&code, 0, &[]);
    assert_eq!(stop, Ok(Stop::Exit(55)));

    let mut e = Emitter::new();
    let _ = e.function(&programs::fact_main().unwrap()).unwrap();
    let _ = e.function(&programs::fact().unwrap()).unwrap();
    let p = e.finish().unwrap();
    let (_, stop) = run(p.bytes(), p.symbol("main").unwrap(), &[]);
    assert_eq!(stop, Ok(Stop::Exit(120)));

    let mut e = Emitter::new();
    let _ = e.function(&programs::kitchen().unwrap()).unwrap();
    let _ = e.function(&programs::helper().unwrap()).unwrap();
    let p = e.finish().unwrap();
    let (_, stop) = run(p.bytes(), 0, &[]);
    assert_eq!(stop, Ok(Stop::Exit(200)));
}

#[test]
fn test_recursion_preserves_callee_saved_registers_for_the_caller() {
    let mut e = Emitter::new();
    let _ = e.function(&programs::fact().unwrap()).unwrap();
    let p = e.finish().unwrap();
    let (m, stop) = run(p.bytes(), 0, &[(7, 10), (3, 0xB0B), (5, 0xBEEF)]);
    assert_eq!(stop, Ok(Stop::Returned));
    assert_eq!(m.regs[0], 3_628_800);
    assert_eq!(m.regs[3], 0xB0B, "rbx preserved");
    assert_eq!(m.regs[5], 0xBEEF, "rbp preserved");
}

#[test]
fn test_stack_probes_touch_every_page_and_restore_rsp() {
    for (abi, size) in [(&WIN64, 10_000u32), (&WIN64, 100_000), (&SYSV, 64_000)] {
        let mut f = Function::new("big", abi);
        let s = f.stack_slot(SlotKind::Local { size, align: 16 }).unwrap();
        let b = f.block().unwrap();
        f.push(b, op::STORE64, &[Operand::slot(s), Operand::Imm(7)])
            .unwrap();
        f.push(b, op::RET, &[]).unwrap();
        let code = emit_one(&f);
        let text = listing(&code);
        assert!(
            text.iter().any(|l| l == "test qword ptr [rsp], rsp"),
            "{text:?}"
        );
        if size >= 5 * 4096 {
            assert!(text.iter().any(|l| l.starts_with("dec r11d")), "{text:?}");
        }
        let (m, stop) = run(&code, 0, &[]);
        assert_eq!(stop, Ok(Stop::Returned), "{text:?}");
        assert_eq!(m.rsp(), common::STACK_BASE + common::STACK_SIZE as u64 - 64);
    }
}

#[test]
fn test_function_address_and_self_call_in_one_function() {
    // A function may call itself through `emit`; the call resolves to its
    // own entry.
    let code = emit_one(&programs::fact().unwrap());
    let (m, stop) = run(&code, 0, &[(7, 6)]);
    assert_eq!(stop, Ok(Stop::Returned));
    assert_eq!(m.regs[0], 720);
}

/// Registers an edge test may use: the allocatable GPRs.
fn regs() -> Vec<PReg> {
    TARGET.class(GPR).allocatable().iter().collect()
}

fn init_value(l: Loc) -> i64 {
    match l {
        Loc::Reg(r) => 0x1000 + r.index() as i64,
        // Some slot values need a 64-bit immediate, which a store cannot take.
        Loc::Slot(s) if s.index() % 3 == 0 => (1 << 40) + s.index() as i64,
        Loc::Slot(s) => 0x2000 + s.index() as i64,
    }
}

fn operand(l: Loc) -> Operand {
    match l {
        Loc::Reg(r) => r.into(),
        Loc::Slot(s) => Operand::slot(s),
    }
}

/// One edge's parallel copy: destinations, sources (`None` for an
/// immediate), and the immediates.
type Edge = (Vec<Loc>, Vec<Option<Loc>>, Vec<i64>);

/// A random parallel copy: destinations distinct, sources any location or
/// an immediate.
fn edge_strategy(slots: usize) -> impl Strategy<Value = Edge> {
    let mut locs: Vec<Loc> = regs().into_iter().map(Loc::Reg).collect();
    locs.extend((0..slots as u32).map(|s| Loc::Slot(StackSlot::new(s))));
    let n = locs.len();
    (
        Just(locs.clone()).prop_shuffle(),
        proptest::collection::vec(proptest::option::weighted(0.9, 0..n), 0..=n),
        proptest::collection::vec(any::<i64>(), n),
    )
        .prop_map(move |(dsts, srcs, imms)| {
            let srcs: Vec<Option<Loc>> = srcs.into_iter().map(|s| s.map(|i| locs[i])).collect();
            let dsts = dsts[..srcs.len()].to_vec();
            (dsts, srcs, imms)
        })
}

/// Builds `b0: init everything; <terminator>; targets: ud2` and checks the
/// copy on the taken edge executed exactly.
fn run_edge(
    abi: &'static backend_lang::target::abi::Abi,
    slots: usize,
    edges: &[Edge],
    taken: Option<bool>,
) {
    let mut f = Function::new("edge", abi);
    for _ in 0..slots {
        let _ = f.stack_slot(SlotKind::Local { size: 8, align: 8 }).unwrap();
    }
    let b0 = f.block().unwrap();
    let targets: Vec<_> = edges.iter().map(|_| f.block().unwrap()).collect();
    let mut all: Vec<Loc> = regs().into_iter().map(Loc::Reg).collect();
    all.extend((0..slots as u32).map(|s| Loc::Slot(StackSlot::new(s))));
    for &l in &all {
        f.push(b0, op::COPY, &[operand(l), Operand::Imm(init_value(l))])
            .unwrap();
    }
    let mut calls = Vec::new();
    for (k, (dsts, srcs, imms)) in edges.iter().enumerate() {
        for &d in dsts {
            f.add_param(targets[k], operand(d)).unwrap();
        }
        let args: Vec<Operand> = srcs
            .iter()
            .zip(imms)
            .map(|(s, &v)| s.map_or(Operand::Imm(v), operand))
            .collect();
        calls.push(f.block_call(targets[k], &args).unwrap());
        f.push(targets[k], op::UD2, &[]).unwrap();
    }
    match taken {
        None => {
            f.push(b0, op::JMP, &[calls[0].into()]).unwrap();
        }
        Some(t) => {
            // rax == rax, so `e` is taken and `ne` is not.
            f.push(b0, op::CMP64, &[RAX.into(), RAX.into()]).unwrap();
            let c = if t { Cond::E } else { Cond::Ne };
            f.push(
                b0,
                op::JCC,
                &[x86_64::cond(c).into(), calls[0].into(), calls[1].into()],
            )
            .unwrap();
        }
    }
    let mut e = Emitter::new();
    let info = e.function(&f).unwrap_or_else(|err| panic!("{err}\n{f}"));
    let p = e.finish().unwrap();
    let (m, stop) = run(p.bytes(), 0, &[]);
    let Ok(Stop::Ud2(_)) = stop else {
        panic!("{stop:?}\n{f}\n{:#?}", listing(p.bytes()));
    };
    let edge = &edges[usize::from(taken == Some(false))];
    let value = |l: Loc| -> i64 {
        match l {
            Loc::Reg(r) => m.regs[r.index()] as i64,
            Loc::Slot(s) => {
                let off = info.frame().slot_offset(s).unwrap();
                m.read((m.rsp() as i64 + i64::from(off)) as u64, 8).unwrap() as i64
            }
        }
    };
    let (dsts, srcs, imms) = edge;
    let mut written = BTreeSet::new();
    for ((&d, s), &v) in dsts.iter().zip(srcs).zip(imms) {
        let want = s.map_or(v, init_value);
        assert_eq!(value(d), want, "{d:?}\n{f}\n{:#?}", listing(p.bytes()));
        let _ = written.insert(d);
    }
    for &l in &all {
        if !written.contains(&l) {
            assert_eq!(
                value(l),
                init_value(l),
                "{l:?} changed\n{f}\n{:#?}",
                listing(p.bytes())
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn jump_edges_perform_their_parallel_copy(edge in edge_strategy(6), win in any::<bool>()) {
        let abi = if win { &WIN64 } else { &SYSV };
        run_edge(abi, 6, &[edge], None);
    }

    #[test]
    fn branch_edges_perform_the_taken_copy(
        a in edge_strategy(4),
        b in edge_strategy(4),
        taken in any::<bool>(),
        empty in 0..4usize,
    ) {
        // Cover all four shapes: both edges with moves, one empty, both empty.
        let a = if empty & 1 == 1 { (vec![], vec![], vec![]) } else { a };
        let b = if empty & 2 == 2 { (vec![], vec![], vec![]) } else { b };
        run_edge(&SYSV, 4, &[a, b], Some(taken));
    }
}

#[test]
fn test_copy_and_pcopy_through_memory() {
    let mut f = Function::new("f", &SYSV);
    let s: Vec<StackSlot> = (0..3)
        .map(|_| f.stack_slot(SlotKind::Local { size: 8, align: 8 }).unwrap())
        .collect();
    let b = f.block().unwrap();
    f.push(b, op::COPY, &[Operand::slot(s[0]), Operand::Imm(1)])
        .unwrap();
    f.push(b, op::COPY, &[Operand::slot(s[1]), Operand::Imm(2)])
        .unwrap();
    f.push(b, op::COPY, &[Operand::slot(s[2]), Operand::slot(s[0])])
        .unwrap();
    // Rotate the three slots in parallel.
    f.push(
        b,
        op::PCOPY,
        &[
            Operand::slot(s[0]),
            Operand::slot(s[1]),
            Operand::slot(s[2]),
            Operand::slot(s[1]),
            Operand::slot(s[2]),
            Operand::slot(s[0]),
        ],
    )
    .unwrap();
    f.push(b, op::MOV64, &[RAX.into(), Operand::slot(s[0])])
        .unwrap();
    f.push(b, op::MOV64, &[RCX.into(), Operand::slot(s[1])])
        .unwrap();
    f.push(b, op::MOV64, &[RDX.into(), Operand::slot(s[2])])
        .unwrap();
    f.push(b, op::RET, &[]).unwrap();
    let init: Vec<(usize, u64)> = (0..16)
        .filter(|&r| r != 4 && r != 5)
        .map(|r| (r, 0x100 + r as u64))
        .collect();
    let (m, stop) = run(&emit_one(&f), 0, &init);
    assert_eq!(stop, Ok(Stop::Returned));
    assert_eq!((m.regs[0], m.regs[1], m.regs[2]), (2, 1, 1));
    // Slot-to-slot moves borrow a register (rax) around each move and
    // restore it; nothing else changes.
    for &(r, v) in &init {
        if r > 2 {
            assert_eq!(m.regs[r], v, "register {r}");
        }
    }
}

#[test]
fn test_errors() {
    // Virtual registers.
    let mut f = Function::new("f", &SYSV);
    let b = f.block().unwrap();
    let v = f.vreg(GPR).unwrap();
    f.add_param(b, v.into()).unwrap();
    f.push(b, op::RET, &[]).unwrap();
    assert_eq!(
        backend_lang::emit(&f),
        Err(EmitError::VirtualRegister {
            block: b,
            inst: None
        })
    );

    // Tied mismatch.
    let mut f = Function::new("f", &SYSV);
    let b = f.block().unwrap();
    let i = f
        .push(b, op::ADD64, &[RAX.into(), RCX.into(), Operand::Imm(1)])
        .unwrap();
    f.push(b, op::RET, &[]).unwrap();
    assert_eq!(
        backend_lang::emit(&f),
        Err(EmitError::TiedMismatch { inst: i })
    );

    // Writing the stack pointer, the kept frame pointer, or clobbering them.
    let mut f = Function::new("f", &SYSV);
    let b = f.block().unwrap();
    let i = f
        .push(b, op::MOV64RI, &[RSP.into(), Operand::Imm(0)])
        .unwrap();
    f.push(b, op::RET, &[]).unwrap();
    assert_eq!(
        backend_lang::emit(&f),
        Err(EmitError::ReservedWrite {
            block: b,
            inst: Some(i)
        })
    );
    let mut f = Function::new("f", &SYSV);
    let b = f.block().unwrap();
    let i = f
        .push_with(b, op::NOP, &[], Effects::new().clobbers(RegSet::of(&[RBP])))
        .unwrap();
    f.push(b, op::RET, &[]).unwrap();
    assert_eq!(
        backend_lang::emit(&f),
        Err(EmitError::ReservedWrite {
            block: b,
            inst: Some(i)
        })
    );
    // Without a frame pointer, rbp is an ordinary (callee-saved) register.
    f.omit_frame_pointer();
    assert!(backend_lang::emit(&f).is_ok());
    let mut f = Function::new("f", &SYSV);
    let b = f.block().unwrap();
    f.add_param(b, RBP.into()).unwrap();
    f.push(b, op::RET, &[]).unwrap();
    assert_eq!(
        backend_lang::emit(&f),
        Err(EmitError::ReservedWrite {
            block: b,
            inst: None
        })
    );

    // A vector move, and a Win64 prologue that must save xmm6.
    let mut f = Function::new("f", &SYSV);
    let b = f.block().unwrap();
    let i = f.push(b, op::COPY, &[XMM0.into(), XMM1.into()]).unwrap();
    f.push(b, op::RET, &[]).unwrap();
    assert_eq!(
        backend_lang::emit(&f),
        Err(EmitError::Unencodable { inst: None }),
        "{i}"
    );
    let mut f = Function::new("f", &WIN64);
    let b = f.block().unwrap();
    f.push(b, op::COPY, &[XMM6.into(), XMM1.into()]).unwrap();
    f.push(b, op::RET, &[]).unwrap();
    assert_eq!(
        backend_lang::emit(&f),
        Err(EmitError::Unencodable { inst: None })
    );

    // Malformed MIR and frames.
    let f = Function::new("f", &SYSV);
    assert_eq!(
        backend_lang::emit(&f),
        Err(EmitError::Mir(MirError::NoBlocks))
    );
    let mut f = Function::new("f", &SYSV);
    let _ = f
        .stack_slot(SlotKind::Local { size: 8, align: 32 })
        .unwrap();
    let b = f.block().unwrap();
    f.push(b, op::RET, &[]).unwrap();
    assert!(matches!(backend_lang::emit(&f), Err(EmitError::Frame(_))));

    // Symbols.
    let mut e = Emitter::new();
    let _ = e.function(&programs::helper().unwrap()).unwrap();
    assert_eq!(
        e.function(&programs::helper().unwrap()),
        Err(EmitError::DuplicateSymbol("helper".into()))
    );
    let mut e = Emitter::new();
    let _ = e.function(&programs::kitchen().unwrap()).unwrap();
    assert_eq!(e.finish(), Err(EmitError::UndefinedSymbol("helper".into())));

    // A probe loop with r10 and r11 both carrying entry values.
    let mut f = Function::new("f", &WIN64);
    let _ = f
        .stack_slot(SlotKind::Local {
            size: 100_000,
            align: 8,
        })
        .unwrap();
    let b = f.block().unwrap();
    f.add_param(b, R10.into()).unwrap();
    f.add_param(b, R11.into()).unwrap();
    f.push(b, op::RET, &[]).unwrap();
    assert_eq!(backend_lang::emit(&f), Err(EmitError::ProbeScratch));

    // A target this emitter does not encode.
    static INSTS: [backend_lang::target::InstDesc; 2] = Target::generic_insts();
    static OTHER: Target = Target::new(
        "other",
        Isa::X86_64,
        x86_64::TARGET.regs(),
        x86_64::TARGET.classes(),
        &INSTS,
        TargetConfig::of(&x86_64::TARGET),
    );
    static OTHER_ABI: backend_lang::target::abi::Abi = backend_lang::target::abi::Abi::new(
        "other",
        &OTHER,
        *SYSV.args(),
        *SYSV.rets(),
        RegSet::EMPTY,
        *SYSV.stack(),
    );
    let mut f = Function::new("f", &OTHER_ABI);
    let b = f.block().unwrap();
    f.push(
        b,
        backend_lang::target::Opcode::COPY,
        &[RAX.into(), RCX.into()],
    )
    .unwrap();
    assert_eq!(backend_lang::emit(&f), Err(EmitError::UnsupportedTarget));

    // Every error displays something actionable.
    for e in [
        EmitError::UnsupportedTarget,
        EmitError::ProbeScratch,
        EmitError::Unencodable { inst: None },
        EmitError::UndefinedSymbol("x".into()),
    ] {
        assert!(!e.to_string().is_empty());
    }
}

#[test]
fn test_memory_operands_encode_with_every_shape() {
    let mut f = Function::new("f", &SYSV);
    f.omit_frame_pointer();
    let b = f.block().unwrap();
    let shapes = [
        Mem::base(RBX.into()),
        Mem::base(R13.into()),
        Mem::base(RSP.into()).with_disp(8),
        Mem::base(RAX.into())
            .with_index(R12.into(), backend_lang::mir::Scale::X2)
            .with_disp(-300),
        Mem::absolute(0x1000),
    ];
    for m in shapes {
        f.push(b, op::MOV64, &[RCX.into(), m.into()]).unwrap();
    }
    f.push(b, op::RET, &[]).unwrap();
    assert_eq!(
        listing(&emit_one(&f)),
        [
            "mov rcx, qword ptr [rbx]",
            "mov rcx, qword ptr [r13]",
            "mov rcx, qword ptr [rsp+0x8]",
            "mov rcx, qword ptr [rax+r12*2-0x12c]",
            "mov rcx, qword ptr [0x1000]",
            "ret"
        ]
    );
}
