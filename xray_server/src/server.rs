use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::rc::Rc;

use bytes::Bytes;
use futures::{future, stream, Future, IntoFuture, Sink, Stream};
use futures_cpupool::CpuPool;
use tokio_core::net::{TcpListener, TcpStream};
use tokio_core::reactor;
use tokio_io::codec;
use uuid;
use xray_core::app::Command;
use xray_core::{self, App, Never, WindowId};

use database;
use fs;
use git;
use messages::{IncomingMessage, OutgoingMessage};

#[derive(Clone)]
pub struct Server {
    app: Rc<RefCell<xray_core::App>>,
    providers: Rc<RefCell<HashMap<PathBuf, Rc<git::GitProvider>>>>,
    databases: Rc<RefCell<HashMap<PathBuf, Rc<database::Database>>>>,
    reactor: reactor::Handle,
}

impl Server {
    pub fn new(headless: bool, reactor: reactor::Handle) -> Self {
        let foreground = Rc::new(reactor.clone());
        let background = Rc::new(CpuPool::new_num_cpus());
        Server {
            app: App::new(headless, foreground, background),
            providers: Rc::new(RefCell::new(HashMap::new())),
            databases: Rc::new(RefCell::new(HashMap::new())),
            reactor,
        }
    }

    pub fn accept_connection<'a, S>(&mut self, socket: S)
    where
        S: 'static
            + Stream<Item = IncomingMessage, Error = io::Error>
            + Sink<SinkItem = OutgoingMessage>,
    {
        let (outgoing, incoming) = socket.split();
        let server = self.clone();
        self.reactor.spawn(
            incoming
                .into_future()
                .map(move |(first_message, incoming)| {
                    first_message.map(|first_message| match first_message {
                        IncomingMessage::StartApp => {
                            server.start_app(outgoing, incoming);
                        }
                        IncomingMessage::StartCli { headless } => {
                            server.start_cli(outgoing, incoming, headless);
                        }
                        IncomingMessage::StartWindow { window_id, height } => {
                            server.start_window(outgoing, incoming, window_id, height);
                        }
                        _ => eprintln!("Unexpected message {:?}", first_message),
                    });
                })
                .then(|_| Ok(())),
        );
    }

    fn start_app<O, I>(&self, outgoing: O, incoming: I)
    where
        O: 'static + Sink<SinkItem = OutgoingMessage>,
        I: 'static + Stream<Item = IncomingMessage, Error = io::Error>,
    {
        if self.app.borrow().headless() {
            self.send_outgoing(
                outgoing,
                stream::once(Ok(OutgoingMessage::Error {
                    description: "This is a headless application instance".into(),
                })),
            );
        } else {
            if let Some(commands) = self.app.borrow_mut().commands() {
                let server = self.clone();
                let outgoing_commands = commands.map(|update| match update {
                    Command::OpenWindow(window_id) => OutgoingMessage::OpenWindow { window_id },
                });
                let outgoing_responses = report_input_errors(incoming.and_then(move |message| {
                    server
                        .handle_app_message(message)
                        .map_err(|_| unreachable!())
                }));
                self.send_outgoing(outgoing, outgoing_commands.select(outgoing_responses));
            } else {
                self.send_outgoing(
                    outgoing,
                    stream::once(Ok(OutgoingMessage::Error {
                        description: "An application client is already registered".into(),
                    })),
                );
            }
        }
    }

    fn start_cli<O, I>(&self, outgoing: O, incoming: I, headless: bool)
    where
        O: 'static + Sink<SinkItem = OutgoingMessage>,
        I: 'static + Stream<Item = IncomingMessage, Error = io::Error>,
    {
        match (self.app.borrow().headless(), headless) {
            (true, false) => {
                return self.send_outgoing(outgoing, stream::once(Ok(OutgoingMessage::Error {
                    description: "Since Xray was initially started with --headless, all subsequent commands must be --headless".into()
                })));
            }
            (false, true) => {
                return self.send_outgoing(outgoing, stream::once(Ok(OutgoingMessage::Error {
                    description: "Since Xray was initially started without --headless, no subsequent commands may be --headless".into()
                })));
            }
            _ => {}
        }

        let server = self.clone();
        let outgoing_ack = stream::once(Ok(OutgoingMessage::Ok));
        let outgoing_responses = report_input_errors(incoming.and_then(move |message| {
            server
                .handle_app_message(message)
                .map_err(|_| unreachable!())
        }));
        self.send_outgoing(outgoing, outgoing_ack.chain(outgoing_responses));
    }

    pub fn start_window<O, I>(&self, outgoing: O, incoming: I, window_id: WindowId, height: f64)
    where
        O: 'static + Sink<SinkItem = OutgoingMessage>,
        I: 'static + Stream<Item = IncomingMessage, Error = io::Error>,
    {
        let server = self.clone();
        let receive_incoming = incoming
            .for_each(move |message| {
                server.handle_window_message(window_id, message);
                Ok(())
            })
            .then(|_| Ok(()));
        self.reactor.spawn(receive_incoming);

        match self.app.borrow_mut().start_window(&window_id, height) {
            Ok(updates) => {
                self.send_outgoing(
                    outgoing,
                    updates.map(|update| OutgoingMessage::UpdateWindow(update)),
                );
            }
            Err(_) => {
                self.send_outgoing(
                    outgoing,
                    stream::once(Ok(OutgoingMessage::Error {
                        description: format!("No window exists for id {}", window_id),
                    })),
                );
            }
        }
    }

    fn handle_app_message(
        &self,
        message: IncomingMessage,
    ) -> Box<Future<Item = OutgoingMessage, Error = Never>> {
        let result = match message {
            IncomingMessage::OpenWorkspace { paths } => {
                Box::new(self.open_workspace(paths).into_future())
            }
            IncomingMessage::TcpListen { port } => Box::new(self.tcp_listen(port).into_future()),
            IncomingMessage::ConnectToPeer { address } => self.connect_to_peer(address),
            IncomingMessage::CloseWindow { window_id } => {
                Box::new(self.close_window(window_id).into_future())
            }
            _ => Box::new(future::err(format!("Unexpected message {:?}", message))),
        };

        Box::new(result.then(|result| match result {
            Ok(_) => Ok(OutgoingMessage::Ok),
            Err(description) => Ok(OutgoingMessage::Error { description }),
        }))
    }

    fn handle_window_message(&self, window_id: WindowId, message: IncomingMessage) {
        match message {
            IncomingMessage::Action { view_id, action } => {
                self.app
                    .borrow_mut()
                    .dispatch_action(window_id, view_id, action);
            }
            _ => {
                eprintln!("Unexpected message {:?}", message);
            }
        }
    }

    fn close_window(&self, window_id: WindowId) -> Result<(), String> {
        self.app
            .borrow_mut()
            .close_window(window_id)
            .map_err(|_| "Window not found".to_owned())
    }

    fn open_workspace(&self, paths: Vec<PathBuf>) -> Result<(), String> {
        if !paths.iter().all(|path| path.is_absolute()) {
            return Err("All paths must be absolute".to_owned());
        }

        let replica_id = uuid::Uuid::new_v4();
        let roots = paths
            .iter()
            .map(|path| {
                let git = self.provider(path);
                let database = self.database(path);
                fs::Tree::new(Rc::new(self.reactor.clone()), path, replica_id, git, database).unwrap()
            })
            .collect();
        self.app
            .borrow_mut()
            .open_local_workspace(replica_id, roots);
        Ok(())
    }

    fn tcp_listen(&self, port: u16) -> Result<(), String> {
        let local_addr = SocketAddr::new("0.0.0.0".parse().unwrap(), port);
        let listener = TcpListener::bind(&local_addr, &self.reactor)
            .map_err(|_| "Error binding address".to_owned())?;
        let app = self.app.clone();
        let reactor = self.reactor.clone();
        let handle_incoming = listener
            .incoming()
            .map_err(|_| eprintln!("Error accepting incoming connection"))
            .for_each(move |(socket, _)| {
                socket.set_nodelay(true).unwrap();
                let transport = codec::length_delimited::Framed::<_, Bytes>::new(socket);
                let (tx, rx) = transport.split();
                let connection = App::connect_to_client(app.clone(), rx.map(|frame| frame.into()));
                reactor.spawn(
                    tx.send_all(connection.map_err(|_| -> io::Error { unreachable!() }))
                        .then(|result| {
                            if let Err(error) = result {
                                eprintln!(
                                    "Error sending message to client on TCP socket: {}",
                                    error
                                );
                            }

                            Ok(())
                        }),
                );
                Ok(())
            });
        self.reactor.spawn(handle_incoming);
        Ok(())
    }

    fn connect_to_peer(&self, address: SocketAddr) -> Box<Future<Item = (), Error = String>> {
        let reactor = self.reactor.clone();
        let app = self.app.clone();
        Box::new(
            TcpStream::connect(&address, &self.reactor)
                .map_err(move |error| {
                    format!(
                        "Could not connect to address {}, {}",
                        address,
                        error.description(),
                    )
                })
                .and_then(move |socket| {
                    socket.set_nodelay(true).unwrap();
                    let transport = codec::length_delimited::Framed::<_, Bytes>::new(socket);
                    let (tx, rx) = transport.split();
                    let app = app.borrow();
                    app.connect_to_server(rx.map(|frame| frame.into()))
                        .map_err(|error| format!("RPC error: {}", error))
                        .and_then(move |connection| {
                            reactor.spawn(
                                tx.send_all(
                                    connection
                                        .map(|bytes| bytes.into())
                                        .map_err(|_| -> io::Error { unreachable!() }),
                                )
                                .then(|result| {
                                    if let Err(error) = result {
                                        eprintln!(
                                            "Error sending message to server on TCP socket: {}",
                                            error
                                        );
                                    }

                                    Ok(())
                                }),
                            );
                            Ok(())
                        })
                }),
        )
    }

    fn send_outgoing<O, I>(&self, outgoing: O, responses: I)
    where
        O: 'static + Sink<SinkItem = OutgoingMessage>,
        I: 'static + Stream<Item = OutgoingMessage, Error = ()>,
    {
        self.reactor.spawn(
            outgoing
                .send_all(responses.map_err(|_| unreachable!()))
                .then(|_| Ok(())),
        );
    }

    fn provider<P: Into<PathBuf>>(&self, path: P) -> Rc<git::GitProvider> {
        let path = path.into();
        let mut providers = self.providers.borrow_mut();
        let provider = if let Some(provider) = providers.get(&path) {
            provider.clone()
        } else {
            Rc::new(git::GitProvider::new(&path))
        };

        providers.insert(path, provider.clone());

        provider
    }

    fn database<P: Into<PathBuf>>(&self, path: P) -> Rc<database::Database> {
        let path = path.into();
        let mut databases = self.databases.borrow_mut();
        let database = if let Some(database) = databases.get(&path) {
            database.clone()
        } else {
            Rc::new(database::Database::new(path.clone()))
        };

        databases.insert(path, database.clone());

        database
    }
}

fn report_input_errors<S>(incoming: S) -> Box<Stream<Item = OutgoingMessage, Error = ()>>
where
    S: 'static + Stream<Item = OutgoingMessage, Error = io::Error>,
{
    Box::new(
        incoming
            .then(|value| match value {
                Err(error) => Ok(OutgoingMessage::Error {
                    description: format!("Error reading message on server: {}", error),
                }),
                _ => value,
            })
            .map_err(|_| ()),
    )
}
