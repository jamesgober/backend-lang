//! The Tier-1 path: an ir-lang 2.0 module to x86-64 machine code, with the
//! symbols, relocations, data, and stack maps a linker or JIT needs.

use alloc::collections::BTreeSet;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use ir_lang::{FuncId, Init, Linkage, Module, SymbolRef};

use crate::isel::{self, IselError};
use crate::mir;
use crate::regalloc::{self, RegAllocError};
use crate::target::abi::Abi;
use crate::x86_64::{self, EmitError, EmittedFunction, Emitter, Program, Reloc, RelocKind};

/// How to compile: the platform's C convention, frame and runtime choices,
/// and selection and allocation settings.
///
/// The C convention (`abi`) serves ir-lang's `c` tag and its `fast` tag (the
/// backend's own convention for code compiled together is the platform's C
/// convention in this release). Runtime entry points are called by name and
/// left as relocations unless the module defines them:
///
/// | Symbol | Called | With |
/// |---|---|---|
/// | `trap_symbol` | for `trap` and every trapping policy | the OPS code (first argument); never returns |
/// | `gc_poll_symbol` | for a `safepoint` instruction | nothing; a safepoint with a stack map |
/// | `resume_symbol` | for `resume` | the exception payload; never returns |
///
/// # Examples
///
/// ```
/// use backend_lang::Options;
///
/// let o = Options::sysv().with_frame_pointer(false);
/// assert_eq!(o.abi.name(), "sysv64");
/// assert!(!o.frame_pointer);
/// assert_eq!(Options::win64().abi.name(), "win64");
/// ```
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct Options {
    /// The platform's C convention, used for the `c` and `fast` tags and for
    /// runtime calls.
    pub abi: &'static Abi,
    /// Whether functions keep a frame pointer (on by default). Without one,
    /// `rbp` is one more register for the allocator.
    pub frame_pointer: bool,
    /// The runtime's trap handler.
    pub trap_symbol: &'static str,
    /// The runtime's GC poll, called by `safepoint`.
    pub gc_poll_symbol: &'static str,
    /// The runtime's unwinder entry, called by `resume`.
    pub resume_symbol: &'static str,
    /// The fewest cases a switch needs for a jump table.
    pub jump_table_min_cases: u32,
    /// A jump table is used when the case values span at most this many
    /// table entries per case (3: at least a third of the entries are
    /// cases).
    pub jump_table_max_spread: u32,
    /// Register-allocation settings.
    pub regalloc: regalloc::Options,
}

impl Options {
    /// For System V AMD64 platforms (Linux, the BSDs, macOS).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::Options;
    ///
    /// assert_eq!(Options::sysv().trap_symbol, "__lexersketch_trap");
    /// ```
    #[must_use]
    pub const fn sysv() -> Options {
        Options::with_abi(&x86_64::SYSV)
    }

    /// For Windows x64.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::Options;
    ///
    /// assert_eq!(Options::win64().abi.name(), "win64");
    /// ```
    #[must_use]
    pub const fn win64() -> Options {
        Options::with_abi(&x86_64::WIN64)
    }

    /// For the platform this crate is compiled for: Win64 on Windows, System
    /// V elsewhere.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::Options;
    ///
    /// let name = Options::host().abi.name();
    /// assert!(name == "sysv64" || name == "win64");
    /// ```
    #[must_use]
    pub const fn host() -> Options {
        if cfg!(windows) {
            Options::win64()
        } else {
            Options::sysv()
        }
    }

    /// The defaults with `abi` as the C convention.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::Options;
    /// use backend_lang::x86_64::WIN64;
    ///
    /// assert_eq!(Options::with_abi(&WIN64).abi.name(), "win64");
    /// ```
    #[must_use]
    pub const fn with_abi(abi: &'static Abi) -> Options {
        Options {
            abi,
            frame_pointer: true,
            trap_symbol: "__lexersketch_trap",
            gc_poll_symbol: "__lexersketch_gc_poll",
            resume_symbol: "__lexersketch_resume",
            jump_table_min_cases: 4,
            jump_table_max_spread: 3,
            regalloc: regalloc::Options::new(),
        }
    }

    /// Keeps or omits the frame pointer.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::Options;
    ///
    /// assert!(!Options::sysv().with_frame_pointer(false).frame_pointer);
    /// ```
    #[must_use]
    pub const fn with_frame_pointer(mut self, keep: bool) -> Options {
        self.frame_pointer = keep;
        self
    }

    /// Renames the runtime entry points.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::Options;
    ///
    /// let o = Options::sysv().with_runtime("my_trap", "my_poll", "my_resume");
    /// assert_eq!(o.gc_poll_symbol, "my_poll");
    /// ```
    #[must_use]
    pub const fn with_runtime(
        mut self,
        trap: &'static str,
        gc_poll: &'static str,
        resume: &'static str,
    ) -> Options {
        self.trap_symbol = trap;
        self.gc_poll_symbol = gc_poll;
        self.resume_symbol = resume;
        self
    }

    /// Sets when a switch becomes a jump table.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::Options;
    ///
    /// // Never use a jump table.
    /// let o = Options::sysv().with_jump_tables(u32::MAX, 1);
    /// assert_eq!(o.jump_table_min_cases, u32::MAX);
    /// ```
    #[must_use]
    pub const fn with_jump_tables(mut self, min_cases: u32, max_spread: u32) -> Options {
        self.jump_table_min_cases = min_cases;
        self.jump_table_max_spread = max_spread;
        self
    }

    /// Sets the register-allocation settings.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::{regalloc, Options};
    ///
    /// let o = Options::sysv().with_regalloc(regalloc::Options::new().with_live_budget(1 << 20));
    /// assert_eq!(o.regalloc.live_budget, 1 << 20);
    /// ```
    #[must_use]
    pub const fn with_regalloc(mut self, regalloc: regalloc::Options) -> Options {
        self.regalloc = regalloc;
        self
    }
}

impl Default for Options {
    fn default() -> Options {
        Options::host()
    }
}

/// Why a module could not be compiled.
///
/// # Examples
///
/// ```
/// use backend_lang::{CompileError, Options};
/// use ir_lang::{Linkage, Module, Signature, Type};
///
/// let mut m = Module::new("m");
/// let f = m.declare_function("f", &Signature::new(&[], &[Type::F32]), Linkage::Export)?;
/// let mut b = m.build(f)?;
/// let x = b.f32const(1.0)?;
/// b.ret(&[x])?;
/// let err = backend_lang::compile(&m, &Options::sysv()).unwrap_err();
/// assert!(matches!(err, CompileError::Select { .. }));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompileError {
    /// The module does not validate.
    Invalid(ir_lang::ValidationError),
    /// Instruction selection failed for a function.
    Select {
        /// The function.
        function: String,
        /// Why.
        error: IselError,
    },
    /// Register allocation failed for a function.
    Allocate {
        /// The function.
        function: String,
        /// Why.
        error: RegAllocError,
    },
    /// Emission failed (for a function, if one is named).
    Emit {
        /// The function, if the failure belongs to one.
        function: Option<String>,
        /// Why.
        error: EmitError,
    },
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompileError::Invalid(e) => write!(f, "invalid module: {e}"),
            CompileError::Select { function, error } => write!(f, "`{function}`: {error}"),
            CompileError::Allocate { function, error } => write!(f, "`{function}`: {error}"),
            CompileError::Emit {
                function: Some(n),
                error,
            } => write!(f, "`{n}`: {error}"),
            CompileError::Emit {
                function: None,
                error,
            } => fmt::Display::fmt(error, f),
        }
    }
}

impl core::error::Error for CompileError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            CompileError::Invalid(e) => Some(e),
            CompileError::Select { error, .. } => Some(error),
            CompileError::Allocate { error, .. } => Some(error),
            CompileError::Emit { error, .. } => Some(error),
        }
    }
}

/// A global's bytes and the addresses stored in them.
///
/// # Examples
///
/// ```
/// use backend_lang::Options;
/// use ir_lang::{Global, Module};
///
/// let mut m = Module::new("m");
/// let _ = m.declare_global("answer", Global::bytes(&[42, 0, 0, 0], 4))?;
/// let c = backend_lang::compile(&m, &Options::sysv())?;
/// let d = &c.data()[0];
/// assert_eq!((d.name(), d.size(), d.align()), ("answer", 4, 4));
/// assert_eq!(d.bytes(), Some(&[42u8, 0, 0, 0][..]));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataObject {
    name: String,
    size: u64,
    align: u32,
    mutable: bool,
    exported: bool,
    bytes: Option<Vec<u8>>,
    relocs: Vec<Reloc>,
}

impl DataObject {
    /// The global's symbol name.
    ///
    /// # Examples
    ///
    /// See [`DataObject`].
    #[inline]
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Its size in bytes.
    ///
    /// # Examples
    ///
    /// See [`DataObject`].
    #[inline]
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Its alignment in bytes.
    ///
    /// # Examples
    ///
    /// See [`DataObject`].
    #[inline]
    #[must_use]
    pub fn align(&self) -> u32 {
        self.align
    }

    /// Whether the program may write it (else it belongs in read-only data).
    ///
    /// # Examples
    ///
    /// See [`DataObject`].
    #[inline]
    #[must_use]
    pub fn mutable(&self) -> bool {
        self.mutable
    }

    /// Whether it is visible outside the module.
    ///
    /// # Examples
    ///
    /// See [`DataObject`].
    #[inline]
    #[must_use]
    pub fn exported(&self) -> bool {
        self.exported
    }

    /// Its initial bytes, or `None` for zero-filled data.
    ///
    /// # Examples
    ///
    /// See [`DataObject`].
    #[inline]
    #[must_use]
    pub fn bytes(&self) -> Option<&[u8]> {
        self.bytes.as_deref()
    }

    /// The addresses stored in it ([`RelocKind::Abs64`], offsets from its
    /// start).
    ///
    /// # Examples
    ///
    /// See [`DataObject`].
    #[inline]
    #[must_use]
    pub fn relocations(&self) -> &[Reloc] {
        &self.relocs
    }
}

/// A compiled module: machine code, every function's place and call-site
/// records, the relocations against symbols defined elsewhere, and the
/// module's data.
///
/// The code refers to globals and imports through relocations
/// ([`relocations`](Compiled::relocations)); calls between the module's own
/// functions are already resolved. A linker places the code and data and
/// patches the relocations (object-lang writes them into an ELF relocatable
/// as they are).
///
/// # Examples
///
/// ```
/// use backend_lang::Options;
/// use ir_lang::{Linkage, Module, Overflow, Signature, Type};
///
/// let mut m = Module::new("m");
/// let f = m.declare_function("add", &Signature::new(&[Type::I64, Type::I64], &[Type::I64]), Linkage::Export)?;
/// let mut b = m.build(f)?;
/// let (x, y) = (b.param(0)?, b.param(1)?);
/// let s = b.add(x, y, Overflow::Wrap)?;
/// b.ret(&[s])?;
///
/// let c = backend_lang::compile(&m, &Options::sysv().with_frame_pointer(false))?;
/// assert_eq!(c.function("add").unwrap().offset(), 0);
/// assert!(c.relocations().is_empty());
/// assert_eq!(c.code().last(), Some(&0xC3)); // ret
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compiled {
    program: Program,
    data: Vec<DataObject>,
    imports: Vec<String>,
    exports: Vec<String>,
}

impl Compiled {
    /// The machine code of every function, back to back.
    ///
    /// # Examples
    ///
    /// See [`Compiled`].
    #[inline]
    #[must_use]
    pub fn code(&self) -> &[u8] {
        self.program.bytes()
    }

    /// The emitted program (code, functions, relocations).
    ///
    /// # Examples
    ///
    /// See [`Compiled`].
    #[inline]
    #[must_use]
    pub fn program(&self) -> &Program {
        &self.program
    }

    /// Every function, in module order.
    ///
    /// # Examples
    ///
    /// See [`Compiled`].
    #[inline]
    #[must_use]
    pub fn functions(&self) -> &[EmittedFunction] {
        self.program.functions()
    }

    /// The function named `name`.
    ///
    /// # Examples
    ///
    /// See [`Compiled`].
    #[must_use]
    pub fn function(&self, name: &str) -> Option<&EmittedFunction> {
        // The symbol table is sorted by name, and functions sit in emission
        // order at increasing offsets: two binary searches, not a scan.
        let offset = self.program.symbol(name)?;
        let functions = self.program.functions();
        let i = functions.partition_point(|f| f.offset() < offset);
        functions.get(i).filter(|f| f.name() == name)
    }

    /// The code's relocations against globals, imports, and runtime entry
    /// points.
    ///
    /// # Examples
    ///
    /// See [`Compiled`].
    #[inline]
    #[must_use]
    pub fn relocations(&self) -> &[Reloc] {
        self.program.relocations()
    }

    /// The module's globals (imported globals excluded).
    ///
    /// # Examples
    ///
    /// See [`DataObject`].
    #[inline]
    #[must_use]
    pub fn data(&self) -> &[DataObject] {
        &self.data
    }

    /// The symbols the code and data refer to that the module does not
    /// define (imports and runtime entry points), in name order.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::Options;
    /// use ir_lang::{Linkage, Module, Signature};
    ///
    /// let mut m = Module::new("m");
    /// let f = m.declare_function("f", &Signature::new(&[], &[]), Linkage::Export)?;
    /// let mut b = m.build(f)?;
    /// b.trap(1)?;
    /// let c = backend_lang::compile(&m, &Options::sysv())?;
    /// assert_eq!(c.imports(), ["__lexersketch_trap"]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn imports(&self) -> &[String] {
        &self.imports
    }

    /// The functions and globals with `export` linkage, in name order.
    ///
    /// # Examples
    ///
    /// See [`Compiled::imports`].
    #[inline]
    #[must_use]
    pub fn exports(&self) -> &[String] {
        &self.exports
    }
}

/// Compiles every function of `module` to x86-64 machine code: instruction
/// selection, register allocation, and emission.
///
/// # Errors
///
/// [`CompileError`], naming the function that failed.
///
/// # Examples
///
/// See [`Compiled`].
pub fn compile(module: &Module, options: &Options) -> Result<Compiled, CompileError> {
    module.validate().map_err(CompileError::Invalid)?;
    let mut defined: BTreeSet<String> = BTreeSet::new();
    let mut exports: Vec<String> = Vec::new();
    for func in module.functions() {
        if let Some(d) = module.func_decl(func) {
            if d.linkage() != Linkage::Import {
                let _ = defined.insert(d.name().to_string());
                if d.linkage() == Linkage::Export {
                    exports.push(d.name().to_string());
                }
            }
        }
    }
    for g in module.globals() {
        if let (Some(name), Some(data)) = (module.global_name(g), module.global(g)) {
            if data.linkage != Linkage::Import {
                let _ = defined.insert(name.to_string());
                if data.linkage == Linkage::Export {
                    exports.push(name.to_string());
                }
            }
        }
    }
    exports.sort();

    let mut emitter = Emitter::new();
    let emit_err = |function: Option<&str>, error: EmitError| CompileError::Emit {
        function: function.map(str::to_string),
        error,
    };
    // Everything not defined here is reached through a relocation:
    // imports, globals (data lives apart from code), runtime entry points.
    let mut externals: BTreeSet<String> = BTreeSet::new();
    for func in module.functions() {
        if let Some(d) = module.func_decl(func) {
            if d.linkage() == Linkage::Import {
                let _ = externals.insert(d.name().to_string());
            }
        }
    }
    for g in module.globals() {
        if let Some(name) = module.global_name(g) {
            let _ = externals.insert(name.to_string());
        }
    }
    for rt in [
        options.trap_symbol,
        options.gc_poll_symbol,
        options.resume_symbol,
    ] {
        if !defined.contains(rt) {
            let _ = externals.insert(rt.to_string());
        }
    }
    for name in &externals {
        emitter
            .declare_external(name)
            .map_err(|e| emit_err(Some(name), e))?;
    }

    for func in module.functions() {
        let Some(decl) = module.func_decl(func) else {
            continue;
        };
        if decl.linkage() == Linkage::Import {
            continue;
        }
        let name = decl.name();
        let mir = compile_validated(module, func, options)?;
        let _ = emitter
            .function(&mir)
            .map_err(|e| emit_err(Some(name), e))?;
    }
    let program = emitter.finish().map_err(|e| emit_err(None, e))?;

    let mut data = Vec::new();
    for g in module.globals() {
        let (Some(name), Some(gd)) = (module.global_name(g), module.global(g)) else {
            continue;
        };
        if gd.linkage == Linkage::Import {
            continue;
        }
        let mut relocs = Vec::with_capacity(gd.relocs.len());
        for r in &gd.relocs {
            let target = match r.target {
                SymbolRef::Func(f) => module.func_decl(f).map(|d| d.name().to_string()),
                SymbolRef::Global(t) => module.global_name(t).map(str::to_string),
                _ => None,
            };
            if let Some(t) = target {
                relocs.push(Reloc::new(
                    r.offset as usize,
                    RelocKind::Abs64,
                    &t,
                    r.addend,
                ));
            }
        }
        data.push(DataObject {
            name: name.to_string(),
            size: gd.size,
            align: gd.align,
            mutable: gd.mutable,
            exported: gd.linkage == Linkage::Export,
            bytes: match &gd.init {
                Init::Bytes(b) => Some(b.clone()),
                _ => None,
            },
            relocs,
        });
    }

    let mut imports: BTreeSet<String> = BTreeSet::new();
    for r in program.relocations() {
        if !defined.contains(&r.symbol) {
            let _ = imports.insert(r.symbol.clone());
        }
    }
    for d in &data {
        for r in d.relocations() {
            if !defined.contains(&r.symbol) {
                let _ = imports.insert(r.symbol.clone());
            }
        }
    }
    Ok(Compiled {
        program,
        data,
        imports: imports.into_iter().collect(),
        exports,
    })
}

/// Selects and allocates one function: the MIR on physical registers that
/// [`emit`](crate::emit) or an [`Emitter`] encodes.
///
/// # Errors
///
/// [`CompileError`].
///
/// # Examples
///
/// ```
/// use backend_lang::Options;
/// use ir_lang::{Linkage, Module, Signature, Type};
///
/// let mut m = Module::new("m");
/// let f = m.declare_function("seven", &Signature::new(&[], &[Type::I32]), Linkage::Export)?;
/// let mut b = m.build(f)?;
/// let k = b.iconst(Type::I32, 7)?;
/// b.ret(&[k])?;
/// let mir = backend_lang::compile_function(&m, f, &Options::sysv())?;
/// assert!(mir.to_string().contains("rax"));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn compile_function(
    module: &Module,
    func: FuncId,
    options: &Options,
) -> Result<mir::Function, CompileError> {
    module.validate().map_err(CompileError::Invalid)?;
    compile_validated(module, func, options)
}

fn compile_validated(
    module: &Module,
    func: FuncId,
    options: &Options,
) -> Result<mir::Function, CompileError> {
    let name = module
        .func_decl(func)
        .map_or_else(|| func.to_string(), |d| d.name().to_string());
    let mut mir =
        isel::select_validated(module, func, options).map_err(|error| CompileError::Select {
            function: name.clone(),
            error,
        })?;
    let _ = regalloc::allocate_with(&mut mir, &options.regalloc).map_err(|error| {
        CompileError::Allocate {
            function: name.clone(),
            error,
        }
    })?;
    Ok(mir)
}
