mod error;
mod file_handle;
mod helpers;

pub use error::SandboxIoError;

use std::io::BufReader;
use std::sync::{Arc, Mutex};

use cap_std::fs::Dir;
use mlua::{Lua, MultiValue};

use file_handle::FileHandle;
use helpers::{mode_to_opts, read_one};

pub fn install(lua: &Lua, sandbox_dir: &str) -> mlua::Result<()> {
    let dir = Dir::open_ambient_dir(sandbox_dir, cap_std::ambient_authority())
        .map_err(|e| mlua::Error::RuntimeError(format!("sandbox: open dir: {e}")))?;
    let dir = Arc::new(dir);

    let io_table = lua.create_table()?;

    let dir_open = Arc::clone(&dir);
    io_table.set(
        "open",
        lua.create_function(move |lua, (path, mode): (String, Option<String>)| {
            let mode_str = mode.as_deref().unwrap_or("r");
            let opts = match mode_to_opts(mode_str) {
                Some(o) => o,
                None => {
                    let mut mv = MultiValue::new();
                    mv.push_back(mlua::Value::Nil);
                    mv.push_back(mlua::Value::String(
                        lua.create_string(format!("invalid mode: '{mode_str}'"))?,
                    ));
                    return Ok(mv);
                }
            };
            match dir_open.open_with(&path, &opts) {
                Ok(file) => {
                    let handle = FileHandle::new(file.into_std(), mode_str, path.clone());
                    let mut mv = MultiValue::new();
                    mv.push_back(lua.pack(handle)?);
                    Ok(mv)
                }
                Err(e) => {
                    let mut mv = MultiValue::new();
                    mv.push_back(mlua::Value::Nil);
                    mv.push_back(mlua::Value::String(
                        lua.create_string(
                            SandboxIoError { path: path.clone(), message: e.to_string() }
                                .to_string(),
                        )?,
                    ));
                    Ok(mv)
                }
            }
        })?,
    )?;

    let dir_lines = Arc::clone(&dir);
    io_table.set(
        "lines",
        lua.create_function(move |lua, filename: String| {
            let opts = mode_to_opts("r").unwrap();
            let file = dir_lines.open_with(&filename, &opts).map_err(|e| {
                mlua::Error::ExternalError(Arc::new(SandboxIoError {
                    path: filename.clone(),
                    message: e.to_string(),
                }))
            })?;
            let reader = Arc::new(Mutex::new(Some(BufReader::new(file.into_std()))));
            let path = filename.clone();
            // The iterator auto-closes the file when EOF is reached, matching
            // standard Lua io.lines(filename) behaviour.
            lua.create_function(move |lua, ()| {
                let mut guard = reader.lock().unwrap();
                if guard.is_none() {
                    return Ok(mlua::Value::Nil);
                }
                let r = guard.as_mut().unwrap();
                let fmt = mlua::Value::String(lua.create_string("l")?);
                let value = read_one(lua, r, &fmt, &path)?;
                if matches!(value, mlua::Value::Nil) {
                    *guard = None;
                }
                Ok(value)
            })
        })?,
    )?;

    let sandbox_dir_str = sandbox_dir.to_string();
    io_table.set(
        "tmpfile",
        lua.create_function(move |lua, ()| {
            let file = tempfile::tempfile_in(&sandbox_dir_str)
                .map_err(|e| mlua::Error::RuntimeError(format!("io.tmpfile: {e}")))?;
            let handle = FileHandle::new(file, "w+", "[tmpfile]".to_string());
            lua.pack(handle)
        })?,
    )?;

    lua.globals().set("io", io_table)?;
    Ok(())
}
