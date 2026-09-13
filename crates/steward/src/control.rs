//! The control plane: a thread that serves the pipe (see `steward_ipc::pipe`),
//! one client at a time, handing each request to the manager's loop and
//! waiting for its answer. The manager's state stays on the manager's thread.

use std::io;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use steward_ipc::pipe::{self, ClientError, Server};
use steward_ipc::Response;

use crate::log::error;
use crate::manager::{Control, KEY_WAKE};
use crate::sys::port::Waker;

/// How long a client waits for the manager to answer.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

/// Why the pipe is not being served.
pub enum Refusal {
    /// Another manager of this user serves the session already.
    Held,
    /// The name is taken by something that is not a manager of this user's,
    /// or that cannot be shown to be one: what was found out. Worth trying
    /// again later.
    Taken(String),
    /// Something that trying again will not mend.
    Failed(io::Error),
}

/// Create the pipe -- failing if another manager holds it -- and serve it.
pub fn listen(controls: Sender<Control>, waker: Waker) -> Result<(), Refusal> {
    let server = match Server::create() {
        Ok(server) => server,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // The name is public: whose is it?
            return Err(match pipe::probe() {
                Ok(()) => Refusal::Held,
                Err(ClientError::Impostor(who)) => {
                    Refusal::Taken(format!("it belongs to another user ({who})"))
                }
                Err(ClientError::NotRunning) => Refusal::Taken("it has just gone".into()),
                Err(ClientError::Io(e)) => {
                    Refusal::Taken(format!("whose it is cannot be told: {e}"))
                }
                Err(e) => Refusal::Taken(format!("whose it is cannot be told: {e}")),
            });
        }
        Err(e) => return Err(Refusal::Failed(e)),
    };
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
        })
        .map_err(Refusal::Failed)?;
    Ok(())
}
