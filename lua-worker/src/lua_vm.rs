use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex};

use lua_protocol::{LuaError, LuaValue, Response};
use mlua::{Lua, MultiValue, StdLib};

use crate::{conversion, limits, sandbox_io};

pub struct Vm {
    lua: Lua,
    gas_counter: Arc<AtomicI64>,
    console: Arc<Mutex<Vec<String>>>,
    /// `used_memory()` after init (stdlib, sandbox, hooks, print), with GC paused for a stable read.
    memory_baseline: usize,
}

impl Vm {
    pub fn new(sandbox_dir: &str) -> mlua::Result<Self> {
        let stdlib = StdLib::MATH
            | StdLib::STRING
            | StdLib::TABLE
            | StdLib::UTF8
            | StdLib::COROUTINE;
        let lua = Lua::new_with(stdlib, mlua::LuaOptions::default())?;
        sandbox_io::install(&lua, sandbox_dir)?;
        let gas_counter = limits::install(&lua, limits::DEFAULT_MEMORY, limits::DEFAULT_GAS)?;

        // fix this is a hack (design): Mutex blocks async task; print-heavy scripts can stall.
        let console: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let console_ref = Arc::clone(&console);
        lua.globals().set(
            "print",
            lua.create_function(move |lua_ctx, args: MultiValue| {
                let parts: Vec<String> = args
                    .iter()
                    .map(|v| {
                        lua_ctx
                            .coerce_string(v.clone())
                            .ok()
                            .flatten()
                            .and_then(|s| s.to_str().ok().map(|s| s.to_owned()))
                            .unwrap_or_else(|| format!("{:?}", v))
                    })
                    .collect();
                console_ref.lock().unwrap().push(parts.join("\t"));
                Ok(())
            })?,
        )?;

        lua.gc_stop();
        let memory_baseline = lua.used_memory();
        lua.gc_restart();

        Ok(Self { lua, gas_counter, console, memory_baseline })
    }

    pub async fn exec(&self, script: &str) -> Response {
        self.console.lock().unwrap().clear();

        let func = match self.lua.load(script).set_name("input").into_function() {
            Ok(f) => f,
            Err(e) => return Response::Error(conversion::mlua_err_to_protocol(e)),
        };
        self.run_thread(func, MultiValue::new()).await
    }

    pub async fn call(&self, function: &str, args: &[LuaValue]) -> Response {
        self.console.lock().unwrap().clear();

        let func: mlua::Function = match self.lua.globals().get(function) {
            Ok(f) => f,
            Err(_) => {
                return Response::Error(LuaError::Runtime {
                    message: format!("global '{}' is not a function", function),
                    traceback: None,
                })
            }
        };

        let lua_args: mlua::Result<MultiValue> = args
            .iter()
            .map(|v| conversion::protocol_to_lua(&self.lua, v))
            .collect();
        let lua_args = match lua_args {
            Ok(a) => a,
            Err(e) => return Response::Error(conversion::mlua_err_to_protocol(e)),
        };

        self.run_thread(func, lua_args).await
    }

    async fn run_thread(&self, func: mlua::Function, args: MultiValue) -> Response {
        let thread = match self.lua.create_thread(func) {
            Ok(t) => t,
            Err(e) => return Response::Error(conversion::mlua_err_to_protocol(e)),
        };

        let async_thread = match thread.into_async::<MultiValue>(args) {
            Ok(t) => t,
            Err(e) => return Response::Error(conversion::mlua_err_to_protocol(e)),
        };

        match async_thread.await {
            Ok(values) => match conversion::multi_to_protocol(&values) {
                Ok(values) => {
                    let console = self.console.lock().unwrap().clone();
                    Response::Ok {
                        values,
                        console,
                        gas_remaining: limits::gas_remaining(&self.gas_counter),
                        memory_used: self
                            .lua
                            .used_memory()
                            .saturating_sub(self.memory_baseline),
                    }
                }
                Err(e) => Response::Error(e),
            },
            Err(e) => Response::Error(conversion::mlua_err_to_protocol(e)),
        }
    }
}
