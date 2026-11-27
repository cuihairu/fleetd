//! Embedded Python host for fleetd — PyO3 binding to real CPython.
//!
//! Full Python (3.14 on the flagship), not a subset: the user explicitly rejected
//! an embedded partial implementation (RustPython). Compiled only where a
//! libpython is available (linux flagship); Windows seats run `.py` jobs through
//! the system Python instead (see scheduler::run_command). The `fleetd`
//! module surface is identical on both paths, so scripts are portable.
//!
//!     import fleetd
//!     fleetd.log("probe done")        # observational event into the ledger
//!     fleetd.alert("disk 95%")        # alert-kind event
//!     fleetd.seats()                  # [(seat, verdict)]
//!     fleetd.jobs()                   # [(name, schedule, enabled)]
//!     fleetd.run_job("resource-guard")# fire a job now
//!
//! GIL safety: every call into the interpreter takes the GIL via
//! `Python::attach` (PyO3 0.30); the interpreter is initialized once per
//! process. Registration injects the module into `sys` and `builtins` so a
//! bare `import fleetd` resolves.

// Compiled on every fleetd target: linux flagship links the system libpython,
// windows seats link python3.dll (abi3) and expect the bundled `runtime/`
// directory next to the exe (python3.dll + python315.dll + DLLs/ + Lib/).
// Three-tier fallback at init: bundled runtime -> system python3.dll ->
// caller runs `.py` jobs via the system python subprocess instead.

use anyhow::{Context, Result};
use pyo3::prelude::*;
use pyo3::types::PyModule;

/// Ledger access the hooks need. Implemented by a thin adapter over
/// `ledger::open()` + `ledger::ingest`, kept as a trait so tests can stub it.
pub trait FleetdApi: Send + Sync {
    fn log(&self, message: &str);
    fn alert(&self, message: &str);
    fn seats(&self) -> Vec<(String, String)>;
    fn jobs(&self) -> Vec<(String, String, bool)>;
    fn run_job(&self, name: &str) -> Result<i32>;
}

/// Global API pointer: the pymodule functions read this. Set once by
/// `init()` before any script runs.
static API: std::sync::OnceLock<Box<dyn FleetdApi>> = std::sync::OnceLock::new();

fn api() -> &'static dyn FleetdApi {
    // OnceLock<Box<dyn FleetdApi>>: get() returns Option<&Box<dyn FleetdApi>>;
    // deref the box to reach the inner &dyn FleetdApi.
    let b = API.get().expect("fleetd pyhost: init() not called");
    &**b
}

static MODULE: std::sync::OnceLock<pyo3::Py<pyo3::types::PyModule>> = std::sync::OnceLock::new();

/// Initialize the embedded interpreter and register the `fleetd` module.
/// Safe to call multiple times (subsequent calls are no-ops). With the
/// `auto-initialize` feature, the first `Python::attach` boots CPython.
pub fn init(api: Box<dyn FleetdApi>) -> Result<()> {
    if API.set(api).is_err() {
        return Ok(()); // already initialized
    }
    #[cfg(target_os = "windows")]
    configure_bundled_runtime();
    Python::attach(|py| -> PyResult<()> {
        let module = PyModule::new(py, "fleetd")?;
        module.add_function(wrap_pyfunction!(fleetd_log, &module)?)?;
        module.add_function(wrap_pyfunction!(fleetd_alert, &module)?)?;
        module.add_function(wrap_pyfunction!(fleetd_seats, &module)?)?;
        module.add_function(wrap_pyfunction!(fleetd_jobs, &module)?)?;
        module.add_function(wrap_pyfunction!(fleetd_run_job, &module)?)?;
        // sys.modules["fleetd"] is what the import machinery consults.
        // Keep an owned Py<> reference so the module object outlives this
        // attach scope (a Bound dropped here would leave an empty shell).
        let owned: Py<PyModule> = module.clone().unbind();
        let _ = MODULE.set(owned);
        let sys = py.import("sys")?;
        let modules = sys.getattr("modules")?;
        modules.set_item("fleetd", &module)?;
        Ok(())
    })
    .context("register fleetd module")?;
    Ok(())
}

/// Run a Python source string; returns captured stdout.
pub fn run_source(source: &str) -> Result<String> {
    Python::attach(|py| -> Result<String> {
        let io = py.import("io")?;
        let buf = io.call_method0("StringIO")?;
        let sys = py.import("sys")?;
        let old = sys.getattr("stdout")?;
        sys.setattr("stdout", &buf)?;
        let builtins = py.import("builtins")?;
        let exec = builtins.getattr("exec")?;
        let globals = pyo3::types::PyDict::new(py);
        globals.set_item("__name__", "fleetd_job")?;
        // exec() with a custom globals dict does not auto-populate
        // __builtins__ (the REPL does); without it the import machinery
        // fails to resolve modules.
        let builtins_mod = py.import("builtins")?;
        globals.set_item("__builtins__", &builtins_mod)?;
        let result = exec.call1((source, &globals));
        let out = buf.call_method0("getvalue")?.extract::<String>()?;
        sys.setattr("stdout", old)?;
        result.with_context(|| format!("python error; stdout so far:\n{out}"))?;
        Ok(out)
    })
}

// ---- pymodule functions: thin bridges to the API trait ----

#[pyfunction(name = "log")]
fn fleetd_log(message: &str) {
    api().log(message);
}

#[pyfunction(name = "alert")]
fn fleetd_alert(message: &str) {
    api().alert(message);
}

#[pyfunction(name = "seats")]
fn fleetd_seats() -> Vec<(String, String)> {
    api().seats()
}

#[pyfunction(name = "jobs")]
fn fleetd_jobs() -> Vec<(String, String, bool)> {
    api().jobs()
}

#[pyfunction(name = "run_job")]
fn fleetd_run_job(name: &str) -> PyResult<i32> {
    api().run_job(name)
        .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
}

/// Windows: point the interpreter at the bundled `runtime/` directory next
/// to the exe (python3.dll + DLLs/ + Lib/). No-op when the bundle is absent
/// (then python3.dll must come from PATH, or init fails and the scheduler
/// falls back to the system-python subprocess path).
#[cfg(target_os = "windows")]
fn configure_bundled_runtime() {
    use std::path::PathBuf;
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let runtime = exe.parent().map(|d| d.join("runtime"));
    let Some(runtime) = runtime else { return };
    if !runtime.join("python3.dll").exists() {
        return; // no bundle: let the loader resolve python3.dll from PATH
    }
    // DLL search: put runtime/ first so python3.dll is found without PATH games.
    // PYTHONHOME/PYTHONPATH teach the interpreter where Lib/ lives.
    unsafe {
        // SetDllDirectoryA: kernel32; keep the signature simple.
        #[link(name = "kernel32")]
        extern "system" {
            fn SetDllDirectoryA(path: *const u8) -> i32;
        }
        let mut c: Vec<u8> = runtime.as_os_str().to_string_lossy().bytes().collect();
        c.push(0);
        SetDllDirectoryA(c.as_ptr());
    }
    std::env::set_var("PYTHONHOME", &runtime);
    std::env::set_var("PYTHONPATH", runtime.join("Lib"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StubApi {
        logs: AtomicUsize,
    }
    impl FleetdApi for StubApi {
        fn log(&self, _m: &str) {
            self.logs.fetch_add(1, Ordering::SeqCst);
        }
        fn alert(&self, _m: &str) {}
        fn seats(&self) -> Vec<(String, String)> {
            vec![("coding".into(), "ok".into())]
        }
        fn jobs(&self) -> Vec<(String, String, bool)> {
            vec![("guard".into(), "*/30".into(), true)]
        }
        fn run_job(&self, name: &str) -> Result<i32> {
            Ok(if name == "guard" { 0 } else { 1 })
        }
    }

    #[test]
    fn python_runs_and_hooks_fire() {
        let logs = std::sync::Arc::new(AtomicUsize::new(0));
        // Wrap: the trait object needs 'static ownership; use a shared counter.
        struct Shared(std::sync::Arc<AtomicUsize>);
        impl FleetdApi for Shared {
            fn log(&self, _m: &str) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            fn alert(&self, _m: &str) {}
            fn seats(&self) -> Vec<(String, String)> {
                vec![("coding".into(), "ok".into())]
            }
            fn jobs(&self) -> Vec<(String, String, bool)> {
                vec![("guard".into(), "*/30".into(), true)]
            }
            fn run_job(&self, name: &str) -> Result<i32> {
                Ok(if name == "guard" { 0 } else { 1 })
            }
        }
        init(Box::new(Shared(logs.clone()))).unwrap();
        let out = run_source("import fleetd\nfleetd.log('hi')\nprint('py-ok')").unwrap();
        assert_eq!(out.trim(), "py-ok");
        assert!(logs.load(Ordering::SeqCst) >= 1, "hook must cross the FFI boundary");
    }

    #[test]
    fn seat_list_round_trips() {
        use std::sync::atomic::AtomicUsize;
        init(Box::new(StubApi { logs: AtomicUsize::new(0) })).unwrap();
        let out = run_source("import fleetd\nprint(fleetd.seats())").unwrap();
        assert!(out.contains("coding"), "seats visible from python: {out}");
    }
}
