//! MIR construction, verification, and the text form; frame layout
//! invariants over random slots; and hostile construction (random opcodes
//! and operands never panic the builder, the verifier, or the emitter).

#![allow(clippy::unwrap_used, reason = "test failures should be loud")]

use backend_lang::mir::{
    Block, BlockCall, CondCode, Effects, Function, Mem, MirError, Operand, Scale, SlotKind,
    StackSlot, Symbol,
};
use backend_lang::target::Opcode;
use backend_lang::target::frame::FrameStep;
use backend_lang::x86_64::{self, op, *};
use backend_lang::{PReg, RegClass, RegSet};
use isa_lang::x86_64::Cond;
use proptest::prelude::*;

fn sum() -> Function {
    let mut f = Function::new("sum", &SYSV);
    let (entry, head, exit) = (f.block().unwrap(), f.block().unwrap(), f.block().unwrap());
    let n = f.vreg(GPR).unwrap();
    f.add_param(entry, n.into()).unwrap();
    let (acc, i) = (f.vreg(GPR).unwrap(), f.vreg(GPR).unwrap());
    f.add_param(head, acc.into()).unwrap();
    f.add_param(head, i.into()).unwrap();
    let result = f.vreg(GPR).unwrap();
    f.add_param(exit, result.into()).unwrap();
    let zero = f.vreg(GPR).unwrap();
    f.push(entry, op::MOV64RI, &[zero.into(), Operand::Imm(0)])
        .unwrap();
    let c = f.block_call(head, &[zero.into(), n.into()]).unwrap();
    f.push(entry, op::JMP, &[c.into()]).unwrap();
    let (acc2, i2) = (f.vreg(GPR).unwrap(), f.vreg(GPR).unwrap());
    f.push(head, op::ADD64, &[acc2.into(), acc.into(), i.into()])
        .unwrap();
    f.push(head, op::SUB64, &[i2.into(), i.into(), Operand::Imm(1)])
        .unwrap();
    let again = f.block_call(head, &[acc2.into(), i2.into()]).unwrap();
    let done = f.block_call(exit, &[acc2.into()]).unwrap();
    f.push(
        head,
        op::JCC,
        &[x86_64::cond(Cond::Ne).into(), again.into(), done.into()],
    )
    .unwrap();
    f.push(exit, op::COPY, &[RAX.into(), result.into()])
        .unwrap();
    f.push_with(exit, op::RET, &[], Effects::new().uses(&[RAX]))
        .unwrap();
    f
}

#[test]
fn test_text_form_is_exact() {
    let f = sum();
    f.verify().unwrap();
    assert_eq!(
        f.to_string(),
        "function sum (sysv64) {\n\
         block0(%0:gpr):\n\
         \x20   %4:gpr = mov64ri 0\n\
         \x20   jmp block1(%4, %0)\n\
         block1(%1:gpr, %2:gpr):\n\
         \x20   %5:gpr = add64 %1, %2\n\
         \x20   %6:gpr = sub64 %2, 1\n\
         \x20   jcc ne, block1(%5, %6), block2(%5)\n\
         block2(%3:gpr):\n\
         \x20   rax = copy %3\n\
         \x20   ret implicit(rax)\n\
         }\n"
    );
}

#[test]
fn test_text_form_of_every_operand_shape() {
    let mut f = Function::new("shapes", &WIN64);
    f.omit_frame_pointer();
    f.reserve_outgoing(48);
    let s = f
        .stack_slot(SlotKind::Local { size: 16, align: 8 })
        .unwrap();
    let a = f
        .stack_slot(SlotKind::Incoming {
            offset: 40,
            size: 8,
        })
        .unwrap();
    let g = f.symbol("g").unwrap();
    let b = f.block().unwrap();
    f.add_param(b, Operand::slot(a)).unwrap();
    f.push(
        b,
        op::MOV64,
        &[
            RAX.into(),
            Mem::base(RBX.into())
                .with_index(RCX.into(), Scale::X8)
                .with_disp(-16)
                .into(),
        ],
    )
    .unwrap();
    f.push(b, op::MOV64, &[RAX.into(), Mem::absolute(64).into()])
        .unwrap();
    f.push(
        b,
        op::STORE32,
        &[Operand::Slot { slot: s, offset: 4 }, Operand::Imm(-7)],
    )
    .unwrap();
    f.push(b, op::LEA64, &[RDX.into(), g.into()]).unwrap();
    f.push(
        b,
        op::PCOPY,
        &[RAX.into(), Operand::slot(s), RDX.into(), Operand::Imm(3)],
    )
    .unwrap();
    f.push_with(
        b,
        op::CALL,
        &[g.into()],
        Effects::new()
            .uses(&[RCX])
            .defs(&[RAX])
            .clobbers(RegSet::of(&[R10, R11])),
    )
    .unwrap();
    f.push(b, op::UD2, &[]).unwrap();
    f.verify().unwrap();
    assert_eq!(
        f.to_string(),
        "function shapes (win64) {\n\
         \x20   omit-frame-pointer\n\
         \x20   outgoing 48\n\
         \x20   ss0 = local 16, align 8\n\
         \x20   ss1 = incoming 40, size 8\n\
         \x20   sym0 = @g\n\
         block0(ss1):\n\
         \x20   rax = mov64 [rbx + rcx*8 - 16]\n\
         \x20   rax = mov64 [64]\n\
         \x20   store32 ss0+4, -7\n\
         \x20   rdx = lea64 @g\n\
         \x20   rax, ss0 = pcopy rdx, 3\n\
         \x20   call @g implicit(rcx) implicit-def(rax) clobber(r10, r11)\n\
         \x20   ud2\n\
         }\n"
    );
}

#[test]
fn test_builder_rejects_bad_operands() {
    let mut f = Function::new("f", &SYSV);
    let b = f.block().unwrap();
    let other = f.block().unwrap();
    let v = f.vreg(GPR).unwrap();
    let x = f.vreg(VEC).unwrap();

    // Counts, shapes, classes, ranges.
    assert!(matches!(
        f.push(b, op::ADD64, &[v.into()]),
        Err(MirError::OperandCount {
            expected: 3,
            found: 1,
            ..
        })
    ));
    assert!(matches!(
        f.push(b, op::ADD64, &[Operand::Imm(1), v.into(), v.into()]),
        Err(MirError::OperandShape { index: 0, .. })
    ));
    assert!(matches!(
        f.push(b, op::ADD64, &[v.into(), x.into(), v.into()]),
        Err(MirError::RegClass { index: 1, .. })
    ));
    assert!(matches!(
        f.push(b, op::SHL64, &[v.into(), v.into(), Operand::Imm(256)]),
        Err(MirError::ImmediateRange { value: 256, .. })
    ));
    assert!(matches!(
        f.push(
            b,
            op::STORE8,
            &[Mem::base(RAX.into()).into(), Operand::Imm(128)]
        ),
        Err(MirError::ImmediateRange { .. })
    ));
    let m = Mem::base(RAX.into());
    assert!(matches!(
        f.push(b, op::CMP64, &[m.into(), m.into()]),
        Err(MirError::TooManyMemOperands { .. })
    ));
    assert!(matches!(
        f.push(b, op::MOV64, &[v.into(), Mem::base(x).into()]),
        Err(MirError::RegClass { .. })
    ));
    assert!(matches!(
        f.push(b, op::SETCC, &[v.into(), CondCode::new(16).into()]),
        Err(MirError::OperandShape { .. })
    ));
    assert!(matches!(
        f.push(b, Opcode::new(9999), &[]),
        Err(MirError::UnknownOpcode { .. })
    ));

    // Unknown entities.
    let ghost = backend_lang::mir::Function::new("g", &SYSV);
    let _ = ghost;
    assert_eq!(
        f.push(Block::new(9), op::RET, &[]),
        Err(MirError::UnknownBlock(Block::new(9)))
    );
    assert_eq!(
        f.push(
            b,
            op::MOV64,
            &[RAX.into(), Operand::slot(StackSlot::new(3))]
        ),
        Err(MirError::UnknownSlot(StackSlot::new(3)))
    );
    assert_eq!(
        f.push(b, op::CALL, &[Symbol::new(5).into()]),
        Err(MirError::UnknownSymbol(Symbol::new(5)))
    );
    assert!(matches!(
        f.push(b, op::MOV64, &[PReg::new(100).unwrap().into(), RAX.into()]),
        Err(MirError::UnknownPReg(_))
    ));
    assert_eq!(
        f.vreg(RegClass::new(3)),
        Err(MirError::UnknownClass(RegClass::new(3)))
    );
    assert_eq!(
        f.spill_slot(RegClass::new(3)),
        Err(MirError::UnknownClass(RegClass::new(3)))
    );

    // Params and branch arguments.
    assert_eq!(
        f.add_param(other, Operand::Imm(0)),
        Err(MirError::BadParam { block: other })
    );
    assert_eq!(
        f.add_param(
            other,
            Operand::Slot {
                slot: StackSlot::new(0),
                offset: 4
            }
        ),
        Err(MirError::BadParam { block: other })
    );
    assert_eq!(
        f.add_param(Block::new(7), v.into()),
        Err(MirError::UnknownBlock(Block::new(7)))
    );
    assert_eq!(
        f.block_call(other, &[Mem::base(RAX.into()).into()]),
        Err(MirError::BadArg { block: other })
    );
    f.add_param(other, v.into()).unwrap();
    let none = f.block_call(other, &[]).unwrap();
    assert!(matches!(
        f.push(b, op::JMP, &[none.into()]),
        Err(MirError::ArgCount {
            expected: 1,
            found: 0,
            ..
        })
    ));
    let wrong = f.block_call(other, &[x.into()]).unwrap();
    assert!(matches!(
        f.push(b, op::JMP, &[wrong.into()]),
        Err(MirError::ArgClass { index: 0, .. })
    ));

    // Copies.
    assert!(matches!(
        f.push(b, op::COPY, &[v.into()]),
        Err(MirError::OperandCount { .. })
    ));
    assert!(matches!(
        f.push(b, op::COPY, &[Operand::Imm(1), v.into()]),
        Err(MirError::OperandShape { index: 0, .. })
    ));
    assert!(matches!(
        f.push(b, op::COPY, &[v.into(), x.into()]),
        Err(MirError::RegClass { index: 1, .. })
    ));
    assert!(matches!(
        f.push(b, op::PCOPY, &[v.into(), v.into(), RAX.into()]),
        Err(MirError::OperandCount { .. })
    ));
    assert!(matches!(
        f.push(
            b,
            op::PCOPY,
            &[RAX.into(), RAX.into(), Operand::Imm(1), Operand::Imm(2)]
        ),
        Err(MirError::DuplicateDestination { .. })
    ));
    // Every error message names something.
    assert!(MirError::NoBlocks.to_string().contains("no blocks"));
    assert!(MirError::Limit.to_string().contains("32-bit"));
}

#[test]
fn test_verify_structure() {
    let f = Function::new("f", &SYSV);
    assert_eq!(f.verify(), Err(MirError::NoBlocks));

    let mut f = Function::new("f", &SYSV);
    let b = f.block().unwrap();
    assert_eq!(f.verify(), Err(MirError::MissingTerminator { block: b }));
    let r = f.push(b, op::RET, &[]).unwrap();
    f.push(b, op::NOP, &[]).unwrap();
    assert_eq!(f.verify(), Err(MirError::MisplacedTerminator { inst: r }));

    // A parameter added after the branch to it is caught by verify.
    let mut f = Function::new("f", &SYSV);
    let (b0, b1) = (f.block().unwrap(), f.block().unwrap());
    let c = f.block_call(b1, &[]).unwrap();
    let j = f.push(b0, op::JMP, &[c.into()]).unwrap();
    f.push(b1, op::RET, &[]).unwrap();
    f.verify().unwrap();
    f.add_param(b1, RAX.into()).unwrap();
    assert_eq!(
        f.verify(),
        Err(MirError::ArgCount {
            inst: j,
            block: b1,
            expected: 1,
            found: 0
        })
    );

    // SSA: a parameter and an instruction defining one register.
    let mut f = Function::new("f", &SYSV);
    let b = f.block().unwrap();
    let v = f.vreg(GPR).unwrap();
    f.add_param(b, v.into()).unwrap();
    f.push(b, op::COPY, &[v.into(), Operand::Imm(1)]).unwrap();
    f.push(b, op::RET, &[]).unwrap();
    assert_eq!(
        f.verify(),
        Err(MirError::MultipleDefs {
            vreg: v.as_virt().unwrap()
        })
    );
}

#[test]
fn test_accessors_on_foreign_ids_are_empty() {
    let f = sum();
    let i = backend_lang::mir::Inst::new(9999);
    assert_eq!(f.opcode(i), None);
    assert!(f.operands(i).is_empty());
    assert!(f.implicit(i).is_empty());
    assert!(f.clobbers(i).is_empty());
    assert_eq!(f.inst_block(i), None);
    assert!(f.insts(Block::new(99)).is_empty());
    assert!(f.params(Block::new(99)).is_empty());
    assert_eq!(f.symbol_name(Symbol::new(1)), None);
    // A branch target made by another function names nothing here.
    let mut g = Function::new("g", &SYSV);
    let b = g.block().unwrap();
    let foreign: BlockCall = g
        .block_call(b, &[Operand::Imm(1), Operand::Imm(2)])
        .unwrap();
    let empty = Function::new("e", &SYSV);
    assert!(empty.args(foreign).is_empty());
}

#[test]
fn test_written_regs_and_calls_drive_the_frame() {
    let mut f = Function::new("f", &SYSV);
    let b = f.block().unwrap();
    let g = f.symbol("g").unwrap();
    f.push(b, op::MOV64RI, &[R14.into(), Operand::Imm(1)])
        .unwrap();
    f.push_with(
        b,
        op::CALL,
        &[g.into()],
        Effects::new().clobbers(SYSV.call_clobbers()),
    )
    .unwrap();
    f.push(b, op::RET, &[]).unwrap();
    assert!(f.has_calls());
    let frame = f.frame_layout().unwrap();
    assert_eq!(frame.pushed(), [R14]);
    // ret + rbp + r14 = 24 bytes: 8 more to align the call.
    assert_eq!(
        frame.prologue().last(),
        Some(&FrameStep::Alloc { bytes: 8, probe: 0 })
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// Random locals, incoming slots, written registers, calls, and outgoing
    /// areas: slots are aligned, disjoint, inside the frame, and the stack
    /// is aligned at calls.
    #[test]
    fn frame_layouts_hold_their_invariants(
        locals in proptest::collection::vec((0u32..300, 0u32..5), 0..12),
        incoming in proptest::collection::vec(0u32..8, 0..4),
        written in proptest::collection::vec(0usize..32, 0..10),
        calls in any::<bool>(),
        outgoing in 0u32..200,
        fp in any::<bool>(),
        win in any::<bool>(),
    ) {
        let abi = if win { &WIN64 } else { &SYSV };
        let mut f = Function::new("f", abi);
        if !fp {
            f.omit_frame_pointer();
        }
        f.reserve_outgoing(outgoing);
        let mut ids = Vec::new();
        for &(size, a) in &locals {
            ids.push((f.stack_slot(SlotKind::Local { size, align: 1 << a }).unwrap(), size, 1u32 << a));
        }
        let mut inc = Vec::new();
        for (k, _) in incoming.iter().enumerate() {
            inc.push(f.stack_slot(SlotKind::Incoming { offset: 8 * k as u32, size: 8 }).unwrap());
        }
        let b = f.block().unwrap();
        let mut set = RegSet::EMPTY;
        for &r in &written {
            let p = PReg::new(r).unwrap();
            if p != RSP && (p != RBP || !fp) {
                set = set.with(p);
            }
        }
        f.push_with(b, op::NOP, &[], Effects::new().clobbers(set)).unwrap();
        if calls {
            let g = f.symbol("g").unwrap();
            f.push(b, op::CALL, &[g.into()]).unwrap();
        }
        f.push(b, op::RET, &[]).unwrap();
        let frame = f.frame_layout().unwrap();

        let cfa: i64 = 1 << 20;
        let mut sp = cfa - 8;
        for step in frame.prologue() {
            match *step {
                FrameStep::Push(_) => sp -= 8,
                FrameStep::Alloc { bytes, .. } => sp -= i64::from(bytes),
                _ => {}
            }
        }
        prop_assert_eq!(cfa - sp, i64::from(frame.cfa_offset()));
        if calls {
            prop_assert_eq!(sp % 16, 0);
        }
        let pushes_bottom = cfa - 8 - 8 * (frame.pushed().len() as i64 + i64::from(frame.frame_pointer()));
        let mut spans = Vec::new();
        for &(s, size, align) in &ids {
            let at = sp + i64::from(frame.slot_offset(s).unwrap());
            prop_assert_eq!(at % i64::from(align), 0);
            prop_assert!(at + i64::from(size) <= pushes_bottom);
            if frame.uses_red_zone() {
                prop_assert!(at >= sp - i64::from(abi.stack().red_zone()));
            } else {
                // Clear of the outgoing area (and the Win64 shadow space).
                let out = if calls { i64::from(outgoing.max(abi.stack().shadow_space())) } else { i64::from(outgoing) };
                prop_assert!(at >= sp + out);
            }
            spans.push((at, at + i64::from(size)));
        }
        for (save_reg, off) in frame.vec_saves() {
            let at = sp + i64::from(*off);
            prop_assert_eq!(at % 16, 0, "{:?}", save_reg);
            spans.push((at, at + 16));
        }
        spans.sort_unstable();
        for w in spans.windows(2) {
            prop_assert!(w[0].1 <= w[1].0);
        }
        for (k, &s) in inc.iter().enumerate() {
            prop_assert_eq!(sp + i64::from(frame.slot_offset(s).unwrap()), cfa + 8 * k as i64);
        }
        // Callee-saved registers written are saved; nothing else is.
        let saved: RegSet = frame.pushed().iter().copied().chain(frame.vec_saves().iter().map(|&(r, _)| r)).collect();
        let mut expect = set.intersect(abi.callee_saved());
        if fp {
            expect = expect.without(RBP);
        }
        prop_assert_eq!(saved, expect);
    }

    /// Random opcodes (valid or not) with random operands (valid, foreign, or
    /// ill-shaped): building, verifying, printing, laying out, and emitting
    /// never panic.
    #[test]
    fn hostile_construction_never_panics(
        insts in proptest::collection::vec((0u16..110, proptest::collection::vec(0usize..24, 0..5)), 0..30),
        params in proptest::collection::vec(0usize..24, 0..4),
    ) {
        let mut f = Function::new("h", &SYSV);
        let _ = f.stack_slot(SlotKind::Local { size: 8, align: 8 });
        let b0 = f.block().unwrap();
        let b1 = f.block().unwrap();
        let v = f.vreg(GPR).unwrap();
        let x = f.vreg(VEC).unwrap();
        let g = f.symbol("h").unwrap();
        let call = f.block_call(b1, &[]).unwrap();
        let pool = |k: usize| -> Operand {
            match k {
                0 => RAX.into(),
                1 => RCX.into(),
                2 => v.into(),
                3 => x.into(),
                4 => XMM3.into(),
                5 => Operand::Imm(0),
                6 => Operand::Imm(i64::MIN),
                7 => Operand::Imm(255),
                8 => Mem::base(RBX.into()).into(),
                9 => Mem::base(v).with_index(RCX.into(), Scale::X2).into(),
                10 => Operand::slot(StackSlot::new(0)),
                11 => Operand::slot(StackSlot::new(7)),
                12 => g.into(),
                13 => Symbol::new(9).into(),
                14 => x86_64::cond(Cond::E).into(),
                15 => CondCode::new(200).into(),
                16 => call.into(),
                17 => RSP.into(),
                18 => RBP.into(),
                19 => PReg::new(120).unwrap().into(),
                20 => Mem::absolute(-4).into(),
                21 => FLAGS.into(),
                22 => R15.into(),
                _ => Operand::Slot { slot: StackSlot::new(0), offset: i32::MAX },
            }
        };
        for p in params {
            let _ = f.add_param(b1, pool(p));
        }
        for (opc, ops) in insts {
            let ops: Vec<Operand> = ops.into_iter().map(pool).collect();
            let _ = f.push(b0, Opcode::new(opc), &ops);
        }
        let _ = f.push(b0, op::UD2, &[]);
        let _ = f.push(b1, op::RET, &[]);
        let _ = f.verify();
        let _ = f.to_string();
        let _ = f.frame_layout();
        let _ = backend_lang::emit(&f);
    }
}
