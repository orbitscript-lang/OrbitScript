//! Debugging states that C programs create (ADR-12): each one is
//! served at creation unless made with `ORBIT_STATE_NO_DEBUGGER`, so that
//! `orbit_debug_listen`, or `ORBIT_DEBUG=1` when the state is made, can
//! start the process's debug server from any thread. Until an editor
//! attaches, a served state runs as it would undebugged.
//!
//! Without the `debugger` feature nothing is served and listening fails.

use std::ffi::c_int;

#[cfg(feature = "debugger")]
use std::sync::Mutex;
#[cfg(feature = "debugger")]
use std::sync::atomic::{AtomicBool, Ordering};

/// The process's debug server, once a state is served.
#[cfg(feature = "debugger")]
static SERVICE: Mutex<Option<orbit_dap::Service>> = Mutex::new(None);

/// The socket is removed at exit (set by the first listen).
#[cfg(feature = "debugger")]
static REMOVED_AT_EXIT: AtomicBool = AtomicBool::new(false);

/// Make `iso`, a new state, debuggable; whether it is.
#[cfg(feature = "debugger")]
pub(crate) fn serve(iso: &mut orbit::Isolate) -> bool {
    let mut args = std::env::args();
    let program = args.next().unwrap_or_default();
    let about = orbit_dap::About {
        program,
        args: args.collect(),
    };
    match orbit_dap::serve(iso, about) {
        Ok(service) => {
            let mut slot = SERVICE.lock().unwrap_or_else(|e| e.into_inner());
            *slot = Some(service);
            true
        }
        Err(_) => false,
    }
}

#[cfg(not(feature = "debugger"))]
pub(crate) fn serve(_iso: &mut orbit::Isolate) -> bool {
    false
}

/// Start listening for debuggers: 1 the first time it succeeds (the
/// caller then removes the socket at exit), 0 after that, -1 if it fails.
/// With `announce`, say where on standard error.
#[cfg(feature = "debugger")]
pub(crate) fn listen(announce: bool) -> c_int {
    let service = SERVICE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(service) = service else {
        return -1;
    };
    match service.start() {
        Ok(path) => {
            if announce {
                eprintln!("liborbit: debugger listening on {}", path.display());
            }
            c_int::from(!REMOVED_AT_EXIT.swap(true, Ordering::Relaxed))
        }
        Err(e) => {
            if announce {
                eprintln!("liborbit: can't listen for debuggers: {e}");
            }
            -1
        }
    }
}

#[cfg(not(feature = "debugger"))]
pub(crate) fn listen(_announce: bool) -> c_int {
    -1
}

/// The program is exiting: remove the socket.
pub(crate) fn stop() {
    #[cfg(feature = "debugger")]
    if let Some(service) = &*SERVICE.lock().unwrap_or_else(|e| e.into_inner()) {
        service.stop();
    }
}
