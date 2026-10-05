//! `u32` newtype ids. Every id except [`FuncId`] and [`StrId`] is local to one function.

macro_rules! ids {
    ($($(#[$doc:meta])* $name:ident;)*) => {$(
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u32);

        impl $name {
            /// Makes an id from a vector index. Panics past `u32::MAX` entries.
            pub fn from_index(index: usize) -> Self {
                $name(u32::try_from(index).expect(concat!(stringify!($name), " overflow")))
            }

            pub fn index(self) -> usize {
                self.0 as usize
            }
        }
    )*};
}

ids! {
    /// A function in [`crate::Chunk::functions`].
    FuncId;
    /// A local in [`crate::Function::locals`].
    LocalId;
    /// An expression in [`crate::Function::exprs`].
    ExprId;
    /// A statement in [`crate::Function::stmts`].
    StmtId;
    /// A block in [`crate::Function::blocks`].
    BlockId;
    /// A capture in [`crate::Function::captures`].
    CaptureId;
    /// A `goto` label in [`crate::Function::labels`].
    LabelId;
    /// A string in [`crate::Chunk::strings`].
    StrId;
    /// A type annotation. Ignored by the compiler; read by the optional type checker.
    TypeId;
}
