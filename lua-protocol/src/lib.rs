pub mod codec;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LuaValue {
    Nil,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(Vec<u8>),
    // fix this is a hack (design): A Lua sequence and a Lua map with integer keys are indistinguishable.
    Table(Vec<(LuaValue, LuaValue)>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LuaError {
    Runtime {
        message: String,
        traceback: Option<String>,
    },
    Syntax(String),
    Io { path: String, message: String },
    GasExceeded,
    MemoryExceeded,
    SerializationDepthExceeded,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    Exec { script: String },
    Call { function: String, args: Vec<LuaValue> },
    Ping,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    // fix this is a hack (design): single struct for all Ok cases forces fake gas/memory for Ping/Shutdown.
    Ok {
        values: Vec<LuaValue>,
        console: Vec<String>,
        gas_remaining: i64,
        memory_used: usize,
    },
    Error(LuaError),
}
