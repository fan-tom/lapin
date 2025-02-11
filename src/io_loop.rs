use amq_protocol::frame::{GenError, Offset, gen_frame, parse_frame};
use log::{error, trace};
use mio::{Evented, Events, Poll, PollOpt, Ready, Registration, SetReadiness, Token};
use parking_lot::Mutex;

use std::{
  io::{self, Read, Write},
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  thread::{self, Builder as ThreadBuilder, JoinHandle},
  time::{Duration, Instant},
};

use crate::{
  buffer::Buffer,
  connection::Connection,
  connection_status::ConnectionState,
  error::{Error, ErrorKind},
};

const SOCKET:   Token = Token(1);
const DATA:     Token = Token(2);
const CONTINUE: Token = Token(3);

const FRAMES_STORAGE: usize = 32;

#[derive(Clone, Debug)]
pub(crate) struct IoLoopHandle {
  handle: Arc<Mutex<Option<JoinHandle<Result<(), Error>>>>>,
}

impl Default for IoLoopHandle {
  fn default() -> Self {
    Self { handle: Arc::new(Mutex::new(None)) }
  }
}

impl IoLoopHandle {
  pub(crate) fn register(&self, handle: JoinHandle<Result<(), Error>>) {
    *self.handle.lock() = Some(handle);
  }

  pub(crate) fn wait(&self) -> Result<(), Error> {
    if let Some(handle) = self.handle.lock().take() {
      handle.join().expect("io loop")?
    }
    Ok(())
  }
}

#[derive(Debug, PartialEq)]
enum Status {
  Initial,
  Setup,
  Stop,
}

pub(crate) struct IoLoop<T> {
  connection:     Connection,
  socket:         T,
  status:         Status,
  poll:           Poll,
  registration:   Registration,
  set_readiness:  SetReadiness,
  hb_handle:      Option<JoinHandle<()>>,
  frame_size:     usize,
  receive_buffer: Buffer,
  send_buffer:    Buffer,
  can_write:      bool,
  can_read:       bool,
  has_data:       bool,
  send_heartbeat: Arc<AtomicBool>,
}

impl<T: Evented + Read + Write + Send + 'static> IoLoop<T> {
  pub(crate) fn new(connection: Connection, socket: T) -> Result<Self, Error> {
    let frame_size = std::cmp::max(8192, connection.configuration().frame_max() as usize);
    let (registration, set_readiness) = Registration::new2();
    let inner = Self {
      connection,
      socket,
      status:         Status::Initial,
      poll:           Poll::new().map_err(ErrorKind::IOError)?,
      registration,
      set_readiness,
      hb_handle:      None,
      frame_size,
      receive_buffer: Buffer::with_capacity(FRAMES_STORAGE * frame_size),
      send_buffer:    Buffer::with_capacity(FRAMES_STORAGE * frame_size),
      can_write:      false,
      can_read:       false,
      has_data:       false,
      send_heartbeat: Arc::new(AtomicBool::new(false)),
    };
    inner.poll.register(&inner.socket, SOCKET, Ready::readable() | Ready::writable(), PollOpt::edge()).map_err(ErrorKind::IOError)?;
    inner.poll.register(&inner.connection, DATA, Ready::readable(), PollOpt::edge()).map_err(ErrorKind::IOError)?;
    inner.poll.register(&inner.registration, CONTINUE, Ready::readable(), PollOpt::edge()).map_err(ErrorKind::IOError)?;
    Ok(inner)
  }

  fn start_heartbeat(&mut self, interval: Duration) -> Result<(), Error> {
    let connection    = self.connection.clone();
    let send_hartbeat = self.send_heartbeat.clone();
    let hb_handle = ThreadBuilder::new().name("heartbeat".to_owned()).spawn(move || {
      while connection.status().connected() {
        let start         = Instant::now();
        let mut remaining = interval;

        loop {
          thread::park_timeout(remaining);
          let elapsed = start.elapsed();
          if elapsed >= remaining {
            break
          }
          remaining -= interval - elapsed;
        }

        send_hartbeat.store(true, Ordering::Relaxed);
      }
    }).map_err(ErrorKind::IOError)?;
    self.hb_handle = Some(hb_handle);
    Ok(())
  }

  fn heartbeat(&mut self) -> Result<(), Error> {
    self.connection.send_heartbeat()?;
    self.send_heartbeat.store(false, Ordering::Relaxed);
    Ok(())
  }

  fn ensure_setup(&mut self) -> Result<(), Error> {
    if self.status != Status::Setup && self.connection.status().connected() {
      let frame_max = self.connection.configuration().frame_max() as usize;
      self.frame_size = std::cmp::max(self.frame_size, frame_max);
      self.receive_buffer.grow(FRAMES_STORAGE * self.frame_size);
      self.send_buffer.grow(FRAMES_STORAGE * self.frame_size);
      let heartbeat = self.connection.configuration().heartbeat();
      if heartbeat != 0 {
        trace!("io_loop: start heartbeat");
        self.start_heartbeat(Duration::from_secs(heartbeat as u64))?;
        trace!("io_loop: heartbeat started");
      }
      self.status = Status::Setup;
    }
    Ok(())
  }

  fn wants_to_write(&self) -> bool {
    self.can_write && self.has_data
  }

  fn wants_to_read(&self) -> bool {
    self.can_read
  }

  fn should_continue(&self) -> bool {
    let connection_status = self.connection.status();
    (self.status == Status::Initial || connection_status.connected() || connection_status.closing()) && self.status != Status::Stop && !connection_status.errored()
  }

  pub(crate) fn run(mut self) -> Result<(), Error> {
    self.connection.clone().set_io_loop(ThreadBuilder::new().name("io_loop".to_owned()).spawn(move || {
      let mut events = Events::with_capacity(1024);
      while self.should_continue() {
        self.do_run(&mut events)?;
      }
      if let Some(hb_handle) = self.hb_handle.take() {
        hb_handle.thread().unpark();
        hb_handle.join().expect("heartbeat loop failed");
      }
      Ok(())
    }).map_err(ErrorKind::IOError)?);
    Ok(())
  }

  fn do_run(&mut self, events: &mut Events) -> Result<(), Error> {
    // First, update our internal state
    trace!("io_loop run");
    self.ensure_setup()?;
    trace!("io_loop poll");
    self.poll.poll(events, None).map_err(ErrorKind::IOError)?;
    trace!("io_loop poll done");
    for event in events.iter() {
      match event.token() {
        SOCKET    => {
          if event.readiness().is_readable() {
            self.can_read = true;
          }
          if event.readiness().is_writable() {
            self.can_write = true;
          }
        },
        DATA      => self.has_data = true,
        _         => {},
      }
    }

    // Then, actually run an iteration of the event loop
    trace!("io_loop do_run; can_read={}, can_write={}, has_data={}", self.can_read, self.can_write, self.has_data);
    loop {
      if self.send_heartbeat.load(Ordering::Relaxed) {
        self.heartbeat()?;
      }
      if self.wants_to_write() && !self.connection.status().blocked() {
        if let Err(e) = self.write_to_stream() {
          match e.kind() {
            ErrorKind::IOError(e) if e.kind() == io::ErrorKind::WouldBlock => self.can_write = false,
            _ => {
              error!("error writing: {:?}", e);
              if let ConnectionState::SentProtocolHeader(wait_handle, ..) = self.connection.status().state() {
                wait_handle.error(ErrorKind::ConnectionRefused.into());
                self.status = Status::Stop;
              }
              self.connection.set_error()?;
              return Err(e);
            }
          }
        }
        self.send_buffer.shift_unless_available(self.frame_size);
      }
      if self.connection.status().closed() {
        self.status = Status::Stop;
      }
      if self.should_continue() && self.wants_to_read() {
        if let Err(e) = self.read_from_stream() {
          match e.kind() {
            ErrorKind::IOError(e) if e.kind() == io::ErrorKind::WouldBlock => self.can_read = false,
            _ => {
              error!("error reading: {:?}", e);
              self.connection.set_error()?;
              return Err(e);
            }
          }
        }
        self.receive_buffer.shift_unless_available(self.frame_size);
      }
      if self.can_parse() {
        self.parse()?;
      }
      if !self.wants_to_read() || !self.wants_to_write() || self.status == Status::Stop || self.connection.status().errored() {
        if self.status != Status::Stop && (self.wants_to_read() || self.can_parse() || self.has_data) {
          trace!("io_loop send continue; can_read={}, can_write={}, has_data={}", self.can_read, self.can_write, self.has_data);
          self.set_readiness.set_readiness(Ready::readable()).map_err(ErrorKind::IOError)?;
        }
        break;
      }
    }
    trace!("io_loop do_run done; can_read={}, can_write={}, has_data={}, status={:?}", self.can_read, self.can_write, self.has_data, self.status);
    Ok(())
  }

  fn can_parse(&self) -> bool {
    self.receive_buffer.available_data() > 0
  }

  fn write_to_stream(&mut self) -> Result<(), Error> {
    self.serialize()?;

    self.socket.write(&self.send_buffer.data()).map(|sz| {
      trace!("wrote {} bytes", sz);
      self.send_buffer.consume(sz);
    }).map_err(|e| ErrorKind::IOError(e).into())
  }

  fn read_from_stream(&mut self) -> Result<(), Error> {
    match self.connection.status().state() {
      ConnectionState::Closed => Ok(()),
      ConnectionState::Error  => Err(ErrorKind::InvalidConnectionState(ConnectionState::Error).into()),
      _                       => self.socket.read(&mut self.receive_buffer.space()).map(|sz| {
        trace!("read {} bytes", sz);
        self.receive_buffer.fill(sz);
      }).map_err(|e| ErrorKind::IOError(e).into()),
    }
  }

  fn serialize(&mut self) -> Result<(), Error> {
    if let Some((send_id, next_msg)) = self.connection.next_frame() {
      trace!("will write to buffer: {:?}", next_msg);
      match gen_frame(self.send_buffer.space(), &next_msg).map(|tup| tup.0) {
        Ok(sz) => {
          self.send_buffer.fill(sz);
          self.connection.mark_sent(send_id);
          Ok(())
        },
        Err(e) => {
          match e {
            GenError::BufferTooSmall(_) => {
              // Requeue msg
              self.connection.requeue_frame(send_id, next_msg)?;
              self.send_buffer.shift();
              Ok(())
            },
            GenError::InvalidOffset | GenError::CustomError(_) | GenError::NotYetImplemented => {
              error!("error generating frame: {:?}", e);
              self.connection.set_error()?;
              Err(ErrorKind::SerialisationError(e).into())
            }
          }
        }
      }
    } else {
      self.has_data = false;
      Ok(())
    }
  }

  fn parse(&mut self) -> Result<(), Error> {
    match parse_frame(self.receive_buffer.data()) {
      Ok((i, f)) => {
        let consumed = self.receive_buffer.data().offset(i);
        self.receive_buffer.consume(consumed);

        if let Err(e) = self.connection.handle_frame(f) {
          self.connection.set_error()?;
          Err(e)
        } else {
          Ok(())
        }
      },
      Err(e) => {
        if e.is_incomplete() {
          self.receive_buffer.shift();
          Ok(())
        } else {
          error!("parse error: {:?}", e);
          self.connection.set_error()?;
          Err(ErrorKind::ParsingError(format!("{:?}", e)).into())
        }
      }
    }
  }
}
