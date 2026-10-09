//! Instruction selection: one ir-lang 2.0 function to a MIR function on
//! virtual registers.
//!
//! Blocks and block parameters map one to one (IR.md §15): an IR edge's
//! arguments become the MIR branch's arguments, resolved as a parallel copy
//! at emission. Every value of an integer, `bool`, `ptr`, or `ref` type lives
//! in one general-purpose virtual register.
//!
//! # Value representation
//!
//! A value of a type narrower than 64 bits occupies the low bits of its
//! register; the bits above its width are **unspecified**, exactly as the
//! x86-64 32-bit operations leave them. Arithmetic on 8-, 16-, and 32-bit
//! types runs at 32 bits; wherever the upper bits matter (comparisons,
//! division, right shifts, overflow checks, widening conversions, switch
//! values, array indices) the value is extended first by its type's
//! signedness. `bool` is the exception: always exactly 0 or 1, so it can be
//! tested, stored, and converted directly; booleans entering from outside
//! (parameters, call results, loads) are normalized.
//!
//! # Policies
//!
//! Every OPS integer operation is selected under every policy ir-lang can
//! carry (IR.md §4): `wrap` computes modulo the width; `trap` branches to a
//! per-function trap block that calls the runtime trap handler
//! ([`Options::trap_symbol`](crate::Options)) with the OPS code; `error`
//! (only inside a `check` terminator) branches to the error edge, passing
//! the OPS code as `err`. Division checks zero, then `MIN / -1`, before it
//! divides, so the processor never faults. Shifts check or mask their
//! amount.
//!
//! # What is not selected yet
//!
//! Floating-point types and operations wait on isa-lang's SSE forms (ISSUES
//! P11), as do `lock`-prefixed atomic read-modify-writes other than `xchg`
//! and `cmpxchg`; `aapcs64` waits on the AArch64 target; 32-bit pointer
//! modules on a 32-bit target. Each is reported as
//! [`IselError::Unsupported`], never miscompiled.

mod call;
mod mem;
mod ops;
mod term;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use ir_lang::{
    Block as IBlock, CallConv, FuncId, InstData, Module, PointerWidth, StackSlot as IStackSlot,
    Terminator, Type, Value,
};

use crate::compile::Options;
use crate::mir::{self, Block, Effects, Function, Inst, MirError, Operand, StackSlot, Symbol};
use crate::regs::{PReg, Reg};
use crate::target::abi::Abi;
use crate::x86_64::{self, GPR, op};

/// Why a function could not be selected.
///
/// # Examples
///
/// ```
/// use backend_lang::isel::IselError;
/// use backend_lang::Options;
/// use ir_lang::{Linkage, Module, Signature, Type};
///
/// let mut m = Module::new("m");
/// let f = m.declare_function("f", &Signature::new(&[Type::F64], &[]), Linkage::Export)?;
/// let mut b = m.build(f)?;
/// b.ret(&[])?;
/// let err = backend_lang::isel::select(&m, f, &Options::sysv()).unwrap_err();
/// assert!(matches!(err, IselError::Unsupported { .. }));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum IselError {
    /// The module does not validate (ir-lang's [`Module::validate`]).
    Invalid(ir_lang::ValidationError),
    /// The function handle is not in the module, or it is an import.
    NoBody(FuncId),
    /// A construct this release does not select, and why.
    Unsupported {
        /// The function.
        func: FuncId,
        /// What, and what it waits on.
        what: &'static str,
    },
    /// Building the MIR failed (a 32-bit table limit, or an internal
    /// inconsistency reported rather than panicking).
    Mir(MirError),
}

impl fmt::Display for IselError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IselError::Invalid(e) => write!(f, "invalid module: {e}"),
            IselError::NoBody(func) => write!(f, "{func} has no body to select"),
            IselError::Unsupported { func, what } => write!(f, "{func}: {what}"),
            IselError::Mir(e) => write!(f, "building machine IR: {e}"),
        }
    }
}

impl core::error::Error for IselError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            IselError::Invalid(e) => Some(e),
            IselError::Mir(e) => Some(e),
            _ => None,
        }
    }
}

impl From<MirError> for IselError {
    fn from(e: MirError) -> IselError {
        IselError::Mir(e)
    }
}

type Result<T> = core::result::Result<T, IselError>;

/// Selects `func` of a validated `module` into a MIR function on virtual
/// registers, named after the function and under the calling convention its
/// signature selects. The module is validated first.
///
/// # Errors
///
/// [`IselError`].
///
/// # Examples
///
/// ```
/// use backend_lang::Options;
/// use ir_lang::{Linkage, Module, Overflow, Signature, Type};
///
/// let mut m = Module::new("m");
/// let f = m.declare_function("inc", &Signature::new(&[Type::I64], &[Type::I64]), Linkage::Export)?;
/// let mut b = m.build(f)?;
/// let x = b.param(0)?;
/// let one = b.iconst(Type::I64, 1)?;
/// let y = b.add(x, one, Overflow::Wrap)?;
/// b.ret(&[y])?;
///
/// let mir = backend_lang::isel::select(&m, f, &Options::sysv())?;
/// assert_eq!(mir.name(), "inc");
/// assert!(mir.to_string().contains("add64"));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn select(module: &Module, func: FuncId, options: &Options) -> Result<Function> {
    module.validate().map_err(IselError::Invalid)?;
    select_validated(module, func, options)
}

/// [`select`] for a module the caller has already validated.
pub(crate) fn select_validated(
    module: &Module,
    func: FuncId,
    options: &Options,
) -> Result<Function> {
    let decl = module.func_decl(func).ok_or(IselError::NoBody(func))?;
    let body = module.function(func).ok_or(IselError::NoBody(func))?;
    let sig = module.func_signature(func).ok_or(IselError::NoBody(func))?;
    if module.pointer_width() != PointerWidth::P64 {
        return Err(IselError::Unsupported {
            func,
            what: "32-bit pointers need a 32-bit target",
        });
    }
    let abi = abi_for(func, sig.conv, options)?;
    let mut sel = Sel::new(module, func, body, decl.name(), abi, options);
    sel.run(sig)?;
    Ok(sel.f)
}

/// The convention a signature's tag selects.
pub(crate) fn abi_for(func: FuncId, conv: CallConv, options: &Options) -> Result<&'static Abi> {
    match conv {
        CallConv::Fast | CallConv::C => Ok(options.abi),
        CallConv::SysV64 => Ok(&x86_64::SYSV),
        CallConv::Win64 => Ok(&x86_64::WIN64),
        _ => Err(IselError::Unsupported {
            func,
            what: "the aapcs64 convention needs the AArch64 target",
        }),
    }
}

/// How a narrow value is widened when its upper bits matter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ext {
    /// By the type's signedness.
    Type,
    /// With zeros.
    Zero,
    /// With copies of the sign bit.
    Sign,
}

/// What a failing operation does: branch to a `check`'s error edge, or trap.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Fail<'a> {
    /// The error edge of a `check` (the OPS code becomes `err`).
    Edge(&'a ir_lang::BlockCall),
    /// A deterministic trap with the OPS code.
    Trap,
}

/// Where an address comes from, for folding constant offsets into memory
/// operands.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Addr {
    /// `slot + offset`, addressed from the stack pointer.
    Slot(StackSlot, i64),
    /// `reg + offset`.
    Reg(Reg, i64),
}

/// The selection state of one function.
pub(crate) struct Sel<'m> {
    pub(crate) m: &'m Module,
    pub(crate) func: FuncId,
    pub(crate) irf: &'m ir_lang::Function,
    pub(crate) opts: &'m Options,
    pub(crate) abi: &'static Abi,
    pub(crate) f: Function,
    /// The register of each IR value.
    vals: Vec<Option<Reg>>,
    /// The bits of each IR value known to be a constant.
    consts: Vec<Option<u64>>,
    /// Folded addresses of IR address values.
    addrs: Vec<Option<Addr>>,
    /// Uses of each IR value.
    uses: Vec<u32>,
    /// Compares lowered at their single use (a branch or select in the same
    /// block) so the flags reach it.
    pub(crate) fused: Vec<bool>,
    blocks: Vec<Option<Block>>,
    /// IR slot → MIR slot, and the alignment to round its address up to when
    /// the frame cannot align it.
    slots: Vec<(StackSlot, u32)>,
    syms: BTreeMap<String, Symbol>,
    traps: BTreeMap<u32, Block>,
    gc_slots: Vec<StackSlot>,
    incoming: BTreeMap<u32, StackSlot>,
    fence_slot: Option<StackSlot>,
    /// The block instructions are appended to.
    pub(crate) cur: Block,
}

impl<'m> Sel<'m> {
    fn new(
        m: &'m Module,
        func: FuncId,
        irf: &'m ir_lang::Function,
        name: &str,
        abi: &'static Abi,
        opts: &'m Options,
    ) -> Sel<'m> {
        let nv = irf.value_count();
        let mut f = Function::with_capacity(name, abi, irf.inst_count() * 2, irf.inst_count() * 6);
        if !opts.frame_pointer {
            f.omit_frame_pointer();
        }
        Sel {
            m,
            func,
            irf,
            opts,
            abi,
            f,
            vals: vec![None; nv],
            consts: vec![None; nv],
            addrs: vec![None; nv],
            uses: vec![0; nv],
            fused: vec![false; nv],
            blocks: vec![None; irf.block_count()],
            slots: Vec::new(),
            syms: BTreeMap::new(),
            traps: BTreeMap::new(),
            gc_slots: Vec::new(),
            incoming: BTreeMap::new(),
            fence_slot: None,
            cur: Block::new(0),
        }
    }

    pub(crate) fn unsupported<T>(&self, what: &'static str) -> Result<T> {
        Err(IselError::Unsupported {
            func: self.func,
            what,
        })
    }

    fn run(&mut self, sig: &ir_lang::Signature) -> Result<()> {
        let irf = self.irf;
        // Reverse postorder of the reachable IR blocks: MIR blocks are laid
        // out in this order, the entry first.
        let order = ir_rpo(irf);
        self.count_uses(&order);
        for &b in &order {
            let mb = self.f.block()?;
            self.blocks[b.index()] = Some(mb);
        }
        for &b in &order {
            if b == irf.entry() {
                continue;
            }
            let mb = self.block(b)?;
            for &p in irf.block_params(b) {
                let ty = self.ty(p)?;
                let _ = ty;
                let v = self.new_reg()?;
                self.vals[p.index()] = Some(v);
                self.f.add_param(mb, v.into())?;
            }
        }
        for slot in 0..irf.slot_count() {
            let data = irf
                .slot(IStackSlot::from_u32(slot as u32))
                .ok_or(MirError::Limit)?;
            // The frame aligns slots to at most 16; a larger alignment is
            // reached by rounding the slot's address up at run time inside a
            // slot made large enough for it.
            let (size, align, round) = if data.align > 16 {
                (data.size.saturating_add(data.align - 16), 16, data.align)
            } else {
                (data.size, data.align.max(1), 0)
            };
            let s = self.f.stack_slot(mir::SlotKind::Local { size, align })?;
            self.slots.push((s, round));
        }
        self.entry(sig)?;
        for &b in &order {
            self.cur = self.block(b)?;
            let insts: Vec<ir_lang::Inst> = irf.insts(b).collect();
            for inst in insts {
                self.inst(inst)?;
            }
            let term = irf.terminator(b).ok_or(MirError::NoBlocks)?;
            self.terminator(b, term)?;
        }
        Ok(())
    }

    /// The entry: incoming arguments from their ABI locations into virtual
    /// registers with one parallel copy.
    fn entry(&mut self, sig: &ir_lang::Signature) -> Result<()> {
        let irf = self.irf;
        let entry = self.block(irf.entry())?;
        self.cur = entry;
        let classes = self.arg_classes(&sig.params)?;
        let mut locs = Vec::new();
        let _ = self.abi.assign_args(&classes, &mut locs);
        let mut dsts = Vec::with_capacity(locs.len());
        let mut srcs = Vec::with_capacity(locs.len());
        for (k, (&p, loc)) in irf.block_params(irf.entry()).iter().zip(&locs).enumerate() {
            let src: Operand = match *loc {
                crate::target::abi::ArgLoc::Reg(r) => {
                    self.f.add_param(entry, r.into())?;
                    r.into()
                }
                crate::target::abi::ArgLoc::Stack(off) => {
                    let s = self.incoming_slot(off)?;
                    self.f.add_param(entry, Operand::slot(s))?;
                    Operand::slot(s)
                }
            };
            let v = self.new_reg()?;
            dsts.push(Operand::from(v));
            srcs.push(src);
            self.vals[p.index()] = Some(v);
            let _ = k;
        }
        if !dsts.is_empty() {
            dsts.extend(srcs);
            let _ = self.f.push(entry, op::PCOPY, &dsts)?;
        }
        // Booleans from outside are normalized to 0 or 1.
        for &p in irf.block_params(irf.entry()) {
            if irf.value_type(p) == Some(Type::Bool) {
                let r = self.val(p)?;
                let n = self.emit_def(op::MOVZX8, &[r.into()])?;
                self.vals[p.index()] = Some(n);
            }
        }
        Ok(())
    }

    /// The incoming-argument slot at `offset` above the canonical frame
    /// address (one per offset, shared by the entry and tail calls).
    pub(crate) fn incoming_slot(&mut self, offset: u32) -> Result<StackSlot> {
        if let Some(&s) = self.incoming.get(&offset) {
            return Ok(s);
        }
        let s = self
            .f
            .stack_slot(mir::SlotKind::Incoming { offset, size: 8 })?;
        let _ = self.incoming.insert(offset, s);
        Ok(s)
    }

    /// The `i`-th GC root slot (shared by every safepoint: each reloads its
    /// relocated references right after its call).
    pub(crate) fn gc_slot(&mut self, i: usize) -> Result<StackSlot> {
        while self.gc_slots.len() <= i {
            let s = self
                .f
                .stack_slot(mir::SlotKind::Local { size: 8, align: 8 })?;
            self.gc_slots.push(s);
        }
        self.gc_slots
            .get(i)
            .copied()
            .ok_or(IselError::Mir(MirError::Limit))
    }

    /// A slot whose only use is a full-barrier `xchg`.
    pub(crate) fn fence_slot(&mut self) -> Result<StackSlot> {
        if let Some(s) = self.fence_slot {
            return Ok(s);
        }
        let s = self
            .f
            .stack_slot(mir::SlotKind::Local { size: 8, align: 8 })?;
        self.fence_slot = Some(s);
        Ok(s)
    }

    fn count_uses(&mut self, order: &[IBlock]) {
        let irf = self.irf;
        for &b in order {
            for inst in irf.insts(b) {
                if let Some(data) = irf.inst(inst) {
                    data.for_each_operand(|v| {
                        let v = irf.resolve(v);
                        if let Some(n) = self.uses.get_mut(v.index()) {
                            *n += 1;
                        }
                    });
                }
            }
            if let Some(t) = irf.terminator(b) {
                t.for_each_operand(|v| {
                    let v = irf.resolve(v);
                    if let Some(n) = self.uses.get_mut(v.index()) {
                        *n += 1;
                    }
                });
            }
        }
        // A compare whose only use is the branch or a select later in its own
        // block is lowered there, so the flags it sets reach the use. The
        // candidates of the current block are stamped with its position in
        // `order` (plus one), so each test is O(1) however long the block.
        let mut stamp: Vec<u32> = vec![0; self.fused.len()];
        for (k, &b) in order.iter().enumerate() {
            let here = k as u32 + 1;
            let candidate = |stamp: &[u32], c: Value| stamp.get(c.index()) == Some(&here);
            for inst in irf.insts(b) {
                if let Some(InstData::Compare { args, .. }) = irf.inst(inst) {
                    let r = irf.results(inst);
                    if let Some(v) = r.first() {
                        let fusible_ty = irf
                            .value_type(irf.resolve(args[0]))
                            .is_some_and(|t| !t.is_float());
                        if self.uses.get(v.index()) == Some(&1) && fusible_ty {
                            if let Some(s) = stamp.get_mut(v.index()) {
                                *s = here;
                            }
                        }
                    }
                }
                if let Some(InstData::Select { cond, .. }) = irf.inst(inst) {
                    let c = irf.resolve(*cond);
                    if candidate(&stamp, c) {
                        self.fused[c.index()] = true;
                    }
                }
            }
            if let Some(Terminator::Branch { cond, .. }) = irf.terminator(b) {
                let c = irf.resolve(*cond);
                if candidate(&stamp, c) {
                    self.fused[c.index()] = true;
                }
            }
        }
    }

    /// The MIR block of an IR block.
    pub(crate) fn block(&self, b: IBlock) -> Result<Block> {
        self.blocks
            .get(b.index())
            .copied()
            .flatten()
            .ok_or(IselError::Mir(MirError::UnknownBlock(Block::new(
                b.as_u32(),
            ))))
    }

    /// A fresh general-purpose virtual register.
    pub(crate) fn new_reg(&mut self) -> Result<Reg> {
        Ok(self.f.vreg(GPR)?)
    }

    /// The type of an IR value; floats are not selected yet.
    pub(crate) fn ty(&self, v: Value) -> Result<Type> {
        let t = self
            .irf
            .value_type(self.irf.resolve(v))
            .ok_or(IselError::Mir(MirError::Limit))?;
        if t.is_float() {
            return self.unsupported("floating point waits on isa-lang's SSE forms (ISSUES P11)");
        }
        Ok(t)
    }

    /// The register holding IR value `v`.
    pub(crate) fn val(&self, v: Value) -> Result<Reg> {
        let v = self.irf.resolve(v);
        self.vals
            .get(v.index())
            .copied()
            .flatten()
            .ok_or(IselError::Mir(MirError::Limit))
    }

    /// The constant bits of `v`, if it is one.
    pub(crate) fn konst(&self, v: Value) -> Option<u64> {
        let v = self.irf.resolve(v);
        self.consts.get(v.index()).copied().flatten()
    }

    /// The operands of a commutative two-address operation in the order that
    /// spares a copy: the definition shares the first operand's register,
    /// which it can only do when that operand dies here. So a constant moves
    /// to the right (where it becomes an immediate), and a left operand still
    /// needed later trades places with a right operand used only here.
    pub(crate) fn commute(&self, args: [Value; 2]) -> [Value; 2] {
        let uses = |v: Value| {
            let v = self.irf.resolve(v);
            self.uses.get(v.index()).copied().unwrap_or(0)
        };
        let (k0, k1) = (self.konst(args[0]).is_some(), self.konst(args[1]).is_some());
        if (k0 && !k1) || (!k1 && uses(args[0]) > 1 && uses(args[1]) == 1) {
            [args[1], args[0]]
        } else {
            args
        }
    }

    pub(crate) fn set_val(&mut self, v: Value, r: Reg) {
        if let Some(slot) = self.vals.get_mut(v.index()) {
            *slot = Some(r);
        }
    }

    pub(crate) fn set_addr(&mut self, v: Value, a: Addr) {
        if let Some(slot) = self.addrs.get_mut(v.index()) {
            *slot = Some(a);
        }
    }

    pub(crate) fn addr_of(&self, v: Value) -> Option<Addr> {
        let v = self.irf.resolve(v);
        self.addrs.get(v.index()).copied().flatten()
    }

    pub(crate) fn mir_slot(&self, s: IStackSlot) -> Result<(StackSlot, u32)> {
        self.slots
            .get(s.index())
            .copied()
            .ok_or(IselError::Mir(MirError::UnknownSlot(StackSlot::new(
                s.as_u32(),
            ))))
    }

    /// The symbol for `name` in the function under construction.
    pub(crate) fn symbol(&mut self, name: &str) -> Result<Symbol> {
        if let Some(&s) = self.syms.get(name) {
            return Ok(s);
        }
        let s = self.f.symbol(name)?;
        let _ = self.syms.insert(String::from(name), s);
        Ok(s)
    }

    /// Appends an instruction to the current block.
    pub(crate) fn emit(&mut self, opcode: crate::target::Opcode, ops: &[Operand]) -> Result<Inst> {
        Ok(self.f.push(self.cur, opcode, ops)?)
    }

    /// Appends an instruction with extra implicit operands.
    pub(crate) fn emit_with(
        &mut self,
        opcode: crate::target::Opcode,
        ops: &[Operand],
        effects: Effects<'_>,
    ) -> Result<Inst> {
        Ok(self.f.push_with(self.cur, opcode, ops, effects)?)
    }

    /// Appends `d = opcode ops...` with a fresh `d`, returning it.
    pub(crate) fn emit_def(
        &mut self,
        opcode: crate::target::Opcode,
        ops: &[Operand],
    ) -> Result<Reg> {
        let d = self.new_reg()?;
        let mut all = [Operand::Imm(0); 5];
        all[0] = d.into();
        let n = ops.len().min(4);
        all[1..=n].copy_from_slice(&ops[..n]);
        let _ = self.emit(opcode, &all[..=n])?;
        Ok(d)
    }

    /// `d = src` into a fresh register.
    pub(crate) fn copy_to_new(&mut self, src: Operand) -> Result<Reg> {
        let d = self.new_reg()?;
        let _ = self.emit(op::COPY, &[d.into(), src])?;
        Ok(d)
    }

    /// Starts a new block (with no parameters) and makes it current.
    pub(crate) fn new_block(&mut self) -> Result<Block> {
        Ok(self.f.block()?)
    }

    /// `jcc cc, then(), else()` from the current block.
    pub(crate) fn branch(
        &mut self,
        cc: isa_lang::x86_64::Cond,
        then: Block,
        other: Block,
    ) -> Result<()> {
        let t = self.f.block_call(then, &[])?;
        let e = self.f.block_call(other, &[])?;
        let _ = self.emit(op::JCC, &[x86_64::cond(cc).into(), t.into(), e.into()])?;
        Ok(())
    }

    /// `jmp target(args)` from the current block.
    pub(crate) fn jump(&mut self, target: Block, args: &[Operand]) -> Result<()> {
        let c = self.f.block_call(target, args)?;
        let _ = self.emit(op::JMP, &[c.into()])?;
        Ok(())
    }

    /// The trap block for `code`: the code into the first argument register,
    /// a call to the trap handler, `ud2` after it in case it returns.
    pub(crate) fn trap_block(&mut self, code: u32) -> Result<Block> {
        if let Some(&b) = self.traps.get(&code) {
            return Ok(b);
        }
        let saved = self.cur;
        let b = self.new_block()?;
        self.cur = b;
        self.trap_call(code)?;
        self.cur = saved;
        let _ = self.traps.insert(code, b);
        Ok(b)
    }

    /// In the current block: call the trap handler with `code`, then `ud2`.
    pub(crate) fn trap_call(&mut self, code: u32) -> Result<()> {
        let c_abi = self.opts.abi;
        let arg = first_int_arg(c_abi)?;
        let sym = self.symbol(self.opts.trap_symbol)?;
        let _ = self.emit(op::COPY, &[arg.into(), Operand::Imm(i64::from(code))])?;
        let uses = [arg];
        let _ = self.emit_with(
            op::CALL,
            &[sym.into()],
            Effects::new().uses(&uses).clobbers(c_abi.call_clobbers()),
        )?;
        let _ = self.emit(op::UD2, &[])?;
        Ok(())
    }

    /// Branches to the failure of an operation when `cc` holds, continuing in
    /// a fresh block otherwise. `code` is the OPS error code.
    pub(crate) fn fail_if(
        &mut self,
        cc: isa_lang::x86_64::Cond,
        fail: Fail<'_>,
        code: u32,
    ) -> Result<()> {
        let next = self.new_block()?;
        match fail {
            Fail::Trap => {
                let t = self.trap_block(code)?;
                self.branch(cc, t, next)?;
            }
            Fail::Edge(dest) => {
                let call = self.edge(dest, &[], None, Some(Operand::Imm(i64::from(code))))?;
                let e = self.f.block_call(next, &[])?;
                let _ = self.emit(op::JCC, &[x86_64::cond(cc).into(), call.into(), e.into()])?;
            }
        }
        self.cur = next;
        Ok(())
    }

    /// Lowers an IR edge to a MIR branch target: arguments are values (an
    /// immediate when constant), call results, the exception payload, or the
    /// error code.
    pub(crate) fn edge(
        &mut self,
        dest: &ir_lang::BlockCall,
        results: &[Operand],
        exn: Option<Operand>,
        err: Option<Operand>,
    ) -> Result<mir::BlockCall> {
        let target = self.block(dest.block)?;
        let mut args = Vec::with_capacity(dest.args.len());
        for a in &dest.args {
            let op = match *a {
                ir_lang::BlockArg::Value(v) => self.operand(v)?,
                ir_lang::BlockArg::Result(i) => results
                    .get(i as usize)
                    .copied()
                    .ok_or(IselError::Mir(MirError::Limit))?,
                ir_lang::BlockArg::Exception => exn.ok_or(IselError::Mir(MirError::Limit))?,
                ir_lang::BlockArg::ErrorCode => err.ok_or(IselError::Mir(MirError::Limit))?,
                _ => return self.unsupported("an edge argument kind this release does not know"),
            };
            args.push(op);
        }
        Ok(self.f.block_call(target, &args)?)
    }

    /// A value as an operand: an immediate when it is a constant, else its
    /// register.
    pub(crate) fn operand(&self, v: Value) -> Result<Operand> {
        match self.konst(v) {
            Some(bits) => Ok(Operand::Imm(bits as i64)),
            None => Ok(self.val(v)?.into()),
        }
    }

    fn inst(&mut self, inst: ir_lang::Inst) -> Result<()> {
        let irf = self.irf;
        let data = irf.inst(inst).ok_or(MirError::Limit)?.clone();
        let results = irf.results(inst);
        match data {
            InstData::Const { ty, bits } => {
                if ty.is_float() {
                    return self
                        .unsupported("floating point waits on isa-lang's SSE forms (ISSUES P11)");
                }
                let r = results.first().ok_or(MirError::Limit)?;
                let d = self.emit_def(op::MOV64RI, &[Operand::Imm(bits as i64)])?;
                self.set_val(r, d);
                if let Some(c) = self.consts.get_mut(r.index()) {
                    *c = Some(bits);
                }
                Ok(())
            }
            InstData::Unary { op, policy, arg } => {
                let r = results.first().ok_or(MirError::Limit)?;
                let d = self.unary(op, policy, arg, Fail::Trap)?;
                self.set_val(r, d);
                Ok(())
            }
            InstData::Binary { op, policy, args } => {
                let r = results.first().ok_or(MirError::Limit)?;
                let d = self.binary(op, policy, args, Fail::Trap)?;
                self.set_val(r, d);
                Ok(())
            }
            InstData::Compare { op, args } => {
                let r = results.first().ok_or(MirError::Limit)?;
                if self.fused.get(r.index()).copied().unwrap_or(false) {
                    return Ok(());
                }
                let cc = self.compare(op, args)?;
                let s = self.emit_def(op::SETCC, &[x86_64::cond(cc).into()])?;
                let d = self.emit_def(op::MOVZX8, &[s.into()])?;
                self.set_val(r, d);
                Ok(())
            }
            InstData::Convert {
                op,
                policy,
                to,
                arg,
            } => {
                let r = results.first().ok_or(MirError::Limit)?;
                let d = self.convert(op, policy, to, arg, Fail::Trap)?;
                self.set_val(r, d);
                Ok(())
            }
            InstData::Select { cond, args } => {
                let r = results.first().ok_or(MirError::Limit)?;
                let _ = self.ty(r)?;
                let cc = self.condition(cond)?;
                let (a, b) = (self.val(args[0])?, self.val(args[1])?);
                // d = cc ? a : b
                let d =
                    self.emit_def(op::CMOV64, &[b.into(), a.into(), x86_64::cond(cc).into()])?;
                self.set_val(r, d);
                Ok(())
            }
            InstData::Fma { .. } => self.unsupported(
                "fma is floating point, which waits on isa-lang's SSE forms (ISSUES P11)",
            ),
            InstData::Call(call) => self.call(&call, results),
            InstData::Safepoint { gc } => self.safepoint(&gc, results),
            other => self.memory(&other, results),
        }
    }

    /// The condition code that holds when IR boolean `cond` is true, with
    /// the flags set: a fused compare, or a test of the boolean.
    pub(crate) fn condition(&mut self, cond: Value) -> Result<isa_lang::x86_64::Cond> {
        let c = self.irf.resolve(cond);
        if self.fused.get(c.index()).copied().unwrap_or(false) {
            if let Some(inst) = self.irf.defining_inst(c) {
                if let Some(InstData::Compare { op, args }) = self.irf.inst(inst) {
                    let (op, args) = (*op, *args);
                    return self.compare(op, args);
                }
            }
        }
        let r = self.val(c)?;
        let _ = self.emit(op::TEST32, &[r.into(), r.into()])?;
        Ok(isa_lang::x86_64::Cond::Ne)
    }
}

/// The first integer argument register of a convention.
pub(crate) fn first_int_arg(abi: &Abi) -> Result<PReg> {
    abi.args()
        .int()
        .first()
        .copied()
        .ok_or(IselError::Mir(MirError::Limit))
}

/// The reachable blocks of an IR function in reverse postorder.
fn ir_rpo(f: &ir_lang::Function) -> Vec<IBlock> {
    let n = f.block_count();
    let mut seen = vec![false; n];
    let mut post = Vec::new();
    let entry = f.entry();
    if entry.index() >= n {
        return post;
    }
    let mut stack: Vec<(IBlock, Vec<IBlock>, usize)> = Vec::new();
    let succs = |b: IBlock| -> Vec<IBlock> {
        f.terminator(b)
            .map(|t| t.successors().map(|c| c.block).collect())
            .unwrap_or_default()
    };
    seen[entry.index()] = true;
    stack.push((entry, succs(entry), 0));
    while let Some(top) = stack.last_mut() {
        if let Some(&s) = top.1.get(top.2) {
            top.2 += 1;
            if s.index() < n && !seen[s.index()] {
                seen[s.index()] = true;
                let next = succs(s);
                stack.push((s, next, 0));
            }
        } else {
            post.push(top.0);
            let _ = stack.pop();
        }
    }
    post.reverse();
    post
}
