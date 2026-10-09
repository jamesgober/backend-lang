//! Emission: a MIR function on physical registers, through isa-lang's
//! assembler, to x86-64 machine code.
//!
//! The emitter lays out the frame, writes the prologue, gives every block a
//! label, and encodes each instruction through the row's isa-lang mapping.
//! Control-flow edges with arguments, `copy`, and `pcopy` are parallel copies
//! and go through the [`MoveResolver`](crate::moves::MoveResolver): register
//! cycles become `xchg`, memory-to-memory moves go through a register borrowed
//! around the move (saved to a hidden frame slot), and a cycle through memory
//! is broken with a second hidden slot. The hidden slots are added to the
//! frame only when an edge needs them.
//!
//! A conditional branch whose taken edge carries moves gets those moves on
//! its own path: the emitter branches on the negated condition around them,
//! which splits the critical edge without adding a block. Moves never touch
//! the flags, and every return runs the epilogue before `ret`.
//!
//! Functions are emitted into one [`Emitter`] and call each other by name;
//! [`Emitter::finish`] runs isa-lang's branch relaxation and patches calls,
//! and reports any name that was called but never defined.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use isa_lang::x86_64::{
    AsmError, Assembler, Cond as ICond, EncodeError, Inst as IInst, Label, Mem as IMem,
    Mnemonic as M, Operand as IOp, Reg as IReg, Scale as IScale, Size,
};

use super::table::{DESCS, ENCODINGS, Enc, Fam, Src};
use super::{GPR, R10, R11, REGS};
use crate::mir::{
    Block, BlockCall, Function, ImplicitKind, Inst, Mem, MirError, Operand, Scale, SlotKind,
    StackSlot,
};
use crate::moves::{Loc, Move, MoveEnv, MoveError, MoveResolver, Source, Step};
use crate::regs::{PReg, Reg, RegClass};
use crate::target::frame::{FrameError, FrameInput, FrameLayout, FrameStep};
use crate::target::{InstKind, OperandRule};

/// Why a function could not be emitted.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::{Function, Operand};
/// use backend_lang::x86_64::{op, EmitError, GPR, SYSV};
///
/// // Emission needs physical registers: run register allocation first.
/// let mut f = Function::new("f", &SYSV);
/// let b = f.block()?;
/// let v = f.vreg(GPR)?;
/// f.push(b, op::MOV64RI, &[v.into(), Operand::Imm(1)])?;
/// f.push(b, op::RET, &[])?;
/// let err = backend_lang::emit(&f).unwrap_err();
/// assert!(matches!(err, EmitError::VirtualRegister { .. }));
/// # Ok::<(), backend_lang::mir::MirError>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EmitError {
    /// The function is malformed ([`Function::verify`] fails).
    Mir(MirError),
    /// The frame cannot be laid out.
    Frame(FrameError),
    /// A parallel copy cannot be resolved.
    Moves(MoveError),
    /// The function's target is not this crate's x86-64 description (a
    /// custom [`Target`](crate::target::Target) must reuse its register and
    /// instruction tables to be emitted by this emitter).
    UnsupportedTarget,
    /// A virtual register remains (in an instruction, or, with `inst` absent,
    /// in a block parameter). Emission runs after register allocation.
    VirtualRegister {
        /// The block.
        block: Block,
        /// The instruction, if not a parameter.
        inst: Option<Inst>,
    },
    /// A tied use is not the same register as its definition. The register
    /// allocator must give them one register (or insert a copy).
    TiedMismatch {
        /// The instruction.
        inst: Inst,
    },
    /// The body writes the stack pointer, or the frame pointer while it is
    /// kept (in an instruction, or, with `inst` absent, as a block
    /// parameter). The frame owns both.
    ReservedWrite {
        /// The block.
        block: Block,
        /// The instruction, if not a parameter.
        inst: Option<Inst>,
    },
    /// The operation needs an instruction isa-lang 0.2 does not encode: a
    /// move of a vector register, or a Win64 prologue saving one. (With
    /// `inst` absent: a prologue, epilogue, or edge move.)
    Unencodable {
        /// The instruction, if one.
        inst: Option<Inst>,
    },
    /// isa-lang refused an instruction (with `inst` absent: a prologue,
    /// epilogue, or edge move).
    Encode {
        /// The instruction, if one.
        inst: Option<Inst>,
        /// isa-lang's reason.
        error: EncodeError,
    },
    /// The assembler failed (code beyond 2 GiB, or a label error).
    Asm(AsmError),
    /// A function was called (or its address taken) but never emitted.
    UndefinedSymbol(String),
    /// Two functions with one name were emitted.
    DuplicateSymbol(String),
    /// A frame large enough to need a probe loop takes its counter in `r11`
    /// or `r10`, and the entry block receives values in both.
    ProbeScratch,
    /// An operand constraint the instruction relies on does not hold: a
    /// table branch's temporary is its index register, or a tail call's
    /// register target is one the epilogue restores.
    Constraint {
        /// The instruction.
        inst: Inst,
    },
    /// A function was declared external and also emitted, or a name was
    /// declared external twice in conflicting ways.
    ExternalConflict(String),
}

impl fmt::Display for EmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EmitError::Mir(e) => fmt::Display::fmt(e, f),
            EmitError::Frame(e) => fmt::Display::fmt(e, f),
            EmitError::Moves(e) => fmt::Display::fmt(e, f),
            EmitError::UnsupportedTarget => {
                f.write_str("the function's target is not the x86-64 description this emitter encodes")
            }
            EmitError::VirtualRegister { block, inst } => match inst {
                Some(i) => write!(f, "{i} in {block} still names a virtual register; allocate registers first"),
                None => write!(f, "a parameter of {block} is a virtual register; allocate registers first"),
            },
            EmitError::TiedMismatch { inst } => write!(
                f,
                "{inst}: a tied use must be the same register as its definition"
            ),
            EmitError::ReservedWrite { block, inst } => match inst {
                Some(i) => write!(f, "{i} in {block} writes the stack or frame pointer"),
                None => write!(f, "a parameter of {block} is the stack or frame pointer"),
            },
            EmitError::Unencodable { inst } => match inst {
                Some(i) => write!(f, "{i}: isa-lang 0.2 has no encoding for this vector-register operation"),
                None => f.write_str("a frame or edge move needs a vector-register instruction isa-lang 0.2 does not encode"),
            },
            EmitError::Encode { inst, error } => match inst {
                Some(i) => write!(f, "{i}: {error}"),
                None => write!(f, "frame or edge move: {error}"),
            },
            EmitError::Asm(e) => fmt::Display::fmt(e, f),
            EmitError::UndefinedSymbol(name) => {
                write!(f, "`{name}` is called but no function of that name was emitted")
            }
            EmitError::DuplicateSymbol(name) => {
                write!(f, "two functions named `{name}` were emitted")
            }
            EmitError::ProbeScratch => f.write_str(
                "the stack probe loop needs r10 or r11, and the entry block receives values in both",
            ),
            EmitError::Constraint { inst } => write!(
                f,
                "{inst}: a table branch's temporary is its index, or a tail call's register target is restored by the epilogue"
            ),
            EmitError::ExternalConflict(name) => {
                write!(f, "`{name}` is declared external and also emitted")
            }
        }
    }
}

impl core::error::Error for EmitError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            EmitError::Mir(e) => Some(e),
            EmitError::Frame(e) => Some(e),
            EmitError::Moves(e) => Some(e),
            EmitError::Encode { error, .. } => Some(error),
            EmitError::Asm(e) => Some(e),
            _ => None,
        }
    }
}

impl From<MirError> for EmitError {
    fn from(e: MirError) -> EmitError {
        EmitError::Mir(e)
    }
}

impl From<FrameError> for EmitError {
    fn from(e: FrameError) -> EmitError {
        EmitError::Frame(e)
    }
}

impl From<MoveError> for EmitError {
    fn from(e: MoveError) -> EmitError {
        EmitError::Moves(e)
    }
}

/// What emission decided about one function.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::Function;
/// use backend_lang::x86_64::{op, Emitter, SYSV};
///
/// let mut f = Function::new("f", &SYSV);
/// let b = f.block()?;
/// f.push(b, op::RET, &[])?;
/// let mut e = Emitter::new();
/// let info = e.function(&f)?;
/// assert_eq!(info.frame().alloc(), 0);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionInfo {
    frame: FrameLayout,
}

impl FunctionInfo {
    /// The frame as laid out, including any hidden slots edge moves needed
    /// (numbered after the function's own).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, SlotKind};
    /// use backend_lang::x86_64::{op, Emitter, SYSV};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let s = f.stack_slot(SlotKind::Local { size: 8, align: 8 })?;
    /// let b = f.block()?;
    /// f.push(b, op::RET, &[])?;
    /// let info = Emitter::new().function(&f)?;
    /// assert!(info.frame().slot_offset(s).is_some());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn frame(&self) -> &FrameLayout {
        &self.frame
    }
}

/// What a relocation computes. `S` is the symbol's address, `A` the addend,
/// and `P` the address of the patched bytes.
///
/// # Examples
///
/// ```
/// use backend_lang::x86_64::RelocKind;
///
/// assert_eq!(RelocKind::Call32.size(), 4);
/// assert_eq!(RelocKind::Abs64.size(), 8);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RelocKind {
    /// `S + A - P`, 32 bits: the displacement of a `call` or `jmp` to a
    /// function (ELF `R_X86_64_PLT32`; object-lang `X86_64Reloc::Plt32`).
    Call32,
    /// `S + A - P`, 32 bits: the displacement of a RIP-relative address
    /// (ELF `R_X86_64_PC32`; object-lang `X86_64Reloc::Pc32`).
    Pc32,
    /// `S + A`, 64 bits: an absolute address stored in data (ELF
    /// `R_X86_64_64`; object-lang `X86_64Reloc::Abs64`).
    Abs64,
}

impl RelocKind {
    /// How many bytes the relocation patches.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::RelocKind;
    ///
    /// assert_eq!(RelocKind::Pc32.size(), 4);
    /// ```
    #[inline]
    #[must_use]
    pub const fn size(self) -> usize {
        match self {
            RelocKind::Call32 | RelocKind::Pc32 => 4,
            RelocKind::Abs64 => 8,
        }
    }
}

/// A place in the code that refers to a symbol defined elsewhere (declared
/// with [`Emitter::declare_external`]): the bytes at `offset` receive the
/// value `kind` computes from the symbol's address and `addend`. The bytes
/// hold zero until a linker patches them.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::Function;
/// use backend_lang::x86_64::{op, Emitter, RelocKind, SYSV};
///
/// let mut f = Function::new("f", &SYSV);
/// f.omit_frame_pointer();
/// let b = f.block()?;
/// let puts = f.symbol("puts")?;
/// f.push(b, op::CALL, &[puts.into()])?;
/// f.push(b, op::RET, &[])?;
/// let mut e = Emitter::new();
/// e.declare_external("puts")?;
/// e.function(&f)?;
/// let p = e.finish()?;
/// let r = &p.relocations()[0];
/// assert_eq!((r.kind, r.symbol.as_str(), r.addend), (RelocKind::Call32, "puts", -4));
/// // The field follows the `call` opcode and holds zero until linked.
/// assert_eq!(p.bytes()[r.offset - 1..r.offset + 4], [0xE8, 0, 0, 0, 0]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Reloc {
    /// Where the patched bytes start, from the start of the code.
    pub offset: usize,
    /// What is computed.
    pub kind: RelocKind,
    /// The symbol whose address is used.
    pub symbol: String,
    /// The constant added to the address.
    pub addend: i64,
}

impl Reloc {
    /// A relocation at `offset` of `kind` against `symbol` plus `addend`.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{Reloc, RelocKind};
    ///
    /// let r = Reloc::new(8, RelocKind::Abs64, "table", 16);
    /// assert_eq!(r.addend, 16);
    /// ```
    #[must_use]
    pub fn new(offset: usize, kind: RelocKind, symbol: &str, addend: i64) -> Reloc {
        Reloc {
            offset,
            kind,
            symbol: String::from(symbol),
            addend,
        }
    }
}

/// One call site with a record ([`Function::set_call_site`]): where it
/// returns to, where its GC roots are while it runs, and where unwinding
/// lands.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::{Function, SlotKind};
/// use backend_lang::x86_64::{op, Emitter, SYSV};
///
/// let mut f = Function::new("f", &SYSV);
/// let root = f.stack_slot(SlotKind::Local { size: 8, align: 8 })?;
/// let b = f.block()?;
/// let g = f.symbol("f")?;
/// let call = f.push(b, op::CALL, &[g.into()])?;
/// f.push(b, op::RET, &[])?;
/// f.set_call_site(call, &[root], None)?;
/// let mut e = Emitter::new();
/// e.function(&f)?;
/// let p = e.finish()?;
/// let site = &p.functions()[0].call_sites()[0];
/// assert_eq!(site.gc_slots().len(), 1);
/// assert!(site.return_offset() > 0);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallSiteRecord {
    return_offset: usize,
    gc_slots: Vec<i32>,
    landing_pad: Option<usize>,
}

impl CallSiteRecord {
    /// The offset of the instruction after the call (the return address),
    /// from the start of the code. A stack walker identifies the call by it.
    ///
    /// # Examples
    ///
    /// See [`CallSiteRecord`].
    #[inline]
    #[must_use]
    pub fn return_offset(&self) -> usize {
        self.return_offset
    }

    /// Where the GC roots live while the call runs: byte offsets from the
    /// caller's stack pointer at the call instruction (before the return
    /// address is pushed), one 8-byte reference each. This is the call's
    /// **stack map**: a collector reads the references there and writes the
    /// relocated ones back, and the caller reloads them after the call.
    ///
    /// # Examples
    ///
    /// See [`CallSiteRecord`].
    #[inline]
    #[must_use]
    pub fn gc_slots(&self) -> &[i32] {
        &self.gc_slots
    }

    /// The offset of the landing pad unwinding through this call reaches, if
    /// any.
    ///
    /// # Examples
    ///
    /// See [`CallSiteRecord`].
    #[inline]
    #[must_use]
    pub fn landing_pad(&self) -> Option<usize> {
        self.landing_pad
    }
}

/// One emitted function: its place in the code, its frame, and its call-site
/// records.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::Function;
/// use backend_lang::x86_64::{op, Emitter, SYSV};
///
/// let mut f = Function::new("f", &SYSV);
/// let b = f.block()?;
/// f.push(b, op::RET, &[])?;
/// let mut e = Emitter::new();
/// e.function(&f)?;
/// let p = e.finish()?;
/// let ef = &p.functions()[0];
/// assert_eq!((ef.name(), ef.offset(), ef.size()), ("f", 0, 6));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmittedFunction {
    name: String,
    offset: usize,
    size: usize,
    frame: FrameLayout,
    call_sites: Vec<CallSiteRecord>,
}

impl EmittedFunction {
    /// The function's name.
    ///
    /// # Examples
    ///
    /// See [`EmittedFunction`].
    #[inline]
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The offset of its first byte.
    ///
    /// # Examples
    ///
    /// See [`EmittedFunction`].
    #[inline]
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Its size in bytes, jump tables included.
    ///
    /// # Examples
    ///
    /// See [`EmittedFunction`].
    #[inline]
    #[must_use]
    pub fn size(&self) -> usize {
        self.size
    }

    /// Its frame. With the stack pointer fixed in the body, the canonical
    /// frame address is the body's stack pointer plus
    /// [`cfa_offset`](FrameLayout::cfa_offset), which is all a stack walker
    /// needs to step from a call site to its caller.
    ///
    /// # Examples
    ///
    /// See [`EmittedFunction`].
    #[inline]
    #[must_use]
    pub fn frame(&self) -> &FrameLayout {
        &self.frame
    }

    /// Its call-site records, in code order.
    ///
    /// # Examples
    ///
    /// See [`CallSiteRecord`].
    #[inline]
    #[must_use]
    pub fn call_sites(&self) -> &[CallSiteRecord] {
        &self.call_sites
    }
}

/// Machine code with the offset of each function, the relocations against
/// external symbols, and the call-site records.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::Function;
/// use backend_lang::x86_64::{op, Emitter, SYSV};
///
/// let mut f = Function::new("f", &SYSV);
/// let b = f.block()?;
/// f.push(b, op::RET, &[])?;
/// let mut e = Emitter::new();
/// e.function(&f)?;
/// let program = e.finish()?;
/// assert_eq!(program.symbol("f"), Some(0));
/// // push rbp; mov rbp, rsp; pop rbp; ret
/// assert_eq!(program.bytes(), [0x55, 0x48, 0x89, 0xE5, 0x5D, 0xC3]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    bytes: Vec<u8>,
    symbols: Vec<(String, usize)>,
    functions: Vec<EmittedFunction>,
    relocs: Vec<Reloc>,
}

impl Program {
    /// The machine code.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::Emitter;
    ///
    /// assert!(Emitter::new().finish()?.bytes().is_empty());
    /// # Ok::<(), backend_lang::x86_64::EmitError>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The machine code, by value.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::Emitter;
    ///
    /// assert!(Emitter::new().finish()?.into_bytes().is_empty());
    /// # Ok::<(), backend_lang::x86_64::EmitError>(())
    /// ```
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// The offset of the function named `name`.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::Emitter;
    ///
    /// assert_eq!(Emitter::new().finish()?.symbol("missing"), None);
    /// # Ok::<(), backend_lang::x86_64::EmitError>(())
    /// ```
    #[must_use]
    pub fn symbol(&self, name: &str) -> Option<usize> {
        self.symbols
            .binary_search_by(|(n, _)| n.as_str().cmp(name))
            .ok()
            .and_then(|i| self.symbols.get(i))
            .map(|&(_, off)| off)
    }

    /// Every function and its offset, in name order.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::Emitter;
    ///
    /// assert_eq!(Emitter::new().finish()?.symbols().count(), 0);
    /// # Ok::<(), backend_lang::x86_64::EmitError>(())
    /// ```
    pub fn symbols(&self) -> impl Iterator<Item = (&str, usize)> {
        self.symbols.iter().map(|(n, o)| (n.as_str(), *o))
    }

    /// Every function, in emission order, with its size, frame, and
    /// call-site records.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::Emitter;
    ///
    /// assert!(Emitter::new().finish()?.functions().is_empty());
    /// # Ok::<(), backend_lang::x86_64::EmitError>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn functions(&self) -> &[EmittedFunction] {
        &self.functions
    }

    /// The relocations against external symbols, in code order.
    ///
    /// # Examples
    ///
    /// See [`Reloc`].
    #[inline]
    #[must_use]
    pub fn relocations(&self) -> &[Reloc] {
        &self.relocs
    }
}

#[derive(Clone, Copy, Debug)]
struct Sym {
    label: Label,
    defined: bool,
    external: bool,
}

/// A function emitted but not yet finished: its labels and record ranges.
#[derive(Clone, Debug)]
struct PendingFunc {
    name: String,
    start: Label,
    end: Label,
    frame: FrameLayout,
    sites: core::ops::Range<usize>,
}

/// A call site whose return address is a label bound after the call.
#[derive(Clone, Debug)]
struct PendingSite {
    ret: Label,
    gc: core::ops::Range<usize>,
    landing: Option<Label>,
}

/// A reference to an external symbol: the patched field ends `back` bytes
/// before the label bound right after the instruction (the field is always
/// the instruction's last four bytes).
#[derive(Clone, Debug)]
struct PendingReloc {
    after: Label,
    kind: RelocKind,
    symbol: String,
}

/// A jump table: eight-byte entries at `at`, each the offset of a target
/// from the table, filled in once the layout is final.
#[derive(Clone, Debug)]
struct PendingTable {
    at: Label,
    targets: core::ops::Range<usize>,
}

/// Emits functions into one buffer of x86-64 machine code.
///
/// # Examples
///
/// Two functions, one calling the other:
///
/// ```
/// use backend_lang::mir::{Effects, Function, Operand};
/// use backend_lang::x86_64::{op, Emitter, SYSV, RAX};
///
/// let mut answer = Function::new("answer", &SYSV);
/// let b = answer.block()?;
/// answer.push(b, op::MOV32RI, &[RAX.into(), Operand::Imm(42)])?;
/// answer.push_with(b, op::RET, &[], Effects::new().uses(&[RAX]))?;
///
/// let mut main = Function::new("main", &SYSV);
/// let b = main.block()?;
/// let callee = main.symbol("answer")?;
/// main.push_with(b, op::CALL, &[callee.into()], Effects::new().defs(&[RAX]).clobbers(SYSV.call_clobbers()))?;
/// main.push_with(b, op::RET, &[], Effects::new().uses(&[RAX]))?;
///
/// let mut e = Emitter::new();
/// e.function(&main)?;
/// e.function(&answer)?;
/// let program = e.finish()?;
/// assert_eq!(program.symbol("main"), Some(0));
/// assert!(program.symbol("answer").is_some());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Default)]
pub struct Emitter {
    asm: Assembler,
    symbols: BTreeMap<String, Sym>,
    resolver: MoveResolver,
    moves: Vec<Move>,
    steps: Vec<Step>,
    sites: Vec<(u32, u32)>,
    labels: Vec<Label>,
    funcs: Vec<PendingFunc>,
    call_sites: Vec<PendingSite>,
    gc_offsets: Vec<i32>,
    relocs: Vec<PendingReloc>,
    tables: Vec<PendingTable>,
    table_targets: Vec<Label>,
}

/// Per-function state while emitting its body.
struct Ctx<'a> {
    f: &'a Function,
    frame: &'a FrameLayout,
    site: usize,
    tables: usize,
}
impl Emitter {
    /// An empty emitter.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::Emitter;
    ///
    /// assert!(Emitter::new().finish()?.bytes().is_empty());
    /// # Ok::<(), backend_lang::x86_64::EmitError>(())
    /// ```
    #[must_use]
    pub fn new() -> Emitter {
        Emitter::default()
    }

    /// An empty emitter with room for `bytes` bytes of code.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::Emitter;
    ///
    /// let _e = Emitter::with_capacity(1 << 20);
    /// ```
    #[must_use]
    pub fn with_capacity(bytes: usize) -> Emitter {
        Emitter {
            asm: Assembler::with_capacity(bytes),
            ..Emitter::default()
        }
    }

    /// Emits one function: verifies it, checks it uses only physical
    /// registers, lays out its frame, and encodes its prologue, blocks, and
    /// epilogues.
    ///
    /// # Errors
    ///
    /// An [`EmitError`] naming the problem. After an error the emitter may
    /// hold part of the function; discard it.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, Operand};
    /// use backend_lang::x86_64::{op, Emitter, WIN64, RAX};
    ///
    /// let mut f = Function::new("seven", &WIN64);
    /// let b = f.block()?;
    /// f.push(b, op::MOV32RI, &[RAX.into(), Operand::Imm(7)])?;
    /// f.push(b, op::RET, &[])?;
    /// let mut e = Emitter::new();
    /// e.function(&f)?;
    /// // push rbp; mov rbp, rsp; mov eax, 7; pop rbp; ret
    /// assert_eq!(e.finish()?.bytes(), [0x55, 0x48, 0x89, 0xE5, 0xB8, 7, 0, 0, 0, 0x5D, 0xC3]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn function(&mut self, f: &Function) -> Result<FunctionInfo, EmitError> {
        let target = f.target();
        if !core::ptr::eq(target.insts(), &DESCS[..]) || !core::ptr::eq(target.regs(), &REGS[..]) {
            return Err(EmitError::UnsupportedTarget);
        }
        f.verify()?;
        check_physical(f)?;

        // Resolve every parallel copy first: the frame needs to know whether
        // any of them borrows the hidden slots.
        let base = f.slots().len();
        let scratch = StackSlot::from_index(base);
        let victim = StackSlot::from_index(base + 1);
        let env = MoveEnv::new()
            .with_scratch_slot(scratch)
            .with_victim_slot(victim);
        let (mut need_scratch, mut need_victim) = (false, false);
        self.steps.clear();
        self.sites.clear();
        for block in f.blocks() {
            for &inst in f.insts(block) {
                let Some(opcode) = f.opcode(inst) else {
                    continue;
                };
                let ops = f.operands(inst);
                let mut site =
                    |em: &mut Emitter, pairs: &mut dyn Iterator<Item = (Operand, Operand)>| {
                        em.moves.clear();
                        for (d, s) in pairs {
                            em.moves.push(to_move(f, d, s)?);
                        }
                        let start = em.steps.len();
                        let used = em
                            .resolver
                            .resolve(target, &em.moves, &env, &mut em.steps)?;
                        need_scratch |= used.scratch_slot;
                        need_victim |= used.victim_slot;
                        em.sites.push((start as u32, em.steps.len() as u32));
                        Ok::<(), EmitError>(())
                    };
                match ENCODINGS.get(opcode.index()) {
                    Some(Enc::Copy) => {
                        site(
                            self,
                            &mut ops.first().copied().zip(ops.get(1).copied()).into_iter(),
                        )?;
                    }
                    Some(Enc::Pcopy) => {
                        let (d, s) = ops.split_at(ops.len() / 2);
                        site(self, &mut d.iter().copied().zip(s.iter().copied()))?;
                    }
                    Some(Enc::Jmp | Enc::Jcc) => {
                        for op in ops {
                            if let Operand::Block(call) = op {
                                let params = f.params(call.block()).iter().copied();
                                site(self, &mut params.zip(f.args(*call).iter().copied()))?;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        let mut slots: Vec<SlotKind> = Vec::with_capacity(base + 2);
        slots.extend_from_slice(f.slots());
        for used in [need_scratch, need_victim] {
            slots.push(if used {
                SlotKind::Local {
                    size: 16,
                    align: 16,
                }
            } else {
                SlotKind::Local { size: 0, align: 1 }
            });
        }
        let frame = f.abi().layout_frame(&FrameInput {
            slots: &slots,
            written: f.written_regs(),
            has_calls: f.has_calls(),
            outgoing: f.outgoing(),
            frame_pointer: f.frame_pointer(),
        })?;

        let entry = self.define(f.name())?;
        self.asm.bind(entry).map_err(EmitError::Asm)?;
        let probe_reg = probe_scratch(f)?;
        for &step in frame.prologue() {
            self.frame_step(step, probe_reg)?;
        }
        self.labels.clear();
        for _ in 0..f.block_count() {
            let l = self.asm.label();
            self.labels.push(l);
        }
        let first_site = self.call_sites.len();
        let first_table = self.tables.len();
        let mut ctx = Ctx {
            f,
            frame: &frame,
            site: 0,
            tables: 0,
        };
        for (bi, block) in f.blocks().enumerate() {
            let label = self.labels[bi];
            self.asm.bind(label).map_err(EmitError::Asm)?;
            for &inst in f.insts(block) {
                self.inst(&mut ctx, bi, inst)?;
            }
        }
        // Jump tables follow the code, zero-filled until the layout is final.
        for t in first_table..self.tables.len() {
            let (at, n) = match self.tables.get(t) {
                Some(p) => (p.at, p.targets.len()),
                None => continue,
            };
            self.asm.bind(at).map_err(EmitError::Asm)?;
            let zeros = [0u8; 64];
            let mut left = n.saturating_mul(8);
            while left > 0 {
                let k = left.min(zeros.len());
                self.asm
                    .bytes(&zeros[..k])
                    .map_err(|e| asm_error(None, e))?;
                left -= k;
            }
        }
        let end = self.asm.label();
        self.asm.bind(end).map_err(EmitError::Asm)?;
        self.funcs.push(PendingFunc {
            name: f.name().to_string(),
            start: entry,
            end,
            frame: frame.clone(),
            sites: first_site..self.call_sites.len(),
        });
        Ok(FunctionInfo { frame })
    }

    /// Declares `name` as a symbol defined outside this code (an imported
    /// function, or data such as a global): every call, tail call, or address
    /// of it becomes a [`Reloc`] in [`Program::relocations`] instead of an
    /// error at [`finish`](Emitter::finish).
    ///
    /// # Errors
    ///
    /// [`EmitError::ExternalConflict`] if a function of that name was already
    /// emitted.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::{op, Emitter, SYSV};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// let g = f.symbol("g")?;
    /// f.push(b, op::TAILCALL, &[g.into()])?;
    /// let mut e = Emitter::new();
    /// e.declare_external("g")?;
    /// e.function(&f)?;
    /// assert_eq!(e.finish()?.relocations().len(), 1);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn declare_external(&mut self, name: &str) -> Result<(), EmitError> {
        let _ = self.label_for(name);
        match self.symbols.get_mut(name) {
            Some(s) if s.defined => Err(EmitError::ExternalConflict(name.to_string())),
            Some(s) => {
                s.external = true;
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Finishes the code: relaxes branches, patches calls and jump tables,
    /// and returns the bytes with every function's offset, the relocations
    /// against external symbols, and the call-site records.
    ///
    /// # Errors
    ///
    /// [`EmitError::UndefinedSymbol`] for the first name (in name order)
    /// that was called but neither emitted nor declared external;
    /// [`EmitError::Asm`] if the code exceeds 2 GiB.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::{op, EmitError, Emitter, SYSV};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// let g = f.symbol("g")?;
    /// f.push(b, op::CALL, &[g.into()])?;
    /// f.push(b, op::RET, &[])?;
    /// let mut e = Emitter::new();
    /// e.function(&f)?;
    /// assert_eq!(e.finish().unwrap_err(), EmitError::UndefinedSymbol("g".into()));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn finish(self) -> Result<Program, EmitError> {
        if let Some((name, _)) = self.symbols.iter().find(|(_, s)| !s.defined && !s.external) {
            return Err(EmitError::UndefinedSymbol(name.clone()));
        }
        let code = self.asm.finish().map_err(EmitError::Asm)?;
        let at = |l: Label| {
            code.label_offset(l)
                .ok_or(EmitError::Asm(AsmError::Unbound(l)))
        };
        let symbols = self
            .symbols
            .iter()
            .filter(|(_, s)| s.defined)
            .filter_map(|(n, s)| code.label_offset(s.label).map(|o| (n.clone(), o)))
            .collect();
        let mut relocs = Vec::with_capacity(self.relocs.len());
        for r in &self.relocs {
            // The field is the last four bytes of its instruction, and the
            // processor measures from the end of the instruction.
            let end = at(r.after)?;
            relocs.push(Reloc {
                offset: end.saturating_sub(4),
                kind: r.kind,
                symbol: r.symbol.clone(),
                addend: -4,
            });
        }
        let mut patches: Vec<(usize, i64)> = Vec::new();
        for t in &self.tables {
            let base = at(t.at)?;
            for (k, &target) in self
                .table_targets
                .get(t.targets.clone())
                .unwrap_or(&[])
                .iter()
                .enumerate()
            {
                let to = at(target)?;
                patches.push((base + 8 * k, to as i64 - base as i64));
            }
        }
        let mut functions = Vec::with_capacity(self.funcs.len());
        for pf in &self.funcs {
            let offset = at(pf.start)?;
            let size = at(pf.end)?.saturating_sub(offset);
            let mut call_sites = Vec::with_capacity(pf.sites.len());
            for ps in self.call_sites.get(pf.sites.clone()).unwrap_or(&[]) {
                call_sites.push(CallSiteRecord {
                    return_offset: at(ps.ret)?,
                    gc_slots: self.gc_offsets.get(ps.gc.clone()).unwrap_or(&[]).to_vec(),
                    landing_pad: match ps.landing {
                        Some(l) => Some(at(l)?),
                        None => None,
                    },
                });
            }
            functions.push(EmittedFunction {
                name: pf.name.clone(),
                offset,
                size,
                frame: pf.frame.clone(),
                call_sites,
            });
        }
        let mut bytes = code.into_bytes();
        for (pos, value) in patches {
            if let Some(field) = bytes.get_mut(pos..pos + 8) {
                field.copy_from_slice(&value.to_le_bytes());
            }
        }
        Ok(Program {
            bytes,
            symbols,
            functions,
            relocs,
        })
    }

    fn label_for(&mut self, name: &str) -> Label {
        if let Some(s) = self.symbols.get(name) {
            return s.label;
        }
        let label = self.asm.label();
        let _ = self.symbols.insert(
            name.to_string(),
            Sym {
                label,
                defined: false,
                external: false,
            },
        );
        label
    }

    fn define(&mut self, name: &str) -> Result<Label, EmitError> {
        let label = self.label_for(name);
        match self.symbols.get_mut(name) {
            Some(s) if s.external => Err(EmitError::ExternalConflict(name.to_string())),
            Some(s) if s.defined => Err(EmitError::DuplicateSymbol(name.to_string())),
            Some(s) => {
                s.defined = true;
                Ok(label)
            }
            None => Ok(label),
        }
    }

    fn is_external(&self, name: &str) -> bool {
        self.symbols.get(name).is_some_and(|s| s.external)
    }

    /// Emits already-encoded bytes that end in a four-byte field referring to
    /// an external symbol, and records the relocation.
    fn external_ref(
        &mut self,
        inst: Option<Inst>,
        bytes: &[u8],
        kind: RelocKind,
        symbol: &str,
    ) -> Result<(), EmitError> {
        self.asm.bytes(bytes).map_err(|e| asm_error(inst, e))?;
        let after = self.asm.label();
        self.asm.bind(after).map_err(EmitError::Asm)?;
        self.relocs.push(PendingReloc {
            after,
            kind,
            symbol: symbol.to_string(),
        });
        Ok(())
    }

    /// `call name` or `jmp name`: to a label when the name is defined here,
    /// through a relocation when it is external.
    fn branch_to_symbol(&mut self, inst: Inst, name: &str, jump: bool) -> Result<(), EmitError> {
        if self.is_external(name) {
            let opcode = if jump { 0xE9 } else { 0xE8 };
            return self.external_ref(Some(inst), &[opcode, 0, 0, 0, 0], RelocKind::Call32, name);
        }
        let label = self.label_for(name);
        if jump {
            self.asm.jmp(label).map_err(|e| asm_error(Some(inst), e))
        } else {
            self.asm.call(label).map_err(|e| asm_error(Some(inst), e))
        }
    }

    fn emit(&mut self, inst: Option<Inst>, i: &IInst) -> Result<(), EmitError> {
        self.asm.emit(i).map_err(|e| asm_error(inst, e))
    }
    fn frame_step(&mut self, step: FrameStep, probe_reg: PReg) -> Result<(), EmitError> {
        let rsp = IOp::Reg(IReg::RSP);
        match step {
            FrameStep::Push(r) => {
                self.emit(None, &IInst::new(M::Push, [ireg(None, r, Size::Qword)?]))
            }
            FrameStep::Pop(r) => {
                self.emit(None, &IInst::new(M::Pop, [ireg(None, r, Size::Qword)?]))
            }
            FrameStep::SetFramePointer => {
                self.emit(None, &IInst::new(M::Mov, [IOp::Reg(IReg::RBP), rsp]))
            }
            FrameStep::Alloc { bytes, probe } if probe == 0 || bytes < probe => {
                self.emit(None, &IInst::new(M::Sub, [rsp, IOp::Imm(i64::from(bytes))]))
            }
            FrameStep::Alloc { bytes, probe } => self.probe(bytes, probe, probe_reg),
            FrameStep::Free(bytes) => {
                self.emit(None, &IInst::new(M::Add, [rsp, IOp::Imm(i64::from(bytes))]))
            }
            _ => Err(EmitError::Unencodable { inst: None }),
        }
    }

    /// Allocates `bytes` one `page` at a time, touching each new page, so the
    /// guard page below the stack is never skipped. Up to four pages are
    /// unrolled; beyond that a counted loop runs in `counter` (a
    /// caller-saved register the convention does not pass arguments in).
    fn probe(&mut self, bytes: u32, page: u32, counter: PReg) -> Result<(), EmitError> {
        let rsp = IOp::Reg(IReg::RSP);
        let touch = IInst::new(M::Test, [IMem::new(Size::Qword, IReg::RSP).into(), rsp]);
        let step = IInst::new(M::Sub, [rsp, IOp::Imm(i64::from(page))]);
        let pages = bytes / page;
        let rest = bytes % page;
        if pages <= 4 {
            for _ in 0..pages {
                self.emit(None, &step)?;
                self.emit(None, &touch)?;
            }
        } else {
            let c = ireg(None, counter, Size::Dword)?;
            self.emit(None, &IInst::new(M::Mov, [c, IOp::Imm(i64::from(pages))]))?;
            let top = self.asm.label();
            self.asm.bind(top).map_err(EmitError::Asm)?;
            self.emit(None, &step)?;
            self.emit(None, &touch)?;
            self.emit(None, &IInst::new(M::Dec, [c]))?;
            self.asm.jcc(ICond::Ne, top).map_err(EmitError::Asm)?;
        }
        if rest > 0 {
            self.emit(None, &IInst::new(M::Sub, [rsp, IOp::Imm(i64::from(rest))]))?;
        }
        Ok(())
    }

    /// Emits the steps of the next parallel-copy site.
    fn site(&mut self, ctx: &mut Ctx<'_>) -> Result<bool, EmitError> {
        let (start, end) = self.sites.get(ctx.site).copied().unwrap_or((0, 0));
        ctx.site += 1;
        for i in start as usize..end as usize {
            let step = self.steps[i];
            self.step(ctx.frame, step)?;
        }
        Ok(end > start)
    }

    fn next_site_empty(&self, ctx: &Ctx<'_>, ahead: usize) -> bool {
        self.sites
            .get(ctx.site + ahead)
            .is_none_or(|&(s, e)| s == e)
    }

    fn step(&mut self, frame: &FrameLayout, step: Step) -> Result<(), EmitError> {
        let gpr = |class: RegClass| {
            if class == GPR {
                Ok(())
            } else {
                Err(EmitError::Unencodable { inst: None })
            }
        };
        match step {
            Step::Swap { a, b, class } => {
                gpr(class)?;
                let i = IInst::new(
                    M::Xchg,
                    [ireg(None, a, Size::Qword)?, ireg(None, b, Size::Qword)?],
                );
                self.emit(None, &i)
            }
            Step::Move { dst, src, class } => {
                gpr(class)?;
                let i = match (dst, src) {
                    (Loc::Reg(d), Source::Imm(v)) => return self.mov_imm(None, d, v),
                    (Loc::Reg(d), Source::Loc(Loc::Reg(s))) => IInst::new(
                        M::Mov,
                        [ireg(None, d, Size::Qword)?, ireg(None, s, Size::Qword)?],
                    ),
                    (Loc::Reg(d), Source::Loc(Loc::Slot(s))) => IInst::new(
                        M::Mov,
                        [
                            ireg(None, d, Size::Qword)?,
                            slot_mem(frame, s, 0, Size::Qword)?.into(),
                        ],
                    ),
                    (Loc::Slot(d), Source::Loc(Loc::Reg(s))) => IInst::new(
                        M::Mov,
                        [
                            slot_mem(frame, d, 0, Size::Qword)?.into(),
                            ireg(None, s, Size::Qword)?,
                        ],
                    ),
                    (Loc::Slot(d), Source::Imm(v)) => IInst::new(
                        M::Mov,
                        [slot_mem(frame, d, 0, Size::Qword)?.into(), IOp::Imm(v)],
                    ),
                    // The resolver legalizes memory-to-memory moves away.
                    (Loc::Slot(_), Source::Loc(Loc::Slot(_))) => {
                        return Err(EmitError::Unencodable { inst: None });
                    }
                };
                self.emit(None, &i)
            }
        }
    }

    /// `mov r64, imm`, as `mov r32, imm32` when the value is below 2^32 (the
    /// upper half is zeroed either way).
    fn mov_imm(&mut self, inst: Option<Inst>, reg: PReg, value: i64) -> Result<(), EmitError> {
        let i = if (0..=i64::from(u32::MAX)).contains(&value) {
            IInst::new(
                M::Mov,
                [
                    ireg(inst, reg, Size::Dword)?,
                    IOp::Imm(i64::from(value as u32 as i32)),
                ],
            )
        } else {
            IInst::new(M::Mov, [ireg(inst, reg, Size::Qword)?, IOp::Imm(value)])
        };
        self.emit(inst, &i)
    }

    fn inst(&mut self, ctx: &mut Ctx<'_>, bi: usize, inst: Inst) -> Result<(), EmitError> {
        let f = ctx.f;
        let Some(opcode) = f.opcode(inst) else {
            return Ok(());
        };
        let ops = f.operands(inst);
        let enc = ENCODINGS
            .get(opcode.index())
            .copied()
            .ok_or(EmitError::UnsupportedTarget)?;
        let next = Block::from_index(bi + 1);
        match enc {
            Enc::Copy | Enc::Pcopy => {
                let _ = self.site(ctx)?;
                Ok(())
            }
            Enc::Jmp => {
                let call = block_operand(ops, 0, inst)?;
                let _ = self.site(ctx)?;
                if call.block() != next {
                    self.jmp(call.block())?;
                }
                Ok(())
            }
            Enc::Jcc => {
                let cond = cond_operand(ops, 0, inst)?;
                let then = block_operand(ops, 1, inst)?;
                let other = block_operand(ops, 2, inst)?;
                let then_moves = !self.next_site_empty(ctx, 0);
                let else_moves = !self.next_site_empty(ctx, 1);
                if !then_moves {
                    self.jcc(cond, then.block())?;
                    let _ = self.site(ctx)?;
                    let _ = self.site(ctx)?;
                } else if !else_moves {
                    self.jcc(cond.negate(), other.block())?;
                    let _ = self.site(ctx)?;
                    if then.block() != next {
                        self.jmp(then.block())?;
                    }
                    let _ = self.site(ctx)?;
                    return Ok(());
                } else {
                    let stub = self.asm.label();
                    self.asm.jcc(cond.negate(), stub).map_err(EmitError::Asm)?;
                    let _ = self.site(ctx)?;
                    self.jmp(then.block())?;
                    self.asm.bind(stub).map_err(EmitError::Asm)?;
                    let _ = self.site(ctx)?;
                }
                if other.block() != next {
                    self.jmp(other.block())?;
                }
                Ok(())
            }
            Enc::Ret => {
                for &step in ctx.frame.epilogue() {
                    self.frame_step(step, R11)?;
                }
                self.emit(Some(inst), &IInst::new(M::Ret, []))
            }
            Enc::TailCall => {
                // The target register must outlive the epilogue's pops.
                let target = match ops.first() {
                    Some(Operand::Reg(r)) => {
                        let p = r.as_phys().ok_or(EmitError::VirtualRegister {
                            block: Block::from_index(bi),
                            inst: Some(inst),
                        })?;
                        if ctx.frame.pushed().contains(&p)
                            || (ctx.frame.frame_pointer() && p == super::RBP)
                        {
                            return Err(EmitError::Constraint { inst });
                        }
                        Some(ireg(Some(inst), p, Size::Qword)?)
                    }
                    _ => None,
                };
                for &step in ctx.frame.epilogue() {
                    self.frame_step(step, R11)?;
                }
                match (target, ops.first()) {
                    (Some(t), _) => self.emit(Some(inst), &IInst::new(M::Jmp, [t])),
                    (None, Some(Operand::Symbol(s))) => {
                        let name = f
                            .symbol_name(*s)
                            .ok_or(EmitError::Mir(MirError::UnknownSymbol(*s)))?;
                        self.branch_to_symbol(inst, name, true)
                    }
                    _ => Err(EmitError::Mir(MirError::OperandShape { inst, index: 0 })),
                }
            }
            Enc::JumpTable => {
                let reg = |index: usize| -> Result<IReg, EmitError> {
                    match ops.get(index) {
                        Some(Operand::Reg(r)) => qword(inst, *r),
                        _ => Err(EmitError::Mir(MirError::OperandShape { inst, index })),
                    }
                };
                let (t, i) = (reg(0)?, reg(1)?);
                if t == i {
                    return Err(EmitError::Constraint { inst });
                }
                let table = self.asm.label();
                let start = self.table_targets.len();
                for (index, op) in ops.iter().enumerate().skip(2) {
                    let b = match op {
                        Operand::Block(call) => call.block(),
                        _ => return Err(EmitError::Mir(MirError::OperandShape { inst, index })),
                    };
                    let l = self
                        .labels
                        .get(b.index())
                        .copied()
                        .ok_or(EmitError::Mir(MirError::UnknownBlock(b)))?;
                    self.table_targets.push(l);
                }
                self.tables.push(PendingTable {
                    at: table,
                    targets: start..self.table_targets.len(),
                });
                ctx.tables += 1;
                // t = &table; t += table[i] (the target's offset from the
                // table); jmp t. Position-independent: no absolute address.
                let lea = IInst::new(M::Lea, [IOp::Reg(t), IOp::Mem(IMem::rip(Size::Qword, 0))]);
                self.asm
                    .emit_rip(&lea, table)
                    .map_err(|e| asm_error(Some(inst), e))?;
                let entry = IMem::new(Size::Qword, t).with_index(i, IScale::X8);
                self.emit(
                    Some(inst),
                    &IInst::new(M::Add, [IOp::Reg(t), IOp::Mem(entry)]),
                )?;
                self.emit(Some(inst), &IInst::new(M::Jmp, [IOp::Reg(t)]))
            }
            Enc::Call => {
                match ops.first() {
                    Some(Operand::Symbol(s)) => {
                        let name = f
                            .symbol_name(*s)
                            .ok_or(EmitError::Mir(MirError::UnknownSymbol(*s)))?;
                        self.branch_to_symbol(inst, name, false)?;
                    }
                    Some(op) => {
                        let target = self.operand(ctx, inst, op, Size::Qword)?;
                        self.emit(Some(inst), &IInst::new(M::Call, [target]))?;
                    }
                    None => return Err(EmitError::Mir(MirError::OperandShape { inst, index: 0 })),
                }
                if let Some(site) = f.call_site(inst) {
                    // The return address identifies the call to a stack
                    // walker: bind it right after the call.
                    let ret = self.asm.label();
                    self.asm.bind(ret).map_err(EmitError::Asm)?;
                    let gc_start = self.gc_offsets.len();
                    for &slot in site.gc_slots() {
                        let off = ctx
                            .frame
                            .slot_offset(slot)
                            .ok_or(EmitError::Mir(MirError::UnknownSlot(slot)))?;
                        self.gc_offsets.push(off);
                    }
                    let landing = match site.landing_pad() {
                        Some(b) => Some(
                            self.labels
                                .get(b.index())
                                .copied()
                                .ok_or(EmitError::Mir(MirError::UnknownBlock(b)))?,
                        ),
                        None => None,
                    };
                    self.call_sites.push(PendingSite {
                        ret,
                        gc: gc_start..self.gc_offsets.len(),
                        landing,
                    });
                }
                Ok(())
            }
            Enc::MovImm64 => match (
                ops.first().and_then(Operand::reg).and_then(Reg::as_phys),
                ops.get(1),
            ) {
                (Some(d), Some(Operand::Imm(v))) => self.mov_imm(Some(inst), d, *v),
                _ => Err(EmitError::Mir(MirError::OperandShape { inst, index: 0 })),
            },
            Enc::Raw(bytes) => self.asm.bytes(bytes).map_err(|e| asm_error(Some(inst), e)),
            Enc::Isa {
                fam,
                ops: srcs,
                cond,
            } => {
                let mnemonic = match fam {
                    Fam::Plain(m) => m,
                    Fam::Setcc => M::Setcc(cond_operand(ops, usize::from(cond), inst)?),
                    Fam::Cmovcc => M::Cmovcc(cond_operand(ops, usize::from(cond), inst)?),
                };
                let mut out = [IOp::Imm(0); 3];
                let mut rip = None;
                for (k, src) in srcs.iter().enumerate().take(3) {
                    out[k] = match *src {
                        Src::Fixed(r) => IOp::Reg(r),
                        Src::Op(n, size) => match ops.get(usize::from(n)) {
                            Some(Operand::Symbol(s)) => {
                                rip = Some(*s);
                                IOp::Mem(IMem::rip(size, 0))
                            }
                            Some(op) => self.operand(ctx, inst, op, size)?,
                            None => {
                                return Err(EmitError::Mir(MirError::OperandShape {
                                    inst,
                                    index: usize::from(n),
                                }));
                            }
                        },
                    };
                }
                let i = IInst::from_slice(mnemonic, &out[..srcs.len().min(3)])
                    .ok_or(EmitError::Unencodable { inst: Some(inst) })?;
                match rip {
                    Some(s) => {
                        let name = f
                            .symbol_name(s)
                            .ok_or(EmitError::Mir(MirError::UnknownSymbol(s)))?;
                        if self.is_external(name) {
                            // A RIP-relative address of an external symbol:
                            // no row with a symbol operand has an immediate,
                            // so the displacement is the last four bytes.
                            let encoded = isa_lang::x86_64::encode(&i).map_err(|error| {
                                EmitError::Encode {
                                    inst: Some(inst),
                                    error,
                                }
                            })?;
                            return self.external_ref(
                                Some(inst),
                                encoded.as_bytes(),
                                RelocKind::Pc32,
                                name,
                            );
                        }
                        let label = self.label_for(name);
                        self.asm
                            .emit_rip(&i, label)
                            .map_err(|e| asm_error(Some(inst), e))
                    }
                    None => self.emit(Some(inst), &i),
                }
            }
        }
    }

    fn jmp(&mut self, block: Block) -> Result<(), EmitError> {
        let label = self
            .labels
            .get(block.index())
            .copied()
            .ok_or(EmitError::Mir(MirError::UnknownBlock(block)))?;
        self.asm.jmp(label).map_err(EmitError::Asm)
    }

    fn jcc(&mut self, cond: ICond, block: Block) -> Result<(), EmitError> {
        let label = self
            .labels
            .get(block.index())
            .copied()
            .ok_or(EmitError::Mir(MirError::UnknownBlock(block)))?;
        self.asm.jcc(cond, label).map_err(EmitError::Asm)
    }

    fn operand(
        &self,
        ctx: &Ctx<'_>,
        inst: Inst,
        op: &Operand,
        size: Size,
    ) -> Result<IOp, EmitError> {
        Ok(match *op {
            Operand::Reg(r) => {
                let p = r.as_phys().ok_or(EmitError::VirtualRegister {
                    block: ctx.f.inst_block(inst).unwrap_or(Block::new(0)),
                    inst: Some(inst),
                })?;
                ireg(Some(inst), p, size)?
            }
            Operand::Imm(v) => IOp::Imm(v),
            Operand::Mem(m) => IOp::Mem(mem(inst, &m, size)?),
            Operand::Slot { slot, offset } => IOp::Mem(slot_mem(ctx.frame, slot, offset, size)?),
            _ => return Err(EmitError::Unencodable { inst: Some(inst) }),
        })
    }
}

fn asm_error(inst: Option<Inst>, e: AsmError) -> EmitError {
    match e {
        AsmError::Encode(error) => EmitError::Encode { inst, error },
        other => EmitError::Asm(other),
    }
}

/// The isa-lang register for a general-purpose physical register.
fn ireg(inst: Option<Inst>, p: PReg, size: Size) -> Result<IOp, EmitError> {
    let info = REGS.get(p.index()).ok_or(EmitError::Unencodable { inst })?;
    if info.class() != GPR {
        return Err(EmitError::Unencodable { inst });
    }
    IReg::new(size, info.hw())
        .map(IOp::Reg)
        .ok_or(EmitError::Unencodable { inst })
}

fn qword(inst: Inst, r: Reg) -> Result<IReg, EmitError> {
    let p = r
        .as_phys()
        .ok_or(EmitError::Unencodable { inst: Some(inst) })?;
    match ireg(Some(inst), p, Size::Qword)? {
        IOp::Reg(r) => Ok(r),
        _ => Err(EmitError::Unencodable { inst: Some(inst) }),
    }
}

fn mem(inst: Inst, m: &Mem, size: Size) -> Result<IMem, EmitError> {
    let mut out = match m.base_reg() {
        Some(b) => IMem::new(size, qword(inst, b)?),
        None => IMem::absolute(size, 0),
    };
    if let Some((i, s)) = m.index_reg() {
        let scale = match s {
            Scale::X1 => IScale::X1,
            Scale::X2 => IScale::X2,
            Scale::X4 => IScale::X4,
            Scale::X8 => IScale::X8,
        };
        out = out.with_index(qword(inst, i)?, scale);
    }
    Ok(out.with_disp(m.disp()))
}

fn slot_mem(
    frame: &FrameLayout,
    slot: StackSlot,
    offset: i32,
    size: Size,
) -> Result<IMem, EmitError> {
    let base = frame
        .slot_offset(slot)
        .ok_or(EmitError::Mir(MirError::UnknownSlot(slot)))?;
    let disp = base
        .checked_add(offset)
        .ok_or(EmitError::Frame(FrameError::TooLarge))?;
    Ok(IMem::new(size, IReg::RSP).with_disp(disp))
}

fn block_operand(ops: &[Operand], index: usize, inst: Inst) -> Result<BlockCall, EmitError> {
    match ops.get(index) {
        Some(Operand::Block(c)) => Ok(*c),
        _ => Err(EmitError::Mir(MirError::OperandShape { inst, index })),
    }
}

fn cond_operand(ops: &[Operand], index: usize, inst: Inst) -> Result<ICond, EmitError> {
    match ops.get(index) {
        Some(Operand::Cond(c)) => {
            ICond::from_code(c.code()).ok_or(EmitError::Mir(MirError::OperandShape { inst, index }))
        }
        _ => Err(EmitError::Mir(MirError::OperandShape { inst, index })),
    }
}

/// A parallel-copy pair as a resolver move: the destination is a block
/// parameter or a copy's destination, the source an argument.
fn to_move(f: &Function, dst: Operand, src: Operand) -> Result<Move, EmitError> {
    let class_of = |op: &Operand| op.reg().and_then(|r| f.reg_class(r));
    let class = class_of(&dst)
        .or_else(|| class_of(&src))
        .unwrap_or(f.target().addr_class());
    let loc = |op: Operand| -> Option<Loc> {
        match op {
            Operand::Reg(r) => r.as_phys().map(Loc::Reg),
            Operand::Slot { slot, offset: 0 } => Some(Loc::Slot(slot)),
            _ => None,
        }
    };
    let bad = EmitError::Unencodable { inst: None };
    let d = loc(dst).ok_or(bad.clone())?;
    let s = match src {
        Operand::Imm(v) => Source::Imm(v),
        other => Source::Loc(loc(other).ok_or(bad)?),
    };
    Ok(Move {
        dst: d,
        src: s,
        class,
    })
}

/// The probe loop's counter: `r11`, or `r10` if the entry block receives a
/// value in `r11` (neither carries arguments in SysV or Win64).
fn probe_scratch(f: &Function) -> Result<PReg, EmitError> {
    let params = f.entry().map_or(&[][..], |e| f.params(e));
    let taken = |r: PReg| params.iter().any(|p| p.reg() == Some(r.into()));
    [R11, R10]
        .into_iter()
        .find(|&r| !taken(r))
        .ok_or(EmitError::ProbeScratch)
}

/// Checks the function is ready for emission: physical registers only, tied
/// operands equal, and the frame's registers never written by the body.
fn check_physical(f: &Function) -> Result<(), EmitError> {
    let target = f.target();
    let reserved = f.reserved();
    for block in f.blocks() {
        for p in f.params(block) {
            if let Some(r) = p.reg() {
                match r.as_phys() {
                    None => return Err(EmitError::VirtualRegister { block, inst: None }),
                    Some(p) if reserved.contains(p) => {
                        return Err(EmitError::ReservedWrite { block, inst: None });
                    }
                    Some(_) => {}
                }
            }
        }
        for &inst in f.insts(block) {
            let virt = EmitError::VirtualRegister {
                block,
                inst: Some(inst),
            };
            let ops = f.operands(inst);
            for op in ops {
                let any_virtual = match op {
                    Operand::Reg(r) => r.is_virtual(),
                    Operand::Mem(m) => m.regs().any(Reg::is_virtual),
                    Operand::Block(call) => f
                        .args(*call)
                        .iter()
                        .any(|a| a.reg().is_some_and(Reg::is_virtual)),
                    _ => false,
                };
                if any_virtual {
                    return Err(virt);
                }
            }
            let Some(desc) = f.opcode(inst).and_then(|o| target.inst(o)) else {
                continue;
            };
            let defs = match desc.kind() {
                InstKind::Copy => 1,
                InstKind::ParallelCopy => ops.len() / 2,
                _ => desc.defs(),
            };
            let reserved_write = ops
                .iter()
                .take(defs)
                .filter_map(|o| o.reg().and_then(Reg::as_phys))
                .chain(
                    f.implicit(inst)
                        .iter()
                        .filter(|i| i.kind == ImplicitKind::Def)
                        .map(|i| i.reg),
                )
                .chain(desc.implicit_defs().iter().copied())
                .any(|p| reserved.contains(p))
                || !f.clobbers(inst).intersect(reserved).is_empty();
            if reserved_write {
                return Err(EmitError::ReservedWrite {
                    block,
                    inst: Some(inst),
                });
            }
            for (i, rule) in desc.operands().iter().enumerate() {
                if let OperandRule::Tied(d) = rule {
                    if ops.get(i).and_then(Operand::reg)
                        != ops.get(usize::from(*d)).and_then(Operand::reg)
                    {
                        return Err(EmitError::TiedMismatch { inst });
                    }
                }
            }
        }
    }
    Ok(())
}
