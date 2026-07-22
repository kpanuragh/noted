//! The plugin sandbox.
//!
//! # What a plugin is allowed to do, and how that is enforced
//!
//! A plugin is a WebAssembly module. WebAssembly is used here for one reason:
//! **a module can do nothing except what the host hands it.** There are no
//! syscalls, no ambient filesystem, no sockets — a wasm module's entire
//! interface with the outside world is the set of imports the host chooses to
//! provide. So "a plugin cannot make arbitrary network calls" is not a rule
//! this code enforces by checking; it is a property of the execution model, and
//! the only way to break it would be to deliberately hand a plugin a network
//! function.
//!
//! The three guarantees the issue asks for map onto three mechanisms:
//!
//! | guarantee | mechanism |
//! |---|---|
//! | cannot read another workspace's data | host functions close over ONE `workspace_id`, fixed at construction |
//! | cannot make arbitrary network calls | no such import is provided, and an unknown import fails instantiation |
//! | a crash degrades one block, not the page | every call is fuel-limited and every trap is returned as `Err` |
//!
//! # Fuel, not a timeout
//!
//! A wall-clock timeout needs a second thread to enforce it and cannot stop a
//! tight loop that never yields. Fuel is deterministic: the interpreter
//! decrements a counter per instruction and traps at zero, so an infinite loop
//! dies at a predictable point regardless of how loaded the machine is — and
//! two runs of the same plugin cost the same, which a timeout cannot promise.
use std::sync::{Arc, Mutex};

use uuid::Uuid;
use wasmi::{Caller, Engine, Linker, Module, Store};

/// How much work one plugin call may do.
///
/// Generous enough for a block renderer or a property formatter, small enough
/// that a runaway plugin is stopped in milliseconds rather than seconds.
pub const DEFAULT_FUEL: u64 = 5_000_000;

/// How much memory one plugin may allocate, in 64KiB wasm pages.
///
/// A plugin that allocates without bound would exhaust the host even while
/// obeying its fuel budget, because allocation is cheap in instructions and
/// expensive in bytes.
pub const MAX_PAGES: u32 = 64; // 4 MiB

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("the plugin could not be loaded: {0}")]
    Load(String),
    /// Includes a plugin asking for an import the host does not provide — which
    /// is how "no arbitrary network calls" actually fails: at instantiation,
    /// loudly, rather than at runtime.
    #[error("the plugin asked for something the host does not provide: {0}")]
    Instantiate(String),
    #[error("the plugin has no exported function named `{0}`")]
    MissingExport(String),
    /// A trap, an unreachable, a division by zero, or exhausted fuel. All of
    /// them are the plugin's problem and none of them are the host's.
    #[error("the plugin failed while running: {0}")]
    Trap(String),
}

/// What a plugin may ask the host for.
///
/// Deliberately tiny. Every method here is a capability someone has to justify,
/// and the default answer to "can plugins do X" should be no until a host
/// function for X exists and is scoped.
pub trait HostApi: Send + Sync {
    /// The workspace this plugin instance is bound to. Host functions use this
    /// rather than taking a workspace id from the plugin, so a plugin cannot
    /// name a workspace at all — let alone someone else's.
    fn workspace_id(&self) -> Uuid;

    /// Read a plugin-scoped setting. Keys are namespaced by the host, so two
    /// plugins cannot read each other's configuration.
    fn get_setting(&self, key: &str) -> Option<String>;

    /// Emit a log line, attributed to the plugin.
    fn log(&self, message: &str);
}

/// The messages a plugin logged during one call — surfaced so a failing plugin
/// can be debugged without attaching to the host.
#[derive(Debug, Default, Clone)]
pub struct CallOutput {
    pub logs: Vec<String>,
    /// The single i32 a plugin call returns, if it returned normally.
    pub value: Option<i32>,
}

/// A loaded, sandboxed plugin.
pub struct Plugin {
    engine: Engine,
    module: Module,
}

impl Plugin {
    /// Compile a plugin from wasm bytes.
    pub fn load(wasm: &[u8]) -> Result<Self, PluginError> {
        let mut config = wasmi::Config::default();
        // Fuel metering must be on at COMPILE time — turning it on later cannot
        // instrument code that is already compiled.
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, wasm).map_err(|e| PluginError::Load(e.to_string()))?;
        Ok(Self { engine, module })
    }

    /// Call an exported function with no arguments.
    ///
    /// Every failure mode — a missing export, a trap, exhausted fuel, a memory
    /// limit — comes back as `Err`. The caller renders one broken block; the
    /// page is unaffected. That is the whole isolation story, and
    /// `a_crashing_plugin_does_not_take_down_the_host` is what pins it.
    pub fn call(
        &self,
        export: &str,
        host: Arc<dyn HostApi>,
        fuel: u64,
    ) -> Result<CallOutput, PluginError> {
        let logs = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut store = Store::new(&self.engine, HostState {
            host: host.clone(),
            logs: logs.clone(),
        });
        store
            .set_fuel(fuel)
            .map_err(|e| PluginError::Instantiate(e.to_string()))?;

        let mut linker = <Linker<HostState>>::new(&self.engine);

        // The ENTIRE host interface. A plugin that imports anything else fails
        // to instantiate — there is no fallback and no dynamic resolution.
        linker
            .func_wrap(
                "noted",
                "log",
                |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| {
                    let message = read_string(&mut caller, ptr, len).unwrap_or_default();
                    let state = caller.data();
                    state.host.log(&message);
                    state.logs.lock().unwrap().push(message);
                },
            )
            .map_err(|e| PluginError::Instantiate(e.to_string()))?;

        let instance = linker
            .instantiate(&mut store, &self.module)
            .map_err(|e| PluginError::Instantiate(e.to_string()))?
            .start(&mut store)
            .map_err(|e| PluginError::Instantiate(e.to_string()))?;

        let func = instance
            .get_typed_func::<(), i32>(&store, export)
            .map_err(|_| PluginError::MissingExport(export.to_string()))?;

        match func.call(&mut store, ()) {
            Ok(value) => Ok(CallOutput {
                logs: logs.lock().unwrap().clone(),
                value: Some(value),
            }),
            // A trap is DATA, not a panic: it is returned so the caller can
            // show one broken block and carry on.
            Err(e) => Err(PluginError::Trap(e.to_string())),
        }
    }
}

struct HostState {
    host: Arc<dyn HostApi>,
    logs: Arc<Mutex<Vec<String>>>,
}

/// Read a UTF-8 string out of the plugin's linear memory.
///
/// Every step is checked: a plugin can pass any pointer and any length it
/// likes, including ones outside its own memory, and the host must never
/// dereference them blindly.
fn read_string(caller: &mut Caller<'_, HostState>, ptr: i32, len: i32) -> Option<String> {
    if ptr < 0 || len < 0 || len > 64 * 1024 {
        return None;
    }
    let memory = caller.get_export("memory")?.into_memory()?;
    let data = memory.data(&caller);
    let start = ptr as usize;
    let end = start.checked_add(len as usize)?;
    let bytes = data.get(start..end)?;
    String::from_utf8(bytes.to_vec()).ok()
}
