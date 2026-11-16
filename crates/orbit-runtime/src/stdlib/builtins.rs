//! Built-in library functions written in OrbitScript (ADR-11), loaded over
//! the intrinsics ([`super::intrinsics`]).
//!
//! The modules in `builtins/` are compiled by the build script and embedded
//! as binary chunks. Each loads with its own environment holding only the
//! intrinsics (taken with `use intrinsics::{...}`), `next` and `type`, so
//! scripts can neither reach the intrinsics nor change what a built-in
//! calls, and returns its exports. Their prototypes are
//! marked built-in, so the debug interface, error messages and tracebacks
//! show them as C functions.

use std::sync::OnceLock;

use crate::object::{Table, Value};
use crate::vm::op::{UnlinkedChunk, format};
use crate::vm::{LuaError, NativeCtx, NativeFn, NativeReturn, Vm};

type R = Result<NativeReturn, LuaError>;

/// A built-in module: its binary chunk, and its decoded form, shared by
/// every isolate in the process.
pub(crate) struct Module {
    bytes: &'static [u8],
    decoded: OnceLock<UnlinkedChunk>,
}

impl Module {
    const fn new(bytes: &'static [u8]) -> Module {
        Module {
            bytes,
            decoded: OnceLock::new(),
        }
    }

    fn chunk(&self) -> &UnlinkedChunk {
        self.decoded.get_or_init(|| {
            format::decode(self.bytes).expect("a built-in module the build script compiled")
        })
    }
}

pub(crate) static BASE: Module =
    Module::new(include_bytes!(concat!(env!("OUT_DIR"), "/base.orbc")));
pub(crate) static TABLE: Module =
    Module::new(include_bytes!(concat!(env!("OUT_DIR"), "/table.orbc")));

/// The name of a native only built-in modules see (their `require`):
/// never shown, since a built-in's errors name the built-in.
macro_rules! native_name {
    ($name:ident) => {
        concat!("(intrinsic ", stringify!($name), ")")
    };
}

/// Where a module's environment keeps the intrinsics table for
/// [`require`]. Not a name a module can use: OrbitScript has no implicit
/// globals.
const INTRINSICS_KEY: &str = "(intrinsics)";

/// Built-ins that code they call may yield across, as their C counterparts
/// in the reference allow (`pairs` calls `__pairs` yieldably). Every other
/// built-in refuses a yield, as a C function without a continuation does.
const YIELDING: &[&str] = &["pairs"];

/// Load `module` and return its exports, `(name, function)` pairs.
pub(crate) fn load(vm: &mut Vm, module: &Module) -> Vec<(Vec<u8>, Value)> {
    let env = vm.new_table_value();
    let Value::Table(env_t) = env else {
        unreachable!("a new table")
    };
    let intrinsics = Value::Table(super::intrinsics::table(vm));
    vm.raw_set_str(env_t, INTRINSICS_KEY, intrinsics);
    let require = vm.native_named(native_name!(require), require);
    vm.raw_set_str(env_t, "require", require);
    for row in [super::base::NEXT, super::base::TYPE] {
        let v = match vm.rt.natives.named(row.name) {
            Some(id) => Value::Native(id),
            None => super::register_row(vm, &row),
        };
        vm.raw_set_str(env_t, row.name, v);
    }
    let main = vm.load_builtin(module.chunk(), env);
    let exports = match vm.call(main, &[]) {
        Ok(r) => r.into_iter().next(),
        Err(_) => None,
    };
    let Some(Value::Table(t)) = exports else {
        panic!("a built-in module returns its exports");
    };
    let exports: Vec<(Vec<u8>, Value)> = vm
        .table_entries(t)
        .into_iter()
        .filter_map(|(k, v)| match k {
            Value::Str(s) => Some((vm.heap.get(s).as_bytes().to_vec(), v)),
            _ => None,
        })
        .collect();
    for (name, v) in &exports {
        if let Value::Closure(c) = *v {
            if YIELDING.iter().any(|y| y.as_bytes() == name.as_slice()) {
                vm.allow_yields(c);
            }
        }
    }
    exports
}

impl Vm {
    fn new_table_value(&mut self) -> Value {
        Value::Table(self.heap.alloc(Table::new()))
    }

    /// The native registered as `name`, registering `f` under it the first time.
    fn native_named(&mut self, name: &'static str, f: NativeFn) -> Value {
        match self.rt.natives.named(name) {
            Some(id) => Value::Native(id),
            None => self.register_native(name, f),
        }
    }

    fn raw_set_str(&mut self, t: crate::object::Gc<Table>, name: &str, v: Value) {
        let k = self.new_str(name.as_bytes());
        self.table_set(t, k, v);
    }

    /// Let code `c` calls yield across it. `c` must be its prototype's only
    /// closure, as an exported built-in is when its module has just run.
    fn allow_yields(&mut self, c: crate::object::Gc<crate::object::Closure>) {
        let proto = self.heap.get(c).proto;
        let code = std::rc::Rc::new(self.heap.get(proto).code.allowing_yields());
        // The new code keeps the old one's constants.
        self.heap.write_no_handles(proto).code = code.clone();
        self.heap.write_no_handles(c).code = code;
    }

    fn table_entries(&self, t: crate::object::Gc<Table>) -> Vec<(Value, Value)> {
        let tb = self.heap.get(t);
        let mut out = Vec::new();
        let mut k = Value::Nil;
        while let Ok(Some((nk, v))) = tb.next(&self.heap, k) {
            out.push((nk, v));
            k = nk;
        }
        out
    }
}

/// `require(name)` inside a built-in module, which `use` calls: only
/// `intrinsics` exists.
fn require(ctx: &mut NativeCtx<'_>) -> R {
    let name = ctx.arg(0);
    let is_intrinsics = matches!(name, Value::Str(s) if ctx.bytes(s) == b"intrinsics");
    let found = match ctx.builtin_env() {
        Value::Table(env) if is_intrinsics => {
            let k = ctx.new_str(INTRINSICS_KEY.as_bytes());
            ctx.vm.heap.get(env).get(&ctx.vm.heap, k)
        }
        _ => Value::Nil,
    };
    if found.is_nil() {
        return Err(ctx.builtin_error("built-in modules can only use 'intrinsics'"));
    }
    ctx.ret([found])
}
