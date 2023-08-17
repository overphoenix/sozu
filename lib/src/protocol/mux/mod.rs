use std::{
    cell::RefCell,
    collections::HashMap,
    net::SocketAddr,
    rc::{Rc, Weak},
    str::from_utf8_unchecked,
};

use mio::{net::TcpStream, Token};
use rusty_ulid::Ulid;
use sozu_command::ready::Ready;

mod parser;
mod serializer;

use crate::{
    https::HttpsListener,
    pool::{Checkout, Pool},
    protocol::SessionState,
    socket::{FrontRustls, SocketHandler, SocketResult},
    AcceptError, L7Proxy, ProxySession, Readiness, SessionMetrics, SessionResult, StateResult,
};

/// Generic Http representation using the Kawa crate using the Checkout of Sozu as buffer
type GenericHttpStream = kawa::Kawa<Checkout>;
type StreamId = u32;
type GlobalStreamId = usize;

#[derive(Debug, Clone, Copy)]
pub enum Position {
    Client,
    Server,
}

pub struct ConnectionH1<Front: SocketHandler> {
    pub position: Position,
    pub readiness: Readiness,
    pub socket: Front,
    pub stream: GlobalStreamId,
}

#[derive(Debug)]
pub enum H2State {
    ClientPreface,
    ClientSettings,
    ServerSettings,
    Header,
    Frame(parser::FrameHeader),
    Error,
}

#[derive(Debug)]
pub struct H2Settings {
    settings_header_table_size: u32,
    settings_enable_push: bool,
    settings_max_concurrent_streams: u32,
    settings_initial_window_size: u32,
    settings_max_frame_size: u32,
    settings_max_header_list_size: u32,
}

impl Default for H2Settings {
    fn default() -> Self {
        Self {
            settings_header_table_size: 4096,
            settings_enable_push: true,
            settings_max_concurrent_streams: u32::MAX,
            settings_initial_window_size: (1 << 16) - 1,
            settings_max_frame_size: 1 << 14,
            settings_max_header_list_size: u32::MAX,
        }
    }
}

pub struct ConnectionH2<Front: SocketHandler> {
    pub decoder: hpack::Decoder<'static>,
    pub expect: Option<(GlobalStreamId, usize)>,
    pub position: Position,
    pub readiness: Readiness,
    pub settings: H2Settings,
    pub socket: Front,
    pub state: H2State,
    pub streams: HashMap<StreamId, GlobalStreamId>,
}

pub struct Stream {
    pub request_id: Ulid,
    pub window: i32,
    pub front: GenericHttpStream,
    pub back: GenericHttpStream,
}

impl Stream {
    pub fn front(&mut self, position: Position) -> &mut GenericHttpStream {
        match position {
            Position::Client => &mut self.back,
            Position::Server => &mut self.front,
        }
    }
    pub fn back(&mut self, position: Position) -> &mut GenericHttpStream {
        match position {
            Position::Client => &mut self.front,
            Position::Server => &mut self.back,
        }
    }
}

pub enum Connection<Front: SocketHandler> {
    H1(ConnectionH1<Front>),
    H2(ConnectionH2<Front>),
}

impl<Front: SocketHandler> Connection<Front> {
    pub fn new_h1_server(front_stream: Front) -> Connection<Front> {
        Connection::H1(ConnectionH1 {
            socket: front_stream,
            position: Position::Server,
            readiness: Readiness {
                interest: Ready::READABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            stream: 0,
        })
    }
    pub fn new_h1_client(front_stream: Front) -> Connection<Front> {
        Connection::H1(ConnectionH1 {
            socket: front_stream,
            position: Position::Client,
            readiness: Readiness {
                interest: Ready::WRITABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            stream: 0,
        })
    }

    pub fn new_h2_server(front_stream: Front) -> Connection<Front> {
        Connection::H2(ConnectionH2 {
            socket: front_stream,
            position: Position::Server,
            readiness: Readiness {
                interest: Ready::READABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            streams: HashMap::from([(0, 0)]),
            state: H2State::ClientPreface,
            expect: Some((0, 24 + 9)),
            settings: H2Settings::default(),
            decoder: hpack::Decoder::new(),
        })
    }
    pub fn new_h2_client(front_stream: Front) -> Connection<Front> {
        Connection::H2(ConnectionH2 {
            socket: front_stream,
            position: Position::Client,
            readiness: Readiness {
                interest: Ready::WRITABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            streams: HashMap::from([(0, 0)]),
            state: H2State::ClientPreface,
            expect: None,
            settings: H2Settings::default(),
            decoder: hpack::Decoder::new(),
        })
    }

    pub fn readiness(&self) -> &Readiness {
        match self {
            Connection::H1(c) => &c.readiness,
            Connection::H2(c) => &c.readiness,
        }
    }
    pub fn readiness_mut(&mut self) -> &mut Readiness {
        match self {
            Connection::H1(c) => &mut c.readiness,
            Connection::H2(c) => &mut c.readiness,
        }
    }
    fn readable(&mut self, streams: &mut Streams) {
        match self {
            Connection::H1(c) => c.readable(streams),
            Connection::H2(c) => c.readable(streams),
        }
    }
    fn writable(&mut self, streams: &mut Streams) {
        match self {
            Connection::H1(c) => c.writable(streams),
            Connection::H2(c) => c.writable(streams),
        }
    }
}

pub struct Streams {
    pub streams: Vec<Stream>,
    pub pool: Weak<RefCell<Pool>>,
}

pub struct Mux {
    pub frontend_token: Token,
    pub frontend: Connection<FrontRustls>,
    pub backends: HashMap<Token, Connection<TcpStream>>,
    pub listener: Rc<RefCell<HttpsListener>>,
    pub public_address: SocketAddr,
    pub peer_address: Option<SocketAddr>,
    pub sticky_name: String,
    pub streams: Streams,
}

impl Streams {
    pub fn create_stream(
        &mut self,
        request_id: Ulid,
        window: u32,
    ) -> Result<GlobalStreamId, AcceptError> {
        let (front_buffer, back_buffer) = match self.pool.upgrade() {
            Some(pool) => {
                let mut pool = pool.borrow_mut();
                match (pool.checkout(), pool.checkout()) {
                    (Some(front_buffer), Some(back_buffer)) => (front_buffer, back_buffer),
                    _ => return Err(AcceptError::BufferCapacityReached),
                }
            }
            None => return Err(AcceptError::BufferCapacityReached),
        };
        self.streams.push(Stream {
            request_id,
            window: window as i32,
            front: GenericHttpStream::new(kawa::Kind::Request, kawa::Buffer::new(front_buffer)),
            back: GenericHttpStream::new(kawa::Kind::Response, kawa::Buffer::new(back_buffer)),
        });
        Ok(self.streams.len() - 1)
    }
}

impl std::ops::Deref for Streams {
    type Target = [Stream];
    fn deref(&self) -> &Self::Target {
        &self.streams
    }
}
impl std::ops::DerefMut for Streams {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.streams
    }
}

impl Mux {
    pub fn front_socket(&self) -> &TcpStream {
        match &self.frontend {
            Connection::H1(c) => &c.socket.stream,
            Connection::H2(c) => &c.socket.stream,
        }
    }
}

impl SessionState for Mux {
    fn ready(
        &mut self,
        session: Rc<RefCell<dyn ProxySession>>,
        proxy: Rc<RefCell<dyn L7Proxy>>,
        metrics: &mut SessionMetrics,
    ) -> SessionResult {
        let mut counter = 0;
        let max_loop_iterations = 100000;

        if self.frontend.readiness().event.is_hup() {
            return SessionResult::Close;
        }

        let streams = &mut self.streams;
        while counter < max_loop_iterations {
            let mut dirty = false;

            if self.frontend.readiness().filter_interest().is_readable() {
                self.frontend.readable(streams);
                dirty = true;
            }

            for (_, backend) in self.backends.iter_mut() {
                if backend.readiness().filter_interest().is_writable() {
                    backend.writable(streams);
                    dirty = true;
                }

                if backend.readiness().filter_interest().is_readable() {
                    backend.readable(streams);
                    dirty = true;
                }
            }

            if self.frontend.readiness().filter_interest().is_writable() {
                self.frontend.writable(streams);
                dirty = true;
            }

            for backend in self.backends.values() {
                if backend.readiness().filter_interest().is_hup()
                    || backend.readiness().filter_interest().is_error()
                {
                    return SessionResult::Close;
                }
            }

            if !dirty {
                break;
            }

            counter += 1;
        }

        if counter == max_loop_iterations {
            incr!("http.infinite_loop.error");
            return SessionResult::Close;
        }

        SessionResult::Continue
    }

    fn update_readiness(&mut self, token: Token, events: sozu_command::ready::Ready) {
        if token == self.frontend_token {
            self.frontend.readiness_mut().event |= events;
        } else if let Some(c) = self.backends.get_mut(&token) {
            c.readiness_mut().event |= events;
        }
    }

    fn timeout(&mut self, token: Token, metrics: &mut SessionMetrics) -> StateResult {
        println!("MuxState::timeout({token:?})");
        StateResult::CloseSession
    }

    fn cancel_timeouts(&mut self) {
        println!("MuxState::cancel_timeouts");
    }

    fn print_state(&self, context: &str) {
        error!(
            "\
{} Session(Mux)
\tFrontend:
\t\ttoken: {:?}\treadiness: {:?}",
            context,
            self.frontend_token,
            self.frontend.readiness()
        );
    }
    fn close(&mut self, _proxy: Rc<RefCell<dyn L7Proxy>>, _metrics: &mut SessionMetrics) {
        let s = match &mut self.frontend {
            Connection::H1(c) => &mut c.socket,
            Connection::H2(c) => &mut c.socket,
        };
        let mut b = [0; 1024];
        let (size, status) = s.socket_read(&mut b);
        println!("{size} {status:?} {:?}", &b[..size]);
    }
}

impl<Front: SocketHandler> ConnectionH2<Front> {
    fn readable(&mut self, streams: &mut Streams) {
        println!("======= MUX H2 READABLE");
        let kawa = if let Some((stream_id, amount)) = self.expect {
            let kawa = streams[stream_id].front(self.position);
            let (size, status) = self.socket.socket_read(&mut kawa.storage.space()[..amount]);
            println!("{:?}({stream_id}, {amount}) {size} {status:?}", self.state);
            if size > 0 {
                kawa.storage.fill(size);
                if size == amount {
                    self.expect = None;
                } else {
                    self.expect = Some((stream_id, amount - size));
                    return;
                }
            } else {
                self.readiness.event.remove(Ready::READABLE);
                return;
            }
            kawa
        } else {
            self.readiness.event.remove(Ready::READABLE);
            return;
        };
        match (&self.state, &self.position) {
            (H2State::ClientPreface, Position::Client) => {
                error!("Waiting for ClientPreface to finish writing")
            }
            (H2State::ClientPreface, Position::Server) => {
                let i = kawa.storage.data();
                let i = match parser::preface(i) {
                    Ok((i, _)) => i,
                    Err(e) => panic!("{e:?}"),
                };
                match parser::frame_header(i) {
                    Ok((
                        _,
                        parser::FrameHeader {
                            payload_len,
                            frame_type: parser::FrameType::Settings,
                            flags: 0,
                            stream_id: 0,
                        },
                    )) => {
                        kawa.storage.clear();
                        self.state = H2State::ClientSettings;
                        self.expect = Some((0, payload_len as usize));
                    }
                    _ => todo!(),
                };
            }
            (H2State::ClientSettings, Position::Server) => {
                let i = kawa.storage.data();
                match parser::settings_frame(i, i.len()) {
                    Ok((_, settings)) => {
                        kawa.storage.clear();
                        self.handle(settings, streams);
                    }
                    Err(e) => panic!("{e:?}"),
                }
                let kawa = &mut streams[0].back;
                self.state = H2State::ServerSettings;
                match serializer::gen_frame_header(
                    kawa.storage.space(),
                    &parser::FrameHeader {
                        payload_len: 6 * 2,
                        frame_type: parser::FrameType::Settings,
                        flags: 0,
                        stream_id: 0,
                    },
                ) {
                    Ok((_, size)) => kawa.storage.fill(size),
                    Err(e) => panic!("could not serialize HeaderFrame: {e:?}"),
                };
                // kawa.storage
                //     .write(&[1, 3, 0, 0, 0, 100, 0, 4, 0, 1, 0, 0])
                //     .unwrap();
                match serializer::gen_frame_header(
                    kawa.storage.space(),
                    &parser::FrameHeader {
                        payload_len: 0,
                        frame_type: parser::FrameType::Settings,
                        flags: 1,
                        stream_id: 0,
                    },
                ) {
                    Ok((_, size)) => kawa.storage.fill(size),
                    Err(e) => panic!("could not serialize HeaderFrame: {e:?}"),
                };
                self.readiness.interest.insert(Ready::WRITABLE);
                self.readiness.interest.remove(Ready::READABLE);
            }
            (H2State::ServerSettings, Position::Client) => todo!("Receive server Settings"),
            (H2State::ServerSettings, Position::Server) => {
                error!("waiting for ServerPreface to finish writing")
            }
            (H2State::Header, Position::Server) => {
                let i = kawa.storage.data();
                println!("  header: {i:?}");
                match parser::frame_header(i) {
                    Ok((_, header)) => {
                        println!("{header:?}");
                        kawa.storage.clear();
                        let stream_id = if let Some(stream_id) = self.streams.get(&header.stream_id)
                        {
                            *stream_id
                        } else {
                            self.create_stream(header.stream_id, streams)
                        };
                        let stream_id = if header.frame_type == parser::FrameType::Headers {
                            0
                        } else {
                            stream_id
                        };
                        println!("{} {} {:#?}", header.stream_id, stream_id, self.streams);
                        self.expect = Some((stream_id as usize, header.payload_len as usize));
                        self.state = H2State::Frame(header);
                    }
                    Err(e) => panic!("{e:?}"),
                };
            }
            (H2State::Frame(header), Position::Server) => {
                let i = kawa.storage.data();
                println!("  data: {i:?}");
                match parser::frame_body(i, header, self.settings.settings_max_frame_size) {
                    Ok((_, frame)) => {
                        kawa.storage.clear();
                        self.handle(frame, streams);
                    }
                    Err(e) => panic!("{e:?}"),
                }
                self.state = H2State::Header;
                self.expect = Some((0, 9));
            }
            _ => unreachable!(),
        }
    }

    fn writable(&mut self, streams: &mut Streams) {
        println!("======= MUX H2 WRITABLE");
        match (&self.state, &self.position) {
            (H2State::ClientPreface, Position::Client) => todo!("Send PRI + client Settings"),
            (H2State::ClientPreface, Position::Server) => unreachable!(),
            (H2State::ServerSettings, Position::Client) => unreachable!(),
            (H2State::ServerSettings, Position::Server) => {
                let stream = &mut streams[0];
                let kawa = &mut stream.back;
                println!("{:?}", kawa.storage.data());
                let (size, status) = self.socket.socket_write(kawa.storage.data());
                println!("  size: {size}, status: {status:?}");
                let size = kawa.storage.available_data();
                kawa.storage.consume(size);
                if kawa.storage.is_empty() {
                    self.readiness.interest.remove(Ready::WRITABLE);
                    self.readiness.interest.insert(Ready::READABLE);
                    self.state = H2State::Header;
                    self.expect = Some((0, 9));
                }
            }
            _ => unreachable!(),
        }
    }

    pub fn create_stream(&mut self, stream_id: StreamId, streams: &mut Streams) -> GlobalStreamId {
        match streams.create_stream(Ulid::generate(), self.settings.settings_initial_window_size) {
            Ok(global_stream_id) => {
                self.streams.insert(stream_id, global_stream_id);
                global_stream_id
            }
            Err(e) => panic!("{e:?}"),
        }
    }

    fn handle(&mut self, frame: parser::Frame, streams: &mut Streams) {
        println!("{frame:?}");
        match frame {
            parser::Frame::Data(_) => todo!(),
            parser::Frame::Headers(headers) => {
                let kawa = streams[0].front(self.position);
                let buffer = headers.header_block_fragment.data(kawa.storage.buffer());
                println!("{buffer:?}");
                let result = self.decoder.decode(buffer).unwrap();
                for (k, v) in result {
                    unsafe { println!("{} {}", from_utf8_unchecked(&k), from_utf8_unchecked(&v)) };
                }
            }
            parser::Frame::Priority => todo!(),
            parser::Frame::RstStream(_) => todo!(),
            parser::Frame::Settings(settings) => {
                for setting in settings.settings {
                    match setting.identifier {
                        1 => self.settings.settings_header_table_size = setting.value,
                        2 => self.settings.settings_enable_push = setting.value == 1,
                        3 => self.settings.settings_max_concurrent_streams = setting.value,
                        4 => self.settings.settings_initial_window_size = setting.value,
                        5 => self.settings.settings_max_frame_size = setting.value,
                        6 => self.settings.settings_max_header_list_size = setting.value,
                        other => panic!("setting_id: {other}"),
                    }
                }
                println!("{:#?}", self.settings);
            }
            parser::Frame::PushPromise => todo!(),
            parser::Frame::Ping(_) => todo!(),
            parser::Frame::GoAway => todo!(),
            parser::Frame::WindowUpdate(update) => {
                streams[update.stream_id as usize].window += update.increment as i32;
            }
            parser::Frame::Continuation => todo!(),
        }
    }
}

impl<Front: SocketHandler> ConnectionH1<Front> {
    fn readable(&mut self, streams: &mut Streams) {
        println!("======= MUX H1 READABLE");
        let stream = &mut streams[self.stream];
        let kawa = match self.position {
            Position::Client => &mut stream.front,
            Position::Server => &mut stream.back,
        };
        let (size, status) = self.socket.socket_read(kawa.storage.space());
        println!("  size: {size}, status: {status:?}");
        if size > 0 {
            kawa.storage.fill(size);
        } else {
            self.readiness.event.remove(Ready::READABLE);
        }
        match status {
            SocketResult::Continue => {}
            SocketResult::Closed => todo!(),
            SocketResult::Error => todo!(),
            SocketResult::WouldBlock => self.readiness.event.remove(Ready::READABLE),
        }
        kawa::h1::parse(kawa, &mut kawa::h1::NoCallbacks);
        kawa::debug_kawa(kawa);
        if kawa.is_terminated() {
            self.readiness.interest.remove(Ready::READABLE);
        }
    }
    fn writable(&mut self, streams: &mut Streams) {
        println!("======= MUX H1 WRITABLE");
        let stream = &mut streams[self.stream];
        let kawa = match self.position {
            Position::Client => &mut stream.back,
            Position::Server => &mut stream.front,
        };
        kawa.prepare(&mut kawa::h1::BlockConverter);
        let bufs = kawa.as_io_slice();
        if bufs.is_empty() {
            self.readiness.interest.remove(Ready::WRITABLE);
            return;
        }
        let (size, status) = self.socket.socket_write_vectored(&bufs);
        println!("  size: {size}, status: {status:?}");
        if size > 0 {
            kawa.consume(size);
            // self.backend_readiness.interest.insert(Ready::READABLE);
        } else {
            self.readiness.event.remove(Ready::WRITABLE);
        }
    }
}