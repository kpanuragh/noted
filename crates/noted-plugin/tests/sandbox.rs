//! M5-4 — plugin isolation.
//!
//! Fixtures are WebAssembly TEXT so a reader can audit exactly what each plugin
//! tries to do, instead of trusting an opaque byte array.
use std::sync::Arc;

use noted_plugin::{DEFAULT_FUEL, HostApi, Plugin, PluginError};
use uuid::Uuid;

struct TestHost {
    workspace: Uuid,
}

impl HostApi for TestHost {
    fn workspace_id(&self) -> Uuid {
        self.workspace
    }
    fn get_setting(&self, _key: &str) -> Option<String> {
        None
    }
    fn log(&self, _message: &str) {}
}

fn host() -> Arc<dyn HostApi> {
    Arc::new(TestHost {
        workspace: Uuid::new_v4(),
    })
}

fn compile(text: &str) -> Vec<u8> {
    wat::parse_str(text).expect("the fixture must be valid wasm text")
}

/// A well-behaved plugin runs and returns its value.
#[test]
fn a_well_behaved_plugin_runs() {
    let wasm = compile(r#"(module (func (export "run") (result i32) i32.const 42))"#);
    let out = Plugin::load(&wasm).unwrap().call("run", host(), DEFAULT_FUEL).unwrap();
    assert_eq!(out.value, Some(42));
}

/// **A crashing plugin degrades one block, not the page.**
///
/// A trap comes back as `Err` for the caller to render around. If it escaped as
/// a panic the whole request would die, which is the failure this issue names.
#[test]
fn a_crashing_plugin_does_not_take_down_the_host() {
    let wasm = compile(r#"(module (func (export "run") (result i32) unreachable))"#);
    let err = Plugin::load(&wasm).unwrap().call("run", host(), DEFAULT_FUEL).unwrap_err();
    assert!(matches!(err, PluginError::Trap(_)), "got {err}");

    // And the host is still perfectly usable afterwards.
    let good = compile(r#"(module (func (export "run") (result i32) i32.const 7))"#);
    assert_eq!(
        Plugin::load(&good).unwrap().call("run", host(), DEFAULT_FUEL).unwrap().value,
        Some(7)
    );
}

/// **An infinite loop is stopped by fuel, deterministically.**
///
/// A wall-clock timeout would need another thread and could not promise the
/// same plugin costs the same twice. Fuel traps at a predictable instruction
/// count however loaded the machine is.
#[test]
fn an_infinite_loop_is_stopped_by_fuel() {
    let wasm = compile(
        r#"(module (func (export "run") (result i32) (loop $l (br $l)) i32.const 0))"#,
    );
    let started = std::time::Instant::now();
    let err = Plugin::load(&wasm).unwrap().call("run", host(), 100_000).unwrap_err();
    assert!(matches!(err, PluginError::Trap(_)), "got {err}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "fuel must stop it promptly, took {:?}",
        started.elapsed()
    );
}

/// **A plugin cannot make network calls — because it cannot import anything the
/// host did not hand it.**
///
/// This fails at INSTANTIATION, loudly, rather than at runtime. There is no
/// dynamic resolution and no fallback: an unknown import is simply not
/// satisfiable.
#[test]
fn a_plugin_cannot_import_anything_the_host_did_not_provide() {
    for (what, module, name) in [
        ("a socket", "wasi_snapshot_preview1", "sock_send"),
        ("the filesystem", "wasi_snapshot_preview1", "fd_write"),
        ("an invented host call", "noted", "read_any_workspace"),
    ] {
        let wasm = compile(&format!(
            r#"(module (import "{module}" "{name}" (func $f (param i32) (result i32)))
                       (func (export "run") (result i32) i32.const 0))"#
        ));
        let err = Plugin::load(&wasm)
            .unwrap()
            .call("run", host(), DEFAULT_FUEL)
            .unwrap_err();
        assert!(
            matches!(err, PluginError::Instantiate(_)),
            "{what} must be unavailable, got {err}"
        );
    }
}

/// The one host function that IS provided works, and the plugin's output is
/// captured for the caller.
#[test]
fn the_provided_host_function_works() {
    let wasm = compile(
        r#"(module
             (import "noted" "log" (func $log (param i32 i32)))
             (memory (export "memory") 1)
             (data (i32.const 0) "hello from a plugin")
             (func (export "run") (result i32)
               (call $log (i32.const 0) (i32.const 19))
               i32.const 1))"#,
    );
    let out = Plugin::load(&wasm).unwrap().call("run", host(), DEFAULT_FUEL).unwrap();
    assert_eq!(out.value, Some(1));
    assert_eq!(out.logs, vec!["hello from a plugin"]);
}

/// **A plugin cannot read outside its own memory.**
///
/// It can pass any pointer it likes to a host function, including one far
/// outside its linear memory. The host must bounds-check rather than
/// dereference — a plugin reaching into host memory would defeat every other
/// guarantee here at once.
#[test]
fn a_plugin_cannot_read_outside_its_own_memory() {
    let wasm = compile(
        r#"(module
             (import "noted" "log" (func $log (param i32 i32)))
             (memory (export "memory") 1)
             (func (export "run") (result i32)
               ;; far past the end of one 64KiB page
               (call $log (i32.const 999999) (i32.const 128))
               i32.const 2))"#,
    );
    let out = Plugin::load(&wasm).unwrap().call("run", host(), DEFAULT_FUEL).unwrap();
    assert_eq!(out.value, Some(2), "the call itself must not crash the host");
    assert_eq!(
        out.logs,
        vec![""],
        "an out-of-bounds read yields nothing, never host memory"
    );
}

/// A missing export is an error, not a panic — a plugin built against an older
/// host contract must degrade rather than crash.
#[test]
fn a_missing_export_is_an_error() {
    let wasm = compile(r#"(module (func (export "other") (result i32) i32.const 0))"#);
    let err = Plugin::load(&wasm).unwrap().call("run", host(), DEFAULT_FUEL).unwrap_err();
    assert!(matches!(err, PluginError::MissingExport(_)), "got {err}");
}

/// Garbage bytes fail to load rather than doing anything interesting.
#[test]
fn a_malformed_module_fails_to_load() {
    match Plugin::load(&[0xde, 0xad, 0xbe, 0xef]) {
        Err(PluginError::Load(_)) => {}
        Err(other) => panic!("expected a load error, got {other}"),
        Ok(_) => panic!("garbage bytes must not load"),
    }
}

/// **A plugin cannot name a workspace at all**, which is what makes
/// cross-workspace reads impossible rather than merely forbidden.
///
/// The host binds one `workspace_id` at construction and host functions close
/// over it; there is no import that accepts a workspace argument, so there is
/// nothing for a plugin to forge.
#[test]
fn a_plugin_has_no_way_to_name_a_workspace() {
    let ws = Uuid::new_v4();
    let bound: Arc<dyn HostApi> = Arc::new(TestHost { workspace: ws });
    assert_eq!(bound.workspace_id(), ws);

    // The host surface is exactly one function, and it takes a string — not a
    // workspace. Anything else fails to instantiate (asserted above).
    let wasm = compile(
        r#"(module
             (import "noted" "log" (func $log (param i32 i32)))
             (memory (export "memory") 1)
             (func (export "run") (result i32) i32.const 0))"#,
    );
    let out = Plugin::load(&wasm).unwrap().call("run", bound, DEFAULT_FUEL);
    assert!(out.is_ok(), "the bound host must still work");
}
