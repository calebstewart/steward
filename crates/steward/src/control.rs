//! The control plane: a thread that serves the pipe (see `steward_ipc::pipe`),
//! one client at a time, handing each request to the manager's loop and
//! waiting for its answer. The manager's state stays on the manager's thread.

use std::io;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use steward_ipc::pipe::Server;
use steward_ipc::Response;

use crate::log::error;
use crate::manager::{Control, KEY_WAKE};
use crate::sys::port::Waker;

/// How long a client waits for the manager to answer.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

/// Create the pipe -- failing if another manager holds it -- and serve it.
pub fn listen(controls: Sender<Control>, waker: Waker) -> io::Result<()> {
    let server = Server::create()?;
    std::thread::Builder::new()
        .name("control".into())
        .spawn(move || loop {
            let served = server.serve_one(|request| match request {
                Err(message) => Response::error(message),
                Ok(request) => {
                    let (reply, answer) = mpsc::channel();
                    if controls.send(Control::Request(request, reply)).is_err() {
                        return Response::error("steward is shutting down");
                    }
                    let _ = waker.post(KEY_WAKE, 0, 0);
                    answer
                        .recv_timeout(ANSWER_TIMEOUT)
                        .unwrap_or_else(|_| Response::error("steward did not answer in time"))
                }
            });
            if let Err(e) = served {
                error!("serving a control client: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        })?;
    Ok(())
}
