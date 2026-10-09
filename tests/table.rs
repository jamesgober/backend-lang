//! The x86-64 instruction table against isa-lang: every MIR opcode encodes
//! for every operand shape its rules accept, decodes back to the mnemonic it
//! names, and declares the same flag effects and fixed registers isa-lang's
//! description has.

#![allow(clippy::unwrap_used, reason = "test failures should be loud")]

use backend_lang::mir::{Block, Function, Mem, Operand, Scale, SlotKind};
use backend_lang::target::{ImmRange, InstKind, Opcode, OperandRule};
use backend_lang::x86_64::{self, Emitter, FLAGS, GPR, R9, R10, R12, RBX, RCX, SYSV, TARGET, op};
use backend_lang::{PReg, Reg};
use isa_lang::x86_64::{Cond, Decoded, Flags, OperandKind, decode_all};

/// Representative immediates for a range: both ends and small values.
fn imms(r: ImmRange) -> Vec<i64> {
    match r {
        ImmRange::I8 => vec![-128, 0, 1, 127],
        ImmRange::U8 => vec![0, 1, 31, 255],
        ImmRange::I16 => vec![-32768, -1, 32767],
        ImmRange::I32 => vec![i64::from(i32::MIN), -1, 0, 127, 128, i64::from(i32::MAX)],
        ImmRange::I64 => vec![i64::MIN, -1, 0, i64::from(u32::MAX), i64::MAX],
        _ => vec![],
    }
}

/// Every operand list the rules accept, over representative values, with at
/// most one memory operand.
fn shapes(f: &mut Function, rules: &[OperandRule], target: Block) -> Vec<Vec<Operand>> {
    let slot = backend_lang::mir::StackSlot::new(0);
    let helper = f.symbol("helper").unwrap();
    let call = f.block_call(target, &[]).unwrap();
    let mut out: Vec<Vec<Operand>> = vec![vec![]];
    for (i, rule) in rules.iter().enumerate() {
        let mut next = Vec::new();
        for prefix in &out {
            let options: Vec<Operand> = match *rule {
                OperandRule::Def(_) => vec![R9.into(), RBX.into()],
                OperandRule::Tied(d) => vec![prefix[usize::from(d)]],
                OperandRule::Use(_, acc) => {
                    let mut o = Vec::new();
                    if acc.reg() {
                        o.push(RCX.into());
                        o.push(R10.into());
                    }
                    if acc.mem() {
                        o.push(
                            Mem::base(RBX.into())
                                .with_index(R12.into(), Scale::X4)
                                .with_disp(8)
                                .into(),
                        );
                        o.push(Operand::slot(slot));
                    }
                    o.extend(imms(acc.imm()).into_iter().map(Operand::Imm));
                    if acc.symbol() {
                        o.push(helper.into());
                    }
                    o
                }
                OperandRule::Cond => vec![x86_64::cond(Cond::L).into()],
                OperandRule::Block => vec![call.into()],
                _ => panic!("unknown rule {rule:?} at {i}"),
            };
            for o in options {
                let mut v = prefix.clone();
                v.push(o);
                if v.iter().filter(|o| o.is_mem()).count() <= 1 {
                    next.push(v);
                }
            }
        }
        out = next;
    }
    out
}

/// The isa-lang mnemonic text a MIR opcode decodes to.
fn mnemonic(name: &str) -> &str {
    match name {
        "mov32ri" | "mov64ri" | "mov32" | "mov64" | "store8" | "store16" | "store32"
        | "store64" => "mov",
        "movsx32to64" => "movsxd",
        "ud2" | "int3" | "cdq" | "cqo" | "cdqe" | "nop" | "jmp" | "call" => name,
        "setcc" => "setl",
        "jcc" => "jl",
        n if n.starts_with("movsx") => "movsx",
        n if n.starts_with("movzx") => "movzx",
        n if n.starts_with("imulw") => "imul",
        n if n.starts_with("cmov") => "cmovl",
        n => {
            let n = n.strip_suffix("cl").unwrap_or(n);
            let n = n.strip_suffix("ri").unwrap_or(n);
            n.trim_end_matches(|c: char| c.is_ascii_digit())
        }
    }
}

/// Emits `inst` in a function of its own (block 0; blocks 1 and 2 trap; the
/// branch target is block 2, so jumps are not elided) and returns the
/// decoded instruction it produced.
fn emit_inst(opcode: Opcode, ops: &[Operand]) -> Option<Decoded> {
    let desc = TARGET.inst(opcode).unwrap();
    let mut f = Function::new("t", &SYSV);
    f.omit_frame_pointer();
    let _helper = f.symbol("helper").unwrap();
    let _slot = f.stack_slot(SlotKind::Local { size: 8, align: 8 }).unwrap();
    let (b0, b1, b2) = (f.block().unwrap(), f.block().unwrap(), f.block().unwrap());
    f.push(b0, opcode, ops)
        .unwrap_or_else(|e| panic!("{}: {e} {ops:?}", desc.name()));
    if !desc.kind().is_terminator() {
        f.push(b0, op::UD2, &[]).unwrap();
    }
    f.push(b1, op::UD2, &[]).unwrap();
    f.push(b2, op::UD2, &[]).unwrap();
    let mut helper = Function::new("helper", &SYSV);
    let hb = helper.block().unwrap();
    helper.push(hb, op::RET, &[]).unwrap();
    let mut e = Emitter::new();
    let _ = e
        .function(&f)
        .unwrap_or_else(|err| panic!("{}: {err} {ops:?}", desc.name()));
    let _ = e.function(&helper).unwrap();
    let p = e.finish().unwrap();
    let want = mnemonic(desc.name());
    decode_all(p.bytes())
        .filter_map(Result::ok)
        .map(|(_, d)| d)
        .find(|d| d.inst().to_string().split(' ').next() == Some(want))
}

fn preg_for(hw: u8) -> PReg {
    PReg::new(usize::from(hw)).unwrap()
}

#[test]
fn test_every_opcode_encodes_every_accepted_shape_with_matching_flags() {
    let shift = ["shl", "shr", "sar", "rol", "ror"];
    let mut checked = 0;
    for (i, desc) in TARGET.insts().iter().enumerate() {
        let opcode = Opcode::new(i as u16);
        match desc.kind() {
            // Tail calls and table branches have their own tests (emit.rs).
            InstKind::Copy
            | InstKind::ParallelCopy
            | InstKind::Return
            | InstKind::TailCall
            | InstKind::Switch => continue,
            _ => {}
        }
        // syscall is raw bytes; landingpad emits nothing.
        if desc.name() == "syscall" || desc.name() == "landingpad" {
            continue;
        }
        // Operands are made in a scratch function laid out like emit_inst's,
        // so their ids (slot 0, symbol 0, block 2) mean the same there.
        let mut probe = Function::new("t", &SYSV);
        let _ = probe
            .stack_slot(SlotKind::Local { size: 8, align: 8 })
            .unwrap();
        let (_, _, b2) = (
            probe.block().unwrap(),
            probe.block().unwrap(),
            probe.block().unwrap(),
        );
        let all = shapes(&mut probe, desc.operands(), b2);
        assert!(!all.is_empty(), "{}", desc.name());
        for ops in all {
            let decoded = emit_inst(opcode, &ops).unwrap_or_else(|| {
                panic!(
                    "{}: no `{}` in the output for {ops:?}",
                    desc.name(),
                    mnemonic(desc.name())
                )
            });
            checked += 1;
            if desc.kind() == InstKind::Call
                || desc.kind() == InstKind::Jump
                || desc.name() == "ud2"
            {
                continue;
            }
            // Flags: defined iff the form writes or undefines any; read iff
            // it reads any, may leave some unchanged, or is a shift/rotate.
            let fx = decoded.flags();
            let defs_flags = desc.implicit_defs().contains(&FLAGS);
            let uses_flags = desc.implicit_uses().contains(&FLAGS);
            let clobbered = fx.clobbered();
            assert_eq!(
                defs_flags,
                !clobbered.is_empty(),
                "{} defines flags",
                desc.name()
            );
            let partial = !clobbered.is_empty() && clobbered != Flags::ALL;
            let is_shift = shift.iter().any(|s| desc.name().starts_with(s));
            assert_eq!(
                uses_flags,
                !fx.read().is_empty() || partial || is_shift,
                "{} uses flags (read {:?}, clobbered {:?})",
                desc.name(),
                fx.read(),
                clobbered
            );
            // Fixed registers in the form's signature are implicit uses.
            for kind in decoded.form().operands() {
                if let OperandKind::Fixed(r) = kind {
                    assert!(
                        desc.implicit_uses().contains(&preg_for(r.num())),
                        "{} reads {r}",
                        desc.name()
                    );
                }
            }
        }
    }
    assert!(checked > 500, "{checked} shapes");
}

#[test]
fn test_implicit_operands_of_the_multiply_and_divide_family() {
    use backend_lang::x86_64::{RAX, RDX};
    for name in ["mul32", "mul64", "imulw32", "imulw64"] {
        let d = TARGET.inst(TARGET.opcode(name).unwrap()).unwrap();
        assert_eq!(d.implicit_uses(), [RAX]);
        assert_eq!(d.implicit_defs(), [RAX, RDX, FLAGS]);
    }
    for name in ["div32", "div64", "idiv32", "idiv64"] {
        let d = TARGET.inst(TARGET.opcode(name).unwrap()).unwrap();
        assert_eq!(d.implicit_uses(), [RAX, RDX]);
        assert_eq!(d.implicit_defs(), [RAX, RDX, FLAGS]);
    }
}

#[test]
fn test_rules_are_well_formed() {
    for desc in TARGET.insts() {
        let rules = desc.operands();
        let defs = desc.defs();
        for (i, r) in rules.iter().enumerate() {
            match *r {
                OperandRule::Def(c) => assert!(i < defs && c == GPR, "{}", desc.name()),
                OperandRule::Tied(d) => assert!(usize::from(d) < defs, "{}", desc.name()),
                OperandRule::Use(c, _) => assert_eq!(c, GPR, "{}", desc.name()),
                _ => {}
            }
        }
        let _ = Reg::from(RCX);
    }
}
