//! SPIKE: the runtime, the C API and both command-line programs in one
//! shared library. `orbit` and `orbitc` become stubs that call
//! `orbit_main`/`orbitc_main`.
#![allow(unsafe_code, dead_code, missing_debug_implementations)]

#[cfg(feature = "cli")]
mod cli;
#[cfg(feature = "cli")]
mod orbitc;

#[cfg(feature = "cli")]
#[unsafe(no_mangle)]
pub extern "C" fn orbit_main() -> ! {
    exit(cli::main())
}

#[cfg(feature = "cli")]
#[unsafe(no_mangle)]
pub extern "C" fn orbitc_main() -> ! {
    exit(orbitc::main())
}

// Keep the C API linked in even with nothing in Rust calling it.
#[unsafe(no_mangle)]
pub extern "C" fn orbit_shared_keep() -> usize {
    orbit::Lua::new as *const () as usize
}

fn exit(c: std::process::ExitCode) -> ! {
    std::process::exit(if c == std::process::ExitCode::SUCCESS {
        0
    } else {
        1
    })
}
