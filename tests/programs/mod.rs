//! MIR programs shared by the emission tests and the Linux executable test.
//! Every program ends in `exit(result)` (`syscall` with `rax = 60`).

#![allow(dead_code, reason = "each test binary uses a different part")]
#![allow(clippy::unwrap_used, reason = "test support")]

use backend_lang::mir::{Effects, Function, Mem, MirError, Operand, Scale, SlotKind};
use backend_lang::x86_64::{self, op, *};
use isa_lang::x86_64::Cond;

fn imm(v: i64) -> Operand {
    Operand::Imm(v)
}

/// Pushes `rax = 60; syscall` reading `rdi`, then `ud2`.
pub fn exit(f: &mut Function, b: backend_lang::mir::Block) -> Result<(), MirError> {
    f.push(b, op::MOV64RI, &[RAX.into(), imm(60)])?;
    f.push_with(b, op::SYSCALL, &[], Effects::new().uses(&[RDI]))?;
    f.push(b, op::UD2, &[])?;
    Ok(())
}

/// Fibonacci by iteration, `a, b = b, a + b` ten times, with the counter in
/// a stack slot and every loop edge a parallel copy: the entry edge swaps
/// `rax` and `rdx` and stores the count, the back edge rotates values
/// through registers and the slot, and the exit edge moves the result into
/// `rdi`. Exits with fib(10) = 55.
pub fn fib() -> Result<Function, MirError> {
    let mut f = Function::new("fib", &SYSV);
    let n = f.stack_slot(SlotKind::Local { size: 8, align: 8 })?;
    let (b0, b1, b2, b3) = (f.block()?, f.block()?, f.block()?, f.block()?);
    // b1(a = rax, b = rdx, n = ss0)
    f.add_param(b1, RAX.into())?;
    f.add_param(b1, RDX.into())?;
    f.add_param(b1, Operand::slot(n))?;
    // b2(result = rdi)
    f.add_param(b2, RDI.into())?;

    f.push(b0, op::MOV64RI, &[RDX.into(), imm(0)])?;
    f.push(b0, op::MOV64RI, &[RAX.into(), imm(1)])?;
    let enter = f.block_call(b1, &[RDX.into(), RAX.into(), imm(10)])?;
    f.push(b0, op::JMP, &[enter.into()])?;

    f.push(b1, op::CMP64, &[Operand::slot(n), imm(0)])?;
    let done = f.block_call(b2, &[RAX.into()])?;
    let body = f.block_call(b3, &[])?;
    f.push(
        b1,
        op::JCC,
        &[x86_64::cond(Cond::E).into(), done.into(), body.into()],
    )?;

    exit(&mut f, b2)?;

    f.push(b3, op::MOV64, &[RSI.into(), RAX.into()])?;
    f.push(b3, op::ADD64, &[RSI.into(), RSI.into(), RDX.into()])?;
    f.push(b3, op::MOV64, &[RCX.into(), Operand::slot(n)])?;
    f.push(b3, op::DEC64, &[RCX.into(), RCX.into()])?;
    let again = f.block_call(b1, &[RDX.into(), RSI.into(), RCX.into()])?;
    f.push(b3, op::JMP, &[again.into()])?;
    Ok(f)
}

/// `fact(n) = n <= 1 ? 1 : n * fact(n - 1)`, recursive, SysV: `n` in `rdi`,
/// kept across the call in callee-saved `rbx`, the result in `rax`.
pub fn fact() -> Result<Function, MirError> {
    let mut f = Function::new("fact", &SYSV);
    let (b0, base, rec) = (f.block()?, f.block()?, f.block()?);
    f.add_param(b0, RDI.into())?;
    f.push(b0, op::CMP64, &[RDI.into(), imm(1)])?;
    let to_base = f.block_call(base, &[])?;
    let to_rec = f.block_call(rec, &[])?;
    f.push(
        b0,
        op::JCC,
        &[x86_64::cond(Cond::Le).into(), to_base.into(), to_rec.into()],
    )?;

    f.push(base, op::MOV64RI, &[RAX.into(), imm(1)])?;
    f.push_with(base, op::RET, &[], Effects::new().uses(&[RAX]))?;

    f.push(rec, op::COPY, &[RBX.into(), RDI.into()])?;
    f.push(rec, op::SUB64, &[RDI.into(), RDI.into(), imm(1)])?;
    let callee = f.symbol("fact")?;
    f.push_with(
        rec,
        op::CALL,
        &[callee.into()],
        Effects::new()
            .uses(&[RDI])
            .defs(&[RAX])
            .clobbers(SYSV.call_clobbers()),
    )?;
    f.push(rec, op::IMUL64, &[RAX.into(), RAX.into(), RBX.into()])?;
    f.push_with(rec, op::RET, &[], Effects::new().uses(&[RAX]))?;
    Ok(f)
}

/// `main`: exit(fact(5)) = 120.
pub fn fact_main() -> Result<Function, MirError> {
    let mut f = Function::new("main", &SYSV);
    let b = f.block()?;
    f.push(b, op::MOV64RI, &[RDI.into(), imm(5)])?;
    let callee = f.symbol("fact")?;
    f.push_with(
        b,
        op::CALL,
        &[callee.into()],
        Effects::new()
            .uses(&[RDI])
            .defs(&[RAX])
            .clobbers(SYSV.call_clobbers()),
    )?;
    f.push(b, op::MOV64, &[RDI.into(), RAX.into()])?;
    exit(&mut f, b)?;
    Ok(f)
}

/// `helper(x) = x - 40`.
pub fn helper() -> Result<Function, MirError> {
    let mut f = Function::new("helper", &SYSV);
    let b = f.block()?;
    f.add_param(b, RDI.into())?;
    f.push(b, op::MOV64, &[RAX.into(), RDI.into()])?;
    f.push(b, op::SUB64, &[RAX.into(), RAX.into(), imm(40)])?;
    f.push_with(b, op::RET, &[], Effects::new().uses(&[RAX]))?;
    f.omit_frame_pointer();
    Ok(f)
}

/// A mix of the table: `cqo`/`idiv`, three-operand `imul`, `lea` with an
/// index, a store through a register pointing at a slot, a slot load,
/// shifts by an immediate and by `cl`, `cmp`/`setcc`/`movzx`, `cmov`, the
/// address of a function by `lea`, and an indirect call. Exits with 200.
pub fn kitchen() -> Result<Function, MirError> {
    let mut f = Function::new("kitchen", &SYSV);
    let s = f.stack_slot(SlotKind::Local { size: 8, align: 8 })?;
    let b = f.block()?;
    let helper = f.symbol("helper")?;
    f.push(b, op::MOV64RI, &[RAX.into(), imm(1000)])?;
    f.push(b, op::MOV64RI, &[RCX.into(), imm(7)])?;
    f.push(b, op::CQO, &[])?;
    f.push(b, op::IDIV64, &[RCX.into()])?; // rax = 142, rdx = 6
    f.push(b, op::IMUL64RI, &[R8.into(), RAX.into(), imm(3)])?; // 426
    let addr = Mem::base(R8.into())
        .with_index(RDX.into(), Scale::X8)
        .with_disp(4);
    f.push(b, op::LEA64, &[R9.into(), addr.into()])?; // 426 + 48 + 4 = 478
    f.push(b, op::LEA64, &[R10.into(), Operand::slot(s)])?;
    f.push(b, op::STORE64, &[Mem::base(R10.into()).into(), R9.into()])?;
    f.push(b, op::MOV64, &[R11.into(), Operand::slot(s)])?; // 478
    f.push(b, op::SHL64, &[R11.into(), R11.into(), imm(2)])?; // 1912
    f.push(b, op::MOV64RI, &[RCX.into(), imm(3)])?;
    f.push(b, op::SAR64CL, &[R11.into(), R11.into()])?; // 239
    f.push(b, op::CMP64, &[R11.into(), imm(239)])?;
    f.push(b, op::SETCC, &[RSI.into(), x86_64::cond(Cond::E).into()])?;
    f.push(b, op::MOVZX8, &[RSI.into(), RSI.into()])?; // 1
    f.push(b, op::MOV64RI, &[RDI.into(), imm(100)])?;
    f.push(
        b,
        op::CMOV64,
        &[
            RDI.into(),
            RDI.into(),
            R11.into(),
            x86_64::cond(Cond::E).into(),
        ],
    )?; // 239
    f.push(b, op::ADD64, &[RDI.into(), RDI.into(), RSI.into()])?; // 240
    f.push(b, op::LEA64, &[RAX.into(), helper.into()])?;
    f.push_with(
        b,
        op::CALL,
        &[RAX.into()],
        Effects::new()
            .uses(&[RDI])
            .defs(&[RAX])
            .clobbers(SYSV.call_clobbers()),
    )?; // 200
    f.push(b, op::MOV64, &[RDI.into(), RAX.into()])?;
    exit(&mut f, b)?;
    Ok(f)
}
