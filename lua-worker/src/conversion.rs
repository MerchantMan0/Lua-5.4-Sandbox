use lua_protocol::{LuaError, LuaValue};
use mlua::{Lua, MultiValue};

use crate::{limits, sandbox_io};

pub fn lua_to_protocol(value: &mlua::Value) -> Result<LuaValue, LuaError> {
    lua_to_protocol_depth(value, limits::SERIALIZATION_DEPTH)
}

fn lua_to_protocol_depth(value: &mlua::Value, depth: usize) -> Result<LuaValue, LuaError> {
    match value {
        mlua::Value::Nil => Ok(LuaValue::Nil),
        mlua::Value::Boolean(b) => Ok(LuaValue::Bool(*b)),
        mlua::Value::Integer(i) => Ok(LuaValue::Integer(*i)),
        mlua::Value::Number(f) => Ok(LuaValue::Float(*f)),
        mlua::Value::String(s) => Ok(LuaValue::String(s.as_bytes().to_vec())),
        mlua::Value::Table(_) if depth == 0 => Err(LuaError::SerializationDepthExceeded),
        mlua::Value::Table(t) => {
            let mut pairs = Vec::new();
            for pair in t.pairs::<mlua::Value, mlua::Value>() {
                let (k, v) = pair.map_err(|e| LuaError::Runtime {
                    message: e.to_string(),
                    traceback: None,
                })?;
                pairs.push((
                    lua_to_protocol_depth(&k, depth - 1)?,
                    lua_to_protocol_depth(&v, depth - 1)?,
                ));
            }
            Ok(LuaValue::Table(pairs))
        }
        other => Err(LuaError::Runtime {
            message: format!("cannot serialize Lua value of type '{}'", other.type_name()),
            traceback: None,
        }),
    }
}

pub(crate) fn multi_to_protocol(values: &MultiValue) -> Result<Vec<LuaValue>, LuaError> {
    values.iter().map(lua_to_protocol).collect()
}

pub fn protocol_to_lua(lua: &Lua, value: &LuaValue) -> mlua::Result<mlua::Value> {
    match value {
        LuaValue::Nil => Ok(mlua::Value::Nil),
        LuaValue::Bool(b) => Ok(mlua::Value::Boolean(*b)),
        LuaValue::Integer(i) => Ok(mlua::Value::Integer(*i)),
        LuaValue::Float(f) => Ok(mlua::Value::Number(*f)),
        LuaValue::String(s) => Ok(mlua::Value::String(lua.create_string(s)?)),
        LuaValue::Table(pairs) => {
            let table = lua.create_table()?;
            for (k, v) in pairs {
                table.set(protocol_to_lua(lua, k)?, protocol_to_lua(lua, v)?)?;
            }
            Ok(mlua::Value::Table(table))
        }
    }
}

pub fn mlua_err_to_protocol(err: mlua::Error) -> LuaError {
    match &err {
        mlua::Error::MemoryError(_) => LuaError::MemoryExceeded,
        mlua::Error::SyntaxError { message, .. } => LuaError::Syntax(message.clone()),
        mlua::Error::RuntimeError(msg) if limits::is_gas_marker(msg) => LuaError::GasExceeded,
        // fix this is a hack: mlua RuntimeError has no traceback; only CallbackError provides it.
        mlua::Error::RuntimeError(msg) => LuaError::Runtime {
            message: msg.clone(),
            traceback: None,
        },
        mlua::Error::ExternalError(e) => {
            if let Some(io) = e.downcast_ref::<sandbox_io::SandboxIoError>() {
                LuaError::Io { path: io.path.clone(), message: io.message.clone() }
            } else {
                LuaError::Runtime { message: e.to_string(), traceback: None }
            }
        }
        mlua::Error::CallbackError { cause, traceback } => {
            let inner = mlua_err_to_protocol((**cause).clone());
            match inner {
                LuaError::Runtime { message, .. } => LuaError::Runtime {
                    message,
                    traceback: Some(traceback.clone()),
                },
                other => other,
            }
        }
        other => LuaError::Runtime {
            message: other.to_string(),
            traceback: None,
        },
    }
}
