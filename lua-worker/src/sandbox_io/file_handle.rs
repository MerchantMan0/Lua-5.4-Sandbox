use std::io::{BufReader, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use mlua::{MultiValue, UserData, UserDataMethods};

use super::error::SandboxIoError;
use super::helpers::{is_write_mode, read_one};

pub enum Inner {
    Reader(BufReader<std::fs::File>),
    Writer(std::fs::File),
    ReadWriter(BufReader<std::fs::File>),
}

pub struct FileHandle {
    pub path: String,
    // None signals that close() has been called
    // subsequent calls should return a in error.
    pub inner: Arc<Mutex<Option<Inner>>>,
}

impl FileHandle {
    pub fn new(file: std::fs::File, mode: &str, path: String) -> Self {
        let inner = match mode.trim_end_matches('b') {
            "r+" | "w+" | "a+" => Inner::ReadWriter(BufReader::new(file)),
            m if is_write_mode(m) => Inner::Writer(file),
            _ => Inner::Reader(BufReader::new(file)),
        };
        FileHandle {
            path,
            inner: Arc::new(Mutex::new(Some(inner))),
        }
    }
}

impl UserData for FileHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "read",
            |lua, this, formats: mlua::Variadic<mlua::Value>| {
                let mut guard = this.inner.lock().unwrap();
                let inner = guard.as_mut().ok_or_else(|| {
                    mlua::Error::RuntimeError("attempt to use a closed file".into())
                })?;
                let reader = match inner {
                    Inner::Reader(r) | Inner::ReadWriter(r) => r,
                    Inner::Writer(_) => {
                        return Err(mlua::Error::RuntimeError(
                            "attempt to read from a write-mode file".into(),
                        ))
                    }
                };

                let fmts: Vec<mlua::Value> = if formats.is_empty() {
                    vec![mlua::Value::String(lua.create_string("l")?)]
                } else {
                    formats.into_iter().collect()
                };

                let mut results = MultiValue::new();
                for fmt in &fmts {
                    results.push_back(read_one(lua, reader, fmt, &this.path)?);
                }
                Ok(results)
            },
        );

        methods.add_method(
            "write",
            |_lua, this, args: mlua::Variadic<mlua::Value>| {
                let mut guard = this.inner.lock().unwrap();
                let inner = guard.as_mut().ok_or_else(|| {
                    mlua::Error::RuntimeError("attempt to use a closed file".into())
                })?;
                let writer: &mut dyn Write = match inner {
                    Inner::Writer(w) => w,
                    Inner::ReadWriter(rw) => rw.get_mut(),
                    Inner::Reader(_) => {
                        return Err(mlua::Error::RuntimeError(
                            "attempt to write to a read-mode file".into(),
                        ))
                    }
                };
                for arg in args {
                    let bytes: Vec<u8> = match arg {
                        mlua::Value::String(s) => s.as_bytes().to_vec(),
                        mlua::Value::Integer(i) => i.to_string().into_bytes(),
                        mlua::Value::Number(f) => f.to_string().into_bytes(),
                        other => {
                            return Err(mlua::Error::RuntimeError(format!(
                                "cannot write value of type '{}'",
                                other.type_name()
                            )))
                        }
                    };
                    writer.write_all(&bytes).map_err(|e| {
                        mlua::Error::ExternalError(Arc::new(SandboxIoError {
                            path: this.path.clone(),
                            message: e.to_string(),
                        }))
                    })?;
                }
                Ok(())
            },
        );

        methods.add_method("close", |_lua, this, ()| {
            *this.inner.lock().unwrap() = None;
            Ok(())
        });

        methods.add_method("flush", |_lua, this, ()| {
            let mut guard = this.inner.lock().unwrap();
            let inner = guard.as_mut().ok_or_else(|| {
                mlua::Error::RuntimeError("attempt to use a closed file".into())
            })?;
            let io_err = |e: std::io::Error| {
                mlua::Error::ExternalError(Arc::new(SandboxIoError {
                    path: this.path.clone(),
                    message: e.to_string(),
                }))
            };
            match inner {
                Inner::Writer(w) => w.flush().map_err(io_err)?,
                Inner::ReadWriter(rw) => rw.get_mut().flush().map_err(io_err)?,
                Inner::Reader(_) => {}
            }
            Ok(())
        });

        methods.add_method(
            "seek",
            |_lua, this, (whence, offset): (Option<String>, Option<i64>)| {
                let whence = whence.as_deref().unwrap_or("cur");
                let offset = offset.unwrap_or(0);

                let seek_from = match whence {
                    "set" => {
                        if offset < 0 {
                            return Err(mlua::Error::RuntimeError(
                                "cannot seek to negative position".into(),
                            ));
                        }
                        SeekFrom::Start(offset as u64)
                    }
                    "cur" => SeekFrom::Current(offset),
                    "end" => SeekFrom::End(offset),
                    other => {
                        return Err(mlua::Error::RuntimeError(format!(
                            "invalid seek whence '{other}'"
                        )))
                    }
                };

                let mut guard = this.inner.lock().unwrap();
                let inner = guard.as_mut().ok_or_else(|| {
                    mlua::Error::RuntimeError("attempt to use a closed file".into())
                })?;
                let io_err = |e: std::io::Error| {
                    mlua::Error::ExternalError(Arc::new(SandboxIoError {
                        path: this.path.clone(),
                        message: e.to_string(),
                    }))
                };
                // discards buffer and forwards to inner file
                let pos = match inner {
                    Inner::Reader(r) => r.seek(seek_from).map_err(io_err)?,
                    Inner::Writer(w) => w.seek(seek_from).map_err(io_err)?,
                    Inner::ReadWriter(rw) => rw.seek(seek_from).map_err(io_err)?,
                };
                Ok(pos as i64)
            },
        );

        methods.add_method("lines", |lua, this, ()| {
            let inner_arc = Arc::clone(&this.inner);
            let path = this.path.clone();
            lua.create_function(move |lua, ()| {
                let mut guard = inner_arc.lock().unwrap();
                let inner = guard.as_mut().ok_or_else(|| {
                    mlua::Error::RuntimeError("attempt to use a closed file".into())
                })?;
                let reader = match inner {
                    Inner::Reader(r) | Inner::ReadWriter(r) => r,
                    Inner::Writer(_) => {
                        return Err(mlua::Error::RuntimeError(
                            "attempt to read from a write-mode file".into(),
                        ))
                    }
                };
                let fmt = mlua::Value::String(lua.create_string("l")?);
                read_one(lua, reader, &fmt, &path)
            })
        });
    }
}
