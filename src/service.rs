use futures::{prelude::*, sync::mpsc};
use log::{debug, error, trace, warn};
use multiaddr::{Multiaddr, ToMultiaddr};
use secio::{handshake::Config, PublicKey, SecioKeyPair};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::{
    error::{self, Error as ErrorTrait},
    fmt, io,
    time::Duration,
};
use tokio::net::{
    tcp::{ConnectFuture, Incoming},
    TcpListener, TcpStream,
};
use tokio::{
    codec::{Decoder, Encoder},
    prelude::{AsyncRead, AsyncWrite, FutureExt},
    timer::Timeout,
};
use yamux::session::SessionType;

use crate::{
    context::{ServiceContext, ServiceControl, SessionContext},
    error::Error,
    protocol_select::ProtocolInfo,
    session::{Session, SessionEvent, SessionMeta},
    traits::{ProtocolMeta, ServiceHandle, ServiceProtocol, SessionProtocol},
    utils::multiaddr_to_socketaddr,
    ProtocolId, SessionId,
};

/// Protocol handle value
pub(crate) enum ProtocolHandle {
    /// Service level protocol
    Service(Box<dyn ServiceProtocol + Send + 'static>),
    /// Session level protocol
    Session(Box<dyn SessionProtocol + Send + 'static>),
}

/// Error generated by the Service
#[derive(Debug)]
pub enum ServiceError {
    /// When dial remote error
    DialerError {
        /// Remote address
        address: Multiaddr,
        /// error
        error: Error<ServiceTask>,
    },
    /// When listen error
    ListenError {
        /// Listen address
        address: Multiaddr,
        /// error
        error: Error<ServiceTask>,
    },
}

/// Event generated by the Service
#[derive(Debug)]
pub enum ServiceEvent {
    /// A session close
    SessionClose {
        /// Session id
        id: SessionId,
    },
    /// A session open
    SessionOpen {
        /// Session id
        id: SessionId,
        /// Remote address
        address: Multiaddr,
        /// Outbound or Inbound
        ty: SessionType,
        /// Remote public key
        public_key: Option<PublicKey>,
    },
}

/// Task received by the Service.
///
/// An instruction that the outside world can send to the service
pub enum ServiceTask {
    /// Send protocol data task
    ProtocolMessage {
        /// Specify which sessions to send to,
        /// None means broadcast
        session_ids: Option<Vec<SessionId>>,
        /// protocol id
        proto_id: ProtocolId,
        /// data
        data: Vec<u8>,
    },
    /// Service-level notify task
    ProtocolNotify {
        /// Protocol id
        proto_id: ProtocolId,
        /// Notify token
        token: u64,
    },
    /// Session-level notify task
    ProtocolSessionNotify {
        /// Session id
        session_id: SessionId,
        /// Protocol id
        proto_id: ProtocolId,
        /// Notify token
        token: u64,
    },
    /// Future task
    FutureTask {
        /// Future
        task: Box<dyn Future<Item = (), Error = ()> + 'static + Send>,
    },
    /// Disconnect task
    Disconnect {
        /// Session id
        session_id: SessionId,
    },
    /// Dial task
    Dial {
        /// Remote address
        address: Multiaddr,
    },
}

impl fmt::Debug for ServiceTask {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::ServiceTask::*;

        match self {
            ProtocolMessage {
                session_ids,
                proto_id,
                data,
            } => write!(
                f,
                "id: {:?}, protoid: {}, message: {:?}",
                session_ids, proto_id, data
            ),
            ProtocolNotify { proto_id, token } => {
                write!(f, "protocol id: {}, token: {}", proto_id, token)
            }
            ProtocolSessionNotify {
                session_id,
                proto_id,
                token,
            } => write!(
                f,
                "session id: {}, protocol id: {}, token: {}",
                session_id, proto_id, token
            ),
            FutureTask { .. } => write!(f, "Future task"),
            Disconnect { session_id } => write!(f, "Disconnect session [{}]", session_id),
            Dial { address } => write!(f, "Dial address: {}", address),
        }
    }
}

/// An abstraction of p2p service, currently only supports TCP protocol
pub struct Service<T, U> {
    protocol_configs: Arc<HashMap<String, Box<dyn ProtocolMeta<U> + Send + Sync>>>,

    sessions: HashMap<SessionId, SessionContext>,

    listens: Vec<(Multiaddr, Incoming)>,

    dial: Vec<(Multiaddr, Timeout<ConnectFuture>)>,
    timeout: Duration,
    /// Calculate the number of connection requests that need to be sent externally,
    /// if run forever, it will default to 1, else it default to 0
    task_count: usize,

    next_session: SessionId,

    key_pair: Option<SecioKeyPair>,

    /// Can be upgrade to list service level protocols
    handle: T,

    // The service protocols open with the session
    session_service_protos: HashMap<SessionId, HashSet<ProtocolId>>,

    service_proto_handles: HashMap<ProtocolId, Box<dyn ServiceProtocol + Send + 'static>>,

    session_proto_handles:
        HashMap<SessionId, HashMap<ProtocolId, Box<dyn SessionProtocol + Send + 'static>>>,

    /// Send events to service, clone to session
    session_event_sender: mpsc::Sender<SessionEvent>,
    /// Receive event from service
    session_event_receiver: mpsc::Receiver<SessionEvent>,

    /// External event is passed in from this
    service_context: ServiceContext,
    /// External event receiver
    service_task_receiver: mpsc::Receiver<ServiceTask>,
}

impl<T, U> Service<T, U>
where
    T: ServiceHandle,
    U: Decoder<Item = bytes::BytesMut> + Encoder<Item = bytes::Bytes> + Send + 'static,
    <U as Decoder>::Error: error::Error + Into<io::Error>,
    <U as Encoder>::Error: error::Error + Into<io::Error>,
{
    /// New a Service
    pub fn new(
        protocol_configs: Arc<HashMap<String, Box<dyn ProtocolMeta<U> + Send + Sync>>>,
        handle: T,
        key_pair: Option<SecioKeyPair>,
        forever: bool,
        timeout: Duration,
    ) -> Self {
        let (session_event_sender, session_event_receiver) = mpsc::channel(256);
        let (service_task_sender, service_task_receiver) = mpsc::channel(256);
        let proto_infos = protocol_configs
            .values()
            .map(|meta| {
                let proto_info = ProtocolInfo::new(&meta.name(), meta.support_versions());
                (meta.id(), proto_info)
            })
            .collect();

        Service {
            protocol_configs,
            handle,
            key_pair,
            sessions: HashMap::default(),
            session_service_protos: HashMap::default(),
            service_proto_handles: HashMap::default(),
            session_proto_handles: HashMap::default(),
            listens: Vec::new(),
            dial: Vec::new(),
            timeout,
            task_count: if forever { 1 } else { 0 },
            next_session: 0,
            session_event_sender,
            session_event_receiver,
            service_context: ServiceContext::new(service_task_sender, proto_infos),
            service_task_receiver,
        }
    }

    /// Listen on the given address.
    pub fn listen(&mut self, address: &Multiaddr) -> Result<Multiaddr, io::Error> {
        let socket_address =
            multiaddr_to_socketaddr(&address).map_err(|_| io::ErrorKind::InvalidInput)?;
        let tcp = TcpListener::bind(&socket_address)?;
        let listen_addr = tcp.local_addr()?.to_multiaddr().unwrap();
        self.listens.push((listen_addr.clone(), tcp.incoming()));
        Ok(listen_addr)
    }

    /// Dial the given address, doesn't actually make a request, just generate a future
    pub fn dial(mut self, address: Multiaddr) -> Self {
        self.dial_inner(address);
        self
    }

    /// Use by inner
    #[inline(always)]
    fn dial_inner(&mut self, address: Multiaddr) {
        let socket_address = multiaddr_to_socketaddr(&address).expect("Address input error");
        let dial = TcpStream::connect(&socket_address).timeout(self.timeout);
        self.dial.push((address, dial));
        self.task_count += 1;
    }

    /// Get service current protocol configure
    pub fn protocol_configs(
        &self,
    ) -> &Arc<HashMap<String, Box<dyn ProtocolMeta<U> + Send + Sync>>> {
        &self.protocol_configs
    }

    /// Get service control, control can send tasks externally to the runtime inside
    pub fn control(&mut self) -> &mut ServiceControl {
        self.service_context.control()
    }

    /// Send data to the specified protocol for the specified session.
    ///
    /// Valid after Service starts
    #[inline]
    pub fn send_message(&mut self, session_id: SessionId, proto_id: ProtocolId, data: &[u8]) {
        if let Some(session) = self.sessions.get_mut(&session_id) {
            let _ = session
                .event_sender
                .try_send(SessionEvent::ProtocolMessage {
                    id: session_id,
                    proto_id,
                    data: data.into(),
                });
        }
    }

    /// Send data to the specified protocol for the specified sessions.
    ///
    /// Valid after Service starts
    #[inline]
    pub fn filter_broadcast(
        &mut self,
        ids: Option<Vec<SessionId>>,
        proto_id: ProtocolId,
        data: &[u8],
    ) {
        match ids {
            None => self.broadcast(proto_id, data),
            Some(ids) => {
                let data: bytes::Bytes = data.into();
                self.sessions.iter_mut().for_each(|(id, session)| {
                    if ids.contains(id) {
                        let _ = session
                            .event_sender
                            .try_send(SessionEvent::ProtocolMessage {
                                id: *id,
                                proto_id,
                                data: data.clone(),
                            });
                    }
                });
            }
        }
    }

    /// Broadcast data for a specified protocol.
    ///
    /// Valid after Service starts
    #[inline]
    pub fn broadcast(&mut self, proto_id: ProtocolId, data: &[u8]) {
        debug!(
            "broadcast message, peer count: {}, proto_id: {}",
            self.sessions.len(),
            proto_id
        );
        let data: bytes::Bytes = data.into();
        self.sessions.iter_mut().for_each(|(id, session)| {
            let _ = session
                .event_sender
                .try_send(SessionEvent::ProtocolMessage {
                    id: *id,
                    proto_id,
                    data: data.clone(),
                });
        });
    }

    /// Get the callback handle of the specified protocol
    #[inline]
    fn proto_handle(&self, session: bool, proto_id: ProtocolId) -> Option<ProtocolHandle> {
        let handle = self
            .protocol_configs
            .values()
            .filter(|proto| proto.id() == proto_id)
            .map(|proto| {
                if session {
                    proto.session_handle().map(ProtocolHandle::Session)
                } else {
                    proto.service_handle().map(ProtocolHandle::Service)
                }
            })
            .find(Option::is_some)
            .unwrap_or(None);

        if handle.is_none() {
            debug!(
                "can't find proto [{}] {} handle",
                proto_id,
                if session { "session" } else { "service" }
            );
        }

        handle
    }

    /// Handshake
    #[inline]
    fn handshake(&mut self, socket: TcpStream, ty: SessionType) {
        let address: Multiaddr = socket.peer_addr().unwrap().to_multiaddr().unwrap();
        if let Some(ref key_pair) = self.key_pair {
            let key_pair = key_pair.clone();
            let mut sender = self.session_event_sender.clone();

            let task = Config::new(key_pair)
                .handshake(socket)
                .timeout(self.timeout)
                .then(move |result| {
                    match result {
                        Ok((handle, public_key, _)) => {
                            let _ = sender.try_send(SessionEvent::HandshakeSuccess {
                                handle,
                                public_key,
                                address,
                                ty,
                            });
                        }
                        Err(err) => {
                            let error = if err.is_timer() {
                                // tokio timer error
                                io::Error::new(io::ErrorKind::Other, err.description()).into()
                            } else if err.is_elapsed() {
                                // time out error
                                io::Error::new(io::ErrorKind::TimedOut, err.description()).into()
                            } else {
                                // dialer error
                                err.into_inner().unwrap().into()
                            };

                            error!("Handshake with {} failed, error: {:?}", address, error);
                            let _ =
                                sender.try_send(SessionEvent::HandshakeFail { ty, error, address });
                        }
                    }

                    Ok(())
                });

            tokio::spawn(task);
        } else {
            self.session_open(socket, None, address, ty);
            if ty == SessionType::Client {
                self.task_count -= 1;
            }
        }
    }

    /// Session open
    #[inline]
    fn session_open<H>(
        &mut self,
        mut handle: H,
        remote_pubkey: Option<PublicKey>,
        address: Multiaddr,
        ty: SessionType,
    ) where
        H: AsyncRead + AsyncWrite + Send + 'static,
    {
        if let Some(ref key) = remote_pubkey {
            // If the public key exists, the connection has been established
            // and then the useless connection needs to be closed.
            match self
                .sessions
                .values()
                .find(|context| context.remote_pubkey.as_ref() == Some(key))
            {
                Some(context) => {
                    trace!("Connected to the connected node");
                    // TODO: The behavior of receiving error here is undefined. It may be that the server is received or may be received by the client,
                    // TODO: depending on who both parties handle it here or both received.
                    let _ = handle.shutdown();
                    if ty == SessionType::Client {
                        self.handle.handle_error(
                            &mut self.service_context,
                            ServiceError::DialerError {
                                error: Error::RepeatedConnection(context.id),
                                address,
                            },
                        );
                    } else {
                        self.handle.handle_error(
                            &mut self.service_context,
                            ServiceError::ListenError {
                                error: Error::RepeatedConnection(context.id),
                                address,
                            },
                        );
                    }
                    return;
                }
                None => self.next_session += 1,
            }
        } else {
            self.next_session += 1;
        }

        let (service_event_sender, service_event_receiver) = mpsc::channel(256);
        let session = SessionContext {
            event_sender: service_event_sender,
            id: self.next_session,
            address: address.clone(),
            ty,
            remote_pubkey: remote_pubkey.clone(),
        };
        self.sessions.insert(session.id, session);

        let meta = SessionMeta::new(self.next_session, ty, self.timeout)
            .protocol(self.protocol_configs.clone());

        let mut session = Session::new(
            handle,
            self.session_event_sender.clone(),
            service_event_receiver,
            meta,
        );

        if ty == SessionType::Client {
            self.protocol_configs
                .keys()
                .for_each(|name| session.open_proto_stream(name));
        }

        tokio::spawn(session.for_each(|_| Ok(())).map_err(|_| ()));

        self.handle.handle_event(
            &mut self.service_context,
            ServiceEvent::SessionOpen {
                id: self.next_session,
                address,
                ty,
                public_key: remote_pubkey,
            },
        );
    }

    /// Close the specified session, clean up the handle
    #[inline]
    fn session_close(&mut self, id: SessionId) {
        debug!("service session [{}] close", id);
        if let Some(session) = self.sessions.get_mut(&id) {
            let _ = session
                .event_sender
                .try_send(SessionEvent::SessionClose { id });
        }

        // Service handle processing flow
        self.handle
            .handle_event(&mut self.service_context, ServiceEvent::SessionClose { id });

        // Session proto handle processing flow
        if let Some(mut handles) = self.session_proto_handles.remove(&id) {
            for handle in handles.values_mut() {
                handle.disconnected(&mut self.service_context);
            }
        }

        let close_proto_ids = self.session_service_protos.remove(&id).unwrap_or_default();
        debug!("session [{}] close proto [{:?}]", id, close_proto_ids);
        // Service proto handle processing flow
        //
        // You must first confirm that the protocol is open in the session,
        // otherwise a false positive will occur.
        close_proto_ids.into_iter().for_each(|proto_id| {
            self.service_context
                .remove_session_notify_senders(id, proto_id);
            let session_context = self.sessions.get(&id);
            let service_handle = self.service_proto_handles.get_mut(&proto_id);
            if let (Some(handle), Some(session_context)) = (service_handle, session_context) {
                handle.disconnected(&mut self.service_context, session_context);
            }
        });
        self.sessions.remove(&id);
    }

    /// Open the handle corresponding to the protocol
    #[inline]
    fn protocol_open(&mut self, id: SessionId, proto_id: ProtocolId, version: &str) {
        debug!("service session [{}] proto [{}] open", id, proto_id);
        let session_context = self
            .sessions
            .get(&id)
            .expect("Protocol open without session open");

        // Service proto handle processing flow
        if !self.service_proto_handles.contains_key(&proto_id) {
            if let Some(ProtocolHandle::Service(mut handle)) = self.proto_handle(false, proto_id) {
                debug!("init service [{}] level proto [{}] handle", id, proto_id);
                handle.init(&mut self.service_context);
                self.service_proto_handles.insert(proto_id, handle);
            }
        }
        if let Some(handle) = self.service_proto_handles.get_mut(&proto_id) {
            handle.connected(&mut self.service_context, &session_context, version);
            self.session_service_protos
                .entry(id)
                .or_default()
                .insert(proto_id);
        }

        // Session proto handle processing flow
        // Regardless of the existence of the session level handle,
        // you **must record** which protocols are opened for each session.
        if let Some(ProtocolHandle::Session(mut handle)) = self.proto_handle(true, proto_id) {
            debug!("init session [{}] level proto [{}] handle", id, proto_id);
            handle.connected(&mut self.service_context, &session_context, version);
            self.session_proto_handles
                .entry(id)
                .or_default()
                .insert(proto_id, handle);
        }
    }

    /// Processing the received data
    #[inline]
    fn protocol_message(
        &mut self,
        session_id: SessionId,
        proto_id: ProtocolId,
        data: &bytes::Bytes,
    ) {
        debug!(
            "service receive session [{}] proto [{}] data: {:?}",
            session_id, proto_id, data
        );

        // Service proto handle processing flow
        let service_handle = self.service_proto_handles.get_mut(&proto_id);
        let session_context = self.sessions.get_mut(&session_id);
        if let (Some(handle), Some(session_context)) = (service_handle, session_context) {
            handle.received(&mut self.service_context, &session_context, data.to_vec());
        }

        // Session proto handle processing flow
        if let Some(handles) = self.session_proto_handles.get_mut(&session_id) {
            if let Some(handle) = handles.get_mut(&proto_id) {
                handle.received(&mut self.service_context, data.to_vec());
            }
        }
    }

    /// Protocol stream is closed, clean up data
    #[inline]
    fn protocol_close(&mut self, session_id: SessionId, proto_id: ProtocolId) {
        debug!(
            "service session [{}] proto [{}] close",
            session_id, proto_id
        );

        // Service proto handle processing flow
        let service_handle = self.service_proto_handles.get_mut(&proto_id);
        let session_context = self.sessions.get_mut(&session_id);
        if let (Some(handle), Some(session_context)) = (service_handle, session_context) {
            handle.disconnected(&mut self.service_context, &session_context);
        }

        // Session proto handle processing flow
        if let Some(handles) = self.session_proto_handles.get_mut(&session_id) {
            if let Some(mut handle) = handles.remove(&proto_id) {
                handle.disconnected(&mut self.service_context);
            }
        }

        // Session proto info remove
        if let Some(infos) = self.session_service_protos.get_mut(&session_id) {
            infos.remove(&proto_id);
        }

        // Close notify sender
        self.service_context
            .remove_session_notify_senders(session_id, proto_id);
    }

    /// Handling various events uploaded by the session
    fn handle_session_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::SessionClose { id } => self.session_close(id),
            SessionEvent::HandshakeSuccess {
                handle,
                public_key,
                address,
                ty,
            } => {
                self.session_open(handle, Some(public_key), address, ty);
                if ty == SessionType::Client {
                    self.task_count -= 1;
                }
            }
            SessionEvent::HandshakeFail { ty, error, address } => {
                if ty == SessionType::Client {
                    self.task_count -= 1;
                    self.handle.handle_error(
                        &mut self.service_context,
                        ServiceError::DialerError { error, address },
                    )
                }
            }
            SessionEvent::ProtocolMessage { id, proto_id, data } => {
                self.protocol_message(id, proto_id, &data)
            }
            SessionEvent::ProtocolOpen {
                id,
                proto_id,
                version,
                ..
            } => self.protocol_open(id, proto_id, &version),
            SessionEvent::ProtocolClose { id, proto_id, .. } => self.protocol_close(id, proto_id),
        }
    }

    /// Handling various tasks sent externally
    fn handle_service_task(&mut self, event: ServiceTask) {
        match event {
            ServiceTask::ProtocolMessage {
                session_ids,
                proto_id,
                data,
            } => self.filter_broadcast(session_ids, proto_id, &data),
            ServiceTask::Dial { address } => {
                if !self.dial.iter().any(|(addr, _)| addr == &address) {
                    self.dial_inner(address);
                }
                if !self.dial.is_empty() {
                    self.client_poll();
                }
            }
            ServiceTask::Disconnect { session_id } => self.session_close(session_id),
            ServiceTask::FutureTask { task } => {
                tokio::spawn(task);
            }
            ServiceTask::ProtocolNotify { proto_id, token } => {
                if let Some(handle) = self.service_proto_handles.get_mut(&proto_id) {
                    handle.notify(&mut self.service_context, token);
                }
            }
            ServiceTask::ProtocolSessionNotify {
                session_id,
                proto_id,
                token,
            } => {
                if let Some(handles) = self.session_proto_handles.get_mut(&session_id) {
                    if let Some(handle) = handles.get_mut(&proto_id) {
                        handle.notify(&mut self.service_context, token);
                    } else {
                        self.service_context
                            .remove_session_notify_senders(session_id, proto_id);
                    }
                }
            }
        }
    }

    /// Poll client requests
    #[inline]
    fn client_poll(&mut self) {
        for (address, mut dialer) in self.dial.split_off(0) {
            match dialer.poll() {
                Ok(Async::Ready(socket)) => {
                    self.handshake(socket, SessionType::Client);
                }
                Ok(Async::NotReady) => {
                    trace!("client not ready, {}", address);
                    self.dial.push((address, dialer));
                }
                Err(err) => {
                    self.task_count -= 1;
                    let error = if err.is_timer() {
                        // tokio timer error
                        io::Error::new(io::ErrorKind::Other, err.description())
                    } else if err.is_elapsed() {
                        // time out error
                        io::Error::new(io::ErrorKind::TimedOut, err.description())
                    } else {
                        // dialer error
                        err.into_inner().unwrap()
                    };
                    self.handle.handle_error(
                        &mut self.service_context,
                        ServiceError::DialerError {
                            address,
                            error: error.into(),
                        },
                    );
                }
            }
        }
    }

    /// Poll listen connections
    #[inline]
    fn listen_poll(&mut self) {
        let mut update = false;
        for (address, mut listen) in self.listens.split_off(0) {
            match listen.poll() {
                Ok(Async::Ready(Some(socket))) => {
                    self.handshake(socket, SessionType::Server);
                    self.listens.push((address, listen));
                }
                Ok(Async::Ready(None)) => (),
                Ok(Async::NotReady) => {
                    self.listens.push((address, listen));
                }
                Err(err) => {
                    update = true;
                    self.handle.handle_error(
                        &mut self.service_context,
                        ServiceError::ListenError {
                            address,
                            error: err.into(),
                        },
                    );
                }
            }
        }

        if update || self.service_context.listens().is_empty() {
            self.service_context.update_listens(
                self.listens
                    .iter()
                    .map(|(address, _)| address.clone())
                    .collect(),
            );
        }
    }
}

impl<T, U> Stream for Service<T, U>
where
    T: ServiceHandle,
    U: Decoder<Item = bytes::BytesMut> + Encoder<Item = bytes::Bytes> + Send + 'static,
    <U as Decoder>::Error: error::Error + Into<io::Error>,
    <U as Encoder>::Error: error::Error + Into<io::Error>,
{
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if self.listens.is_empty() && self.task_count == 0 && self.sessions.is_empty() {
            return Ok(Async::Ready(None));
        }

        self.client_poll();

        self.listen_poll();

        loop {
            match self.session_event_receiver.poll() {
                Ok(Async::Ready(Some(event))) => self.handle_session_event(event),
                Ok(Async::Ready(None)) => unreachable!(),
                Ok(Async::NotReady) => break,
                Err(err) => {
                    warn!("receive session error: {:?}", err);
                    break;
                }
            }
        }

        loop {
            match self.service_task_receiver.poll() {
                Ok(Async::Ready(Some(task))) => self.handle_service_task(task),
                Ok(Async::Ready(None)) => unreachable!(),
                Ok(Async::NotReady) => break,
                Err(err) => {
                    warn!("receive service task error: {:?}", err);
                    break;
                }
            }
        }

        // Double check service state
        if self.listens.is_empty() && self.task_count == 0 && self.sessions.is_empty() {
            return Ok(Async::Ready(None));
        }
        debug!(
            "listens count: {}, task_count: {}, sessions count: {}",
            self.listens.len(),
            self.task_count,
            self.sessions.len()
        );

        Ok(Async::NotReady)
    }
}
