//! [`Class`]: a Rust type made visible to scripts as userdata with methods.

use std::marker::PhantomData;

use crate::error::Error;
use crate::host::{Call, HostFunction, Ret};
use crate::pin::Pinned;
use crate::scope::Scope;
use crate::value::{Table, Userdata};

/// How the API stores a host value in a userdata: taken out while a method
/// runs, so the method can use the whole [`Call`] at the same time.
pub(crate) struct Slot<T>(pub(crate) Option<T>);

/// A Rust type made visible to scripts: values of `T` become userdata whose
/// metatable has the class's name (`__name`), methods (through `__index`)
/// and metamethods. `T`'s `Drop` is the finalizer.
///
/// ```
/// # let rt = orbit::Runtime::new();
/// # let mut iso = rt.new_isolate(orbit::Config::default());
/// struct Counter {
///     n: i64,
/// }
///
/// iso.scope(|s| {
///     let counter = s.define_class::<Counter>("Counter", |c| {
///         c.method("inc", |me, call| {
///             me.n += call.opt(1, 1)?;
///             call.ret(me.n)
///         })?;
///         Ok(())
///     })?;
///     s.globals().set("c", counter.instance(s, Counter { n: 40 })?)?;
///     s.load(orbit::Chunk::new("c:inc() assert(c:inc() == 42)"))?.call(())?;
///     Ok::<_, orbit::Error>(())
/// })
/// .unwrap();
/// ```
pub struct Class<T: 'static> {
    meta: Pinned<Table<'static>>,
    _t: PhantomData<fn(T)>,
}

impl<T: 'static> std::fmt::Debug for Class<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Class")
            .field("type", &std::any::type_name::<T>())
            .finish_non_exhaustive()
    }
}

impl<T: 'static> Class<T> {
    /// A new instance holding `value`.
    pub fn instance<'s>(&self, s: &'s Scope<'s>, value: T) -> Result<Userdata<'s>, Error> {
        let meta = self.meta.get(s)?;
        let u = s.create_userdata(value);
        let mut stack = s.stack();
        stack.push_copy(meta.slot());
        let r = stack.set_metatable(u.slot());
        debug_assert!(r.is_ok(), "a table is a metatable");
        Ok(u)
    }

    /// The class's metatable.
    pub fn metatable<'s>(&self, s: &'s Scope<'s>) -> Result<Table<'s>, Error> {
        self.meta.get(s)
    }
}

/// Builds a [`Class`] ([`Scope::define_class`]).
#[derive(Debug)]
pub struct ClassBuilder<'s, T: 'static> {
    s: &'s Scope<'s>,
    name: String,
    meta: Table<'s>,
    methods: Table<'s>,
    _t: PhantomData<fn(T)>,
}

impl<'s, T: 'static> ClassBuilder<'s, T> {
    /// A method scripts call as `obj:name(...)`. It gets the object, taken
    /// out of its userdata while it runs, and the call (whose argument 0 is
    /// the object). Calling a method of an object whose method is already
    /// running raises an error.
    pub fn method(
        &mut self,
        name: &str,
        f: impl for<'c> Fn(&mut T, &'c Call<'c>) -> Result<Ret, Error> + 'static,
    ) -> Result<&mut Self, Error> {
        let class = self.name.clone();
        let m = self.s.create_function(move |call| {
            let mut taken = Taken::<T>::take(call, &class)?;
            let value = taken.value.as_mut().expect("taken until dropped");
            f(value, call)
        });
        self.methods.raw_set(name, m)?;
        Ok(self)
    }

    /// A function in the class's method table that doesn't take the object
    /// (`Counter.new(...)` once the methods table is published).
    pub fn function(&mut self, name: &str, f: impl HostFunction) -> Result<&mut Self, Error> {
        let f = self.s.create_function(f);
        self.methods.raw_set(name, f)?;
        Ok(self)
    }

    /// A metamethod (`"__add"`, `"__tostring"`, `"__len"`, ...). `__index`
    /// and `__name` are the class's own; `__gc` is `T`'s `Drop`.
    pub fn metamethod(&mut self, event: &str, f: impl HostFunction) -> Result<&mut Self, Error> {
        if matches!(event, "__index" | "__name" | "__gc") {
            return Err(Error::runtime(format!(
                "a class can't set {event}: it is the class's own"
            )));
        }
        let f = self.s.create_function(f);
        self.meta.raw_set(event, f)?;
        Ok(self)
    }

    /// The table of methods and functions, to publish as a global or
    /// module.
    pub fn methods(&self) -> Table<'s> {
        self.methods
    }
}

impl<'s> Scope<'s> {
    /// Make `T` visible to scripts as userdata named `name`, with what
    /// `build` adds. Each isolate defines its classes once and keeps the
    /// [`Class`].
    pub fn define_class<T: 'static>(
        &'s self,
        name: &str,
        build: impl FnOnce(&mut ClassBuilder<'s, T>) -> Result<(), Error>,
    ) -> Result<Class<T>, Error> {
        let meta = self.create_table();
        let methods = self.create_table();
        meta.raw_set("__name", name)?;
        meta.raw_set("__index", methods)?;
        let mut b = ClassBuilder {
            s: self,
            name: name.to_owned(),
            meta,
            methods,
            _t: PhantomData,
        };
        build(&mut b)?;
        Ok(Class {
            meta: meta.pin(),
            _t: PhantomData,
        })
    }
}

/// An object taken out of its userdata while its method runs; dropping it
/// puts the object back, even when the method fails or panics.
struct Taken<'a, T: 'static> {
    call: &'a Call<'a>,
    value: Option<T>,
}

impl<'a, T: 'static> Taken<'a, T> {
    fn take(call: &'a Call<'a>, class: &str) -> Result<Taken<'a, T>, Error> {
        // The object is argument 0, slot 1 of the window.
        let taken = call.stack().with_userdata::<T, _>(1, Option::take);
        match taken {
            None => Err(call.type_error(0, class)),
            Some(None) => Err(call.error(format!(
                "{class} object is already in use by a running method"
            ))),
            Some(Some(value)) => Ok(Taken {
                call,
                value: Some(value),
            }),
        }
    }
}

impl<T: 'static> Drop for Taken<'_, T> {
    fn drop(&mut self) {
        // During a panic's unwinding the window may still be borrowed;
        // then the object stays taken rather than panicking again.
        let Some(mut stack) = self.call.try_stack() else {
            return;
        };
        let value = self.value.take();
        stack.with_userdata::<T, _>(1, |cell| *cell = value);
    }
}
