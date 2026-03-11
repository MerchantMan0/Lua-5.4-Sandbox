use std::io::{BufRead, Read};
use std::io::BufReader;
use std::sync::Arc;

use cap_std::fs::OpenOptions;
use mlua::Lua;

use super::error::SandboxIoError;

// Max buffer on reads to prevent oom
const MAX_READ_BYTES: usize = 4 * 1024 * 1024;

pub fn mode_to_opts(mode: &str) -> Option<OpenOptions> {
    let mode = mode.trim_end_matches('b');
    let mut opts = OpenOptions::new();
    match mode {
        "r" => {
            opts.read(true);
        }
        "w" => {
            opts.write(true).create(true).truncate(true);
        }
        "a" => {
            opts.write(true).create(true).append(true);
        }
        "r+" => {
            opts.read(true).write(true);
        }
        "w+" => {
            opts.read(true).write(true).create(true).truncate(true);
        }
        "a+" => {
            opts.read(true).write(true).create(true).append(true);
        }
        _ => return None,
    }
    Some(opts)
}

pub fn is_write_mode(mode: &str) -> bool {
    matches!(mode.trim_end_matches('b'), "w" | "a" | "r+" | "w+" | "a+")
}

pub fn read_one(
    lua: &Lua,
    reader: &mut BufReader<std::fs::File>,
    fmt: &mlua::Value,
    path: &str,
) -> mlua::Result<mlua::Value> {
    let io_err = |e: std::io::Error| {
        mlua::Error::ExternalError(Arc::new(SandboxIoError {
            path: path.to_string(),
            message: e.to_string(),
        }))
    };
    match fmt {
        mlua::Value::String(s) => {
            let s_str = s.to_str().map_err(|_| {
                mlua::Error::RuntimeError("read format must be a valid string".into())
            })?;
            // '*' prefix was standard in lua 5.1 and is still supported 
            // however modern versions do not require it but often support it
            // therefore we are for parity.
            let f = s_str.trim_start_matches('*');
            let over_limit = |len: usize| -> mlua::Result<()> {
                if len > MAX_READ_BYTES {
                    Err(mlua::Error::RuntimeError(format!(
                        "read exceeds maximum size ({MAX_READ_BYTES} bytes)"
                    )))
                } else {
                    Ok(())
                }
            };
            match f {
                "l" => {
                    let mut line = String::new();
                    match reader.by_ref().take(MAX_READ_BYTES as u64 + 1).read_line(&mut line) {
                        Ok(0) => Ok(mlua::Value::Nil),
                        Ok(_) => {
                            over_limit(line.len())?;
                            if line.ends_with('\n') {
                                line.pop();
                                if line.ends_with('\r') {
                                    line.pop();
                                }
                            }
                            Ok(mlua::Value::String(lua.create_string(&line)?))
                        }
                        Err(e) => Err(io_err(e)),
                    }
                }
                "L" => {
                    let mut line = String::new();
                    match reader.by_ref().take(MAX_READ_BYTES as u64 + 1).read_line(&mut line) {
                        Ok(0) => Ok(mlua::Value::Nil),
                        Ok(_) => {
                            over_limit(line.len())?;
                            Ok(mlua::Value::String(lua.create_string(&line)?))
                        }
                        Err(e) => Err(io_err(e)),
                    }
                }
                "a" => {
                    let mut buf = Vec::new();
                    reader.by_ref().take(MAX_READ_BYTES as u64 + 1).read_to_end(&mut buf).map_err(io_err)?;
                    over_limit(buf.len())?;
                    Ok(mlua::Value::String(lua.create_string(&buf)?))
                }
                "n" => {
                    let mut line = String::new();
                    reader.by_ref().take(MAX_READ_BYTES as u64 + 1).read_line(&mut line).map_err(io_err)?;
                    over_limit(line.len())?;
                    match line.trim().parse::<f64>() {
                        Ok(n) => Ok(mlua::Value::Number(n)),
                        Err(_) => Ok(mlua::Value::Nil),
                    }
                }
                _ => Err(mlua::Error::RuntimeError(format!(
                    "invalid read format '{f}'"
                ))),
            }
        }
        mlua::Value::Integer(n) => {
            if *n < 0 {
                return Err(mlua::Error::RuntimeError(
                    "bad argument #1 to 'read' (invalid count)".into(),
                ));
            }
            if *n == 0 {
                return Ok(mlua::Value::String(lua.create_string(b"")?));
            }
            let n = (*n as usize).min(MAX_READ_BYTES);
            let mut buf = vec![0u8; n];
            match reader.read(&mut buf) {
                Ok(0) => Ok(mlua::Value::Nil),
                Ok(read) => Ok(mlua::Value::String(lua.create_string(&buf[..read])?)),
                Err(e) => Err(io_err(e)),
            }
        }
        other => Err(mlua::Error::RuntimeError(format!(
            "invalid read format type '{}'",
            other.type_name()
        ))),
    }
}
