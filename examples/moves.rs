//! The parallel-move resolver on its own: a rotation of three registers, a
//! value fanned out to a stack slot, a swap of two slots, and an immediate,
//! all as one parallel copy, resolved with and without a free register.
//!
//! ```sh
//! cargo run --example moves
//! ```

use backend_lang::RegSet;
use backend_lang::mir::StackSlot;
use backend_lang::moves::{self, Move, MoveEnv, Step};
use backend_lang::x86_64::{self, GPR, R11, RAX, RBX, RCX};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (s0, s1) = (StackSlot::new(0), StackSlot::new(1));
    let copy = [
        Move::new(RAX, RBX, GPR),
        Move::new(RBX, RCX, GPR),
        Move::new(RCX, RAX, GPR),
        Move::new(s0, s1, GPR),
        Move::new(s1, s0, GPR),
        Move::new(StackSlot::new(2), RAX, GPR),
        Move::new(StackSlot::new(3), 1_i64 << 40, GPR),
    ];
    let hidden = MoveEnv::new()
        .with_scratch_slot(StackSlot::new(10))
        .with_victim_slot(StackSlot::new(11));
    for (title, env) in [
        ("no free register (swaps, borrowed register)", hidden),
        ("r11 free", hidden.with_free(RegSet::of(&[R11]))),
    ] {
        println!("{title}:");
        for step in moves::resolve(&x86_64::TARGET, &copy, &env)? {
            println!("  {}", show(step));
        }
    }
    Ok(())
}

fn show(step: Step) -> String {
    let name = |l: backend_lang::moves::Loc| match l {
        backend_lang::moves::Loc::Reg(r) => x86_64::TARGET.reg(r).name().to_string(),
        backend_lang::moves::Loc::Slot(s) => s.to_string(),
    };
    match step {
        Step::Move { dst, src, .. } => match src {
            backend_lang::moves::Source::Loc(l) => format!("{} := {}", name(dst), name(l)),
            backend_lang::moves::Source::Imm(v) => format!("{} := {v}", name(dst)),
        },
        Step::Swap { a, b, .. } => format!(
            "swap {}, {}",
            x86_64::TARGET.reg(a).name(),
            x86_64::TARGET.reg(b).name()
        ),
    }
}
