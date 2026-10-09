//! The Tier-1 path: build a MIR function on physical registers, print it,
//! emit it, and list the machine code.
//!
//! The function sums `1..=n` (SysV: `n` in `rdi`, the result in `rax`). Its
//! loop computes the next accumulator into `rcx` and the next counter into
//! `rax`, the opposite of where the loop header keeps them, so the back edge
//! is a parallel swap; the resolver turns it into one `xchg`.
//!
//! ```sh
//! cargo run --example emit
//! ```

use backend_lang::mir::{Effects, Function, Mem, Operand};
use backend_lang::x86_64::{self, RAX, RCX, RDI, RDX, SYSV, op};
use isa_lang::x86_64::{Cond, disassemble};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut f = Function::new("sum", &SYSV);
    f.omit_frame_pointer();
    let (entry, head, exit) = (f.block()?, f.block()?, f.block()?);
    f.add_param(entry, RDI.into())?;
    f.add_param(head, RAX.into())?; // acc
    f.add_param(head, RCX.into())?; // i
    f.add_param(exit, RAX.into())?; // result

    let start = f.block_call(head, &[Operand::Imm(0), RDI.into()])?;
    f.push(entry, op::JMP, &[start.into()])?;

    // acc' = acc + i (into rdx, then rcx); i' = i - 1 (into rax).
    f.push(
        head,
        op::LEA64,
        &[
            RDX.into(),
            Mem::base(RAX.into())
                .with_index(RCX.into(), backend_lang::mir::Scale::X1)
                .into(),
        ],
    )?;
    f.push(
        head,
        op::LEA64,
        &[RAX.into(), Mem::base(RCX.into()).with_disp(-1).into()],
    )?;
    f.push(head, op::MOV64, &[RCX.into(), RDX.into()])?;
    f.push(head, op::CMP64, &[RAX.into(), Operand::Imm(0)])?;
    // Loop with (acc', i') = (rcx, rax): parameters (rax, rcx), a swap.
    let again = f.block_call(head, &[RCX.into(), RAX.into()])?;
    let done = f.block_call(exit, &[RCX.into()])?;
    f.push(
        head,
        op::JCC,
        &[x86_64::cond(Cond::Ne).into(), again.into(), done.into()],
    )?;
    f.push_with(exit, op::RET, &[], Effects::new().uses(&[RAX]))?;

    f.verify()?;
    print!("{f}");
    let code = backend_lang::emit(&f)?;
    println!("\n{} bytes of x86-64:", code.len());
    print!("{}", disassemble(&code, 0));
    Ok(())
}
