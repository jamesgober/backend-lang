//! Dense ids for a function's blocks, instructions, stack slots, symbols, and
//! condition codes.

use core::fmt;

macro_rules! entity {
    ($(#[$doc:meta])* $name:ident, $prefix:literal, $example:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(u32);

        impl $name {
            /// The id with index `index`. Whether a function has it is
            /// checked where it is used.
            ///
            /// # Examples
            ///
            /// ```
            #[doc = concat!("use backend_lang::mir::", stringify!($name), ";")]
            ///
            #[doc = concat!("assert_eq!(", stringify!($name), "::new(3).index(), 3);")]
            /// ```
            #[inline]
            #[must_use]
            pub const fn new(index: u32) -> $name {
                $name(index)
            }

            /// The id's index.
            ///
            /// # Examples
            ///
            /// ```
            #[doc = $example]
            /// ```
            #[inline]
            #[must_use]
            pub const fn index(self) -> usize {
                self.0 as usize
            }

            /// Builds an id from a table length the caller has bounded.
            #[allow(dead_code, reason = "not every id type is built from a length")]
            pub(crate) const fn from_index(index: usize) -> $name {
                $name(index as u32)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "{}"), self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "{}"), self.0)
            }
        }
    };
}

entity!(
    /// A basic block of a function. The first block created is the entry.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let entry = f.block()?;
    /// assert_eq!(entry.to_string(), "block0");
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    Block,
    "block",
    "use backend_lang::mir::Function;\nuse backend_lang::x86_64::SYSV;\nlet mut f = Function::new(\"f\", &SYSV);\nlet _ = f.block()?;\nassert_eq!(f.block()?.index(), 1);\n# Ok::<(), backend_lang::mir::MirError>(())"
);

entity!(
    /// An instruction of a function, numbered in creation order across all
    /// blocks.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, Operand};
    /// use backend_lang::x86_64::{op, SYSV};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// let i = f.push(b, op::RET, &[])?;
    /// assert_eq!(i.to_string(), "inst0");
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    Inst,
    "inst",
    "use backend_lang::mir::Function;\nuse backend_lang::x86_64::{op, SYSV};\nlet mut f = Function::new(\"f\", &SYSV);\nlet b = f.block()?;\nassert_eq!(f.push(b, op::RET, &[])?.index(), 0);\n# Ok::<(), backend_lang::mir::MirError>(())"
);

entity!(
    /// A stack slot of a function: a local, a spill slot, or an incoming
    /// stack argument. Printed `ssN`.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, SlotKind};
    /// use backend_lang::x86_64::SYSV;
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let s = f.stack_slot(SlotKind::Local { size: 8, align: 8 })?;
    /// assert_eq!(s.to_string(), "ss0");
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    StackSlot,
    "ss",
    "use backend_lang::mir::{Function, SlotKind};\nuse backend_lang::x86_64::SYSV;\nlet mut f = Function::new(\"f\", &SYSV);\nlet s = f.stack_slot(SlotKind::Local { size: 8, align: 8 })?;\nassert_eq!(s.index(), 0);\n# Ok::<(), backend_lang::mir::MirError>(())"
);

entity!(
    /// A reference to a function by name: a call target, or a function whose
    /// address an instruction takes. Resolved by name at emission.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let s = f.symbol("helper")?;
    /// assert_eq!(f.symbol_name(s), Some("helper"));
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    Symbol,
    "sym",
    "use backend_lang::mir::Function;\nuse backend_lang::x86_64::SYSV;\nlet mut f = Function::new(\"f\", &SYSV);\nassert_eq!(f.symbol(\"g\")?.index(), 0);\n# Ok::<(), backend_lang::mir::MirError>(())"
);

/// A condition code, numbered by the target (for x86-64, the `cc` field of
/// `Jcc`/`SETcc`/`CMOVcc`; see [`x86_64::cond`](crate::x86_64::cond)).
///
/// # Examples
///
/// ```
/// use backend_lang::mir::CondCode;
/// use backend_lang::x86_64;
/// use isa_lang::x86_64::Cond;
///
/// let l = x86_64::cond(Cond::L);
/// assert_eq!(l, CondCode::new(12));
/// assert_eq!(x86_64::TARGET.cond_name(l), "l");
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CondCode(u8);

impl CondCode {
    /// The condition with number `code`.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::CondCode;
    ///
    /// assert_eq!(CondCode::new(4).code(), 4);
    /// ```
    #[inline]
    #[must_use]
    pub const fn new(code: u8) -> CondCode {
        CondCode(code)
    }

    /// The condition's number.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::CondCode;
    ///
    /// assert_eq!(CondCode::new(15).code(), 15);
    /// ```
    #[inline]
    #[must_use]
    pub const fn code(self) -> u8 {
        self.0
    }
}
