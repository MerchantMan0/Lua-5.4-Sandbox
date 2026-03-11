mod conversion;
mod limits;
mod lua_vm;
mod sandbox_io;

use std::os::unix::io::FromRawFd;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use lua_protocol::{codec, Request, Response};
use tokio::net::UnixStream;
use tokio::task::LocalSet;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let fd: i32 = args
        .next()
        .expect("usage: lua-worker <fd> <sandbox-dir>")
        .parse()
        .expect("fd must be an integer");
    let sandbox_dir = args
        .next()
        .expect("usage: lua-worker <fd> <sandbox-dir>");

    let stream = connect_fd(fd);

    let local = LocalSet::new();
    local.run_until(run(stream, sandbox_dir)).await;
}

fn connect_fd(fd: i32) -> UnixStream {
    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    std_stream.set_nonblocking(true).expect("set_nonblocking");
    UnixStream::from_std(std_stream).expect("wrap UnixStream")
}

async fn run(stream: UnixStream, sandbox_dir: String) {
    let vm = match lua_vm::Vm::new(&sandbox_dir) {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("lua-worker: failed to create Lua VM: {e}");
            return;
        }
    };

    let mut framed = codec::framed(stream);

    loop {
        let frame = match framed.next().await {
            Some(Ok(f)) => f,
            Some(Err(e)) => {
                eprintln!("lua-worker: framing error: {e}");
                break;
            }
            None => break,
        };

        let request: Request = match rmp_serde::from_slice(&frame) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("lua-worker: deserialize error: {e}");
                break;
            }
        };

        match request {
            Request::Shutdown => break,
            Request::Ping => {
                let bytes = rmp_serde::to_vec_named(&Response::Ok {
                    values: vec![],
                    console: vec![],
                    gas_remaining: 0,
                    memory_used: 0,
                })
                .expect("serialize response");
                if let Err(e) = framed.send(Bytes::from(bytes)).await {
                    eprintln!("lua-worker: send error: {e}");
                    break;
                }
            }
            req => {
                let response = handle(&vm, req).await;
                let bytes =
                    rmp_serde::to_vec_named(&response).expect("serialize response");
                if let Err(e) = framed.send(Bytes::from(bytes)).await {
                    eprintln!("lua-worker: send error: {e}");
                    break;
                }
            }
        }
    }
}

async fn handle(vm: &lua_vm::Vm, req: Request) -> Response {
    match req {
        Request::Exec { script } => vm.exec(&script).await,
        Request::Call { function, args } => vm.call(&function, &args).await,
        Request::Ping | Request::Shutdown => unreachable!(),
    }
}
