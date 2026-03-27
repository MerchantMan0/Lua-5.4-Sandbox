use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use mlua::{Debug, HookTriggers, Lua, VmState};

const GAS_CHUNK: u32 = 50;
// fix this is a hack: magic string overloads RuntimeError for gas detection.
// Lua code can not generate null bytes making this impossible to produce in lua.
const GAS_MARKER: &str = "\x00gas_exceeded";

pub const DEFAULT_GAS: i64 = 1_000_000;
pub const DEFAULT_MEMORY: usize = 128 * 1024 * 1024;
pub const SERIALIZATION_DEPTH: usize = 128;

pub fn is_gas_marker(msg: &str) -> bool {
    msg == GAS_MARKER
}

fn is_gas_error(e: &mlua::Error) -> bool {
    match e {
        mlua::Error::RuntimeError(msg) => is_gas_marker(msg),
        mlua::Error::CallbackError { cause, .. } => is_gas_error(cause),
        _ => false,
    }
}

pub fn install(lua: &Lua, memory_limit: usize, gas_budget: i64) -> mlua::Result<Arc<AtomicI64>> {
    lua.set_memory_limit(memory_limit)?;

    let counter = Arc::new(AtomicI64::new(chunks(gas_budget)));

    lua.set_global_hook(
        HookTriggers { every_nth_instruction: Some(GAS_CHUNK), ..Default::default() },
        make_hook(Arc::clone(&counter)),
    )?;

    // These functions catch RuntimeErrors and swallow the gas marker without wrapping.
    wrap_coroutine_resume(lua, Arc::clone(&counter))?;
    wrap_protected_call(lua, "pcall", Arc::clone(&counter))?;
    wrap_protected_call(lua, "xpcall", Arc::clone(&counter))?;
    Ok(counter)
}

// fix this is a hack (design): quantized by GAS_CHUNK, reported value is approximate.
pub fn gas_remaining(counter: &AtomicI64) -> i64 {
    counter.load(Ordering::Relaxed).max(0) * GAS_CHUNK as i64
}

fn chunks(instructions: i64) -> i64 {
    instructions / GAS_CHUNK as i64
}

fn make_hook(
    counter: Arc<AtomicI64>,
) -> impl Fn(&Lua, &Debug<'_>) -> mlua::Result<VmState> + Send + 'static {
    move |_lua, _debug| {
        if counter.fetch_sub(1, Ordering::Relaxed) <= 0 {
            Err(mlua::Error::RuntimeError(GAS_MARKER.to_string()))
        } else {
            Ok(VmState::Continue)
        }
    }
}

// fix this is a hack: monkey-patches coroutine.resume so gas is checked across yields (hook doesn't fire).
fn wrap_coroutine_resume(lua: &Lua, counter: Arc<AtomicI64>) -> mlua::Result<()> {
    let coroutine: mlua::Table = lua.globals().get("coroutine")?;

    let wrapped = lua.create_function(move |lua, args: mlua::MultiValue| {
        let mut args_iter = args.into_iter();
        let thread = match args_iter.next() {
            Some(mlua::Value::Thread(t)) => t,
            _ => return Err(mlua::Error::RuntimeError("coroutine.resume: expected a coroutine".into())),
        };
        let resume_args: mlua::MultiValue = args_iter.collect();

        match thread.resume::<mlua::MultiValue>(resume_args) {
            Ok(values) => {
                if counter.load(Ordering::Relaxed) < 0 {
                    return Err(mlua::Error::RuntimeError(GAS_MARKER.to_string()));
                }
                let mut result = mlua::MultiValue::new();
                result.push_back(mlua::Value::Boolean(true));
                result.extend(values);
                Ok(result)
            }
            Err(e) => {
                if is_gas_error(&e) {
                    return Err(mlua::Error::RuntimeError(GAS_MARKER.to_string()));
                }
                let msg = match &e {
                    mlua::Error::RuntimeError(s) => s.clone(),
                    other => other.to_string(),
                };
                let mut result = mlua::MultiValue::new();
                result.push_back(mlua::Value::Boolean(false));
                result.push_back(mlua::Value::String(lua.create_string(msg.as_bytes())?));
                Ok(result)
            }
        }
    })?;

    coroutine.set("resume", wrapped)?;
    Ok(())
}

// fix this is a hack: monkey-patches pcall/xpcall to check gas after protected call returns.
fn wrap_protected_call(lua: &Lua, name: &'static str, counter: Arc<AtomicI64>) -> mlua::Result<()> {
    let orig: mlua::Function = lua.globals().get(name)?;

    let wrapped = lua.create_function(move |_lua, args: mlua::MultiValue| {
        let results = orig.call::<mlua::MultiValue>(args)?;
        // the counter can go negative if the hook fires after budget is spent.
        // This isnt an issue but means the hook undercounts gas.
        // This can be mitagated by lowering the gas chunk size.
        // But increases hook firing cost and therefore decreases gas efficiency.
        // so don't fix.
        if counter.load(Ordering::Relaxed) < 0 {
            return Err(mlua::Error::RuntimeError(GAS_MARKER.to_string()));
        }
        Ok(results)
    })?;

    lua.globals().set(name, wrapped)?;
    Ok(())
}
