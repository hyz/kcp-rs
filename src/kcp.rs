use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bytes::{Buf, BufMut, ByteOrder, LittleEndian};
use ctime;
use futures::stream::Stream;
use futures::{Poll, Async, Future};
use iovec::IoVec;
use mio::event::Evented;
use mio::{self, Ready, Registration, PollOpt, Token, SetReadiness};
use rand;
use tokio_core::net::UdpSocket;
use tokio_core::reactor::{Handle, PollEvented, Timeout};
use tokio_io::{AsyncRead, AsyncWrite};

use Kcb;

struct KcpPair {
    k: Rc<RefCell<Kcb<KcpOutput>>>,
    set_readiness: SetReadiness,
    token: Rc<RefCell<Timeout>>,
}

pub struct KcpListener {
    udp: Rc<UdpSocket>,
    connections: HashMap<SocketAddr, KcpPair>,
    handle: Handle,
}

pub struct Incoming {
    inner: KcpListener,
}

impl KcpListener {
    pub fn bind(addr: &SocketAddr, handle: &Handle) -> io::Result<KcpListener> {
        let udp = UdpSocket::bind(addr, handle).unwrap();
        let listener = KcpListener {
            udp: Rc::new(udp),
            connections: HashMap::new(),
            handle: handle.clone(),
        };
        Ok(listener)
    }

    pub fn accept(&mut self) -> io::Result<(KcpStream, SocketAddr)> {
        let mut buf = vec![0; 1024];
        loop {
            match self.udp.recv_from(&mut buf) {
                Err(e) => {
                    return Err(e);
                }
                Ok((n, addr)) => {
                    if self.connections.contains_key(&addr) {
                        if let Some(kp) = self.connections.get(&addr) {
                            let mut kcb = kp.k.borrow_mut();
                            kcb.input(&buf[..n]);

                            kcb.update(clock());
                            let dur = kcb.check(clock());
                            kp.token.borrow_mut().reset(
                                Instant::now() +
                                    Duration::from_millis(dur as u64),
                            );

                            kp.set_readiness.set_readiness(mio::Ready::readable());
                        }
                    } else {
                        let conv = LittleEndian::read_u32(&buf[..4]);
                        let mut kcb = Kcb::new(
                            conv,
                            KcpOutput {
                                udp: self.udp.clone(),
                                peer: addr.clone(),
                            },
                        );
                        kcb.wndsize(128, 128);
                        kcb.nodelay(0, 10, 0, true);
                        let kcb = Rc::new(RefCell::new(kcb));
                        let (registration, set_readiness) = Registration::new2();
                        let now = Instant::now();
                        let token = Timeout::new_at(now, &self.handle).unwrap();
                        let token = Rc::new(RefCell::new(token));
                        let core = KcpCore {
                            kcb: kcb.clone(),
                            registration: registration,
                            set_readiness: set_readiness.clone(),
                            token: token.clone(),
                        };
                        let interval = KcpInterval {
                            kcb: kcb.clone(),
                            token: token.clone(),
                        };
                        &self.handle.spawn(
                            interval.for_each(|_| Ok(())).then(|_| Ok(())),
                        );
                        let io = PollEvented::new(core, &self.handle).unwrap();
                        let stream = KcpStream { io: io };
                        stream.io.get_ref().kcb.borrow_mut().input(&buf[..n]);

                        let kcbc = kcb.clone();
                        let mut kcb1 = kcbc.borrow_mut();
                        kcb1.update(clock());
                        let dur = kcb1.check(clock());
                        token.borrow_mut().reset(
                            Instant::now() +
                                Duration::from_millis(dur as u64),
                        );

                        stream.io.get_ref().set_readiness.set_readiness(
                            mio::Ready::readable(),
                        );

                        let kp = KcpPair {
                            k: kcb.clone(),
                            set_readiness: set_readiness.clone(),
                            token: token.clone(),
                        };
                        self.connections.insert(addr, kp);
                        return Ok((stream, addr));
                    }
                }
            }
        }
    }

    pub fn incoming(self) -> Incoming {
        Incoming { inner: self }
    }
}

impl Stream for Incoming {
    type Item = (KcpStream, SocketAddr);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, io::Error> {
        Ok(Async::Ready(Some(try_nb!(self.inner.accept()))))
    }
}

struct Server {
    socket: Rc<UdpSocket>,
    buf: Vec<u8>,
    to_send: Option<(usize, SocketAddr)>,
    kcb: Rc<RefCell<Kcb<KcpOutput>>>,
    set_readiness: SetReadiness,

    token: Rc<RefCell<Timeout>>,
}

impl Future for Server {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), io::Error> {
        loop {
            if let Some((size, peer)) = self.to_send {
                let mut kcb = self.kcb.borrow_mut();
                kcb.input(&self.buf[..size]);

                kcb.update(clock());
                let dur = kcb.check(clock());
                self.token.borrow_mut().reset(
                    Instant::now() +
                        Duration::from_millis(dur as u64),
                );

                self.set_readiness.set_readiness(mio::Ready::readable());
                self.to_send = None;
            }

            self.to_send = Some(try_nb!(self.socket.recv_from(&mut self.buf)));
        }
    }
}

pub struct KcpStreamNew {
    inner: Option<KcpStream>,
}

impl Future for KcpStreamNew {
    type Item = KcpStream;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<KcpStream, io::Error> {
        Ok(Async::Ready(self.inner.take().unwrap()))
    }
}

struct KcpInterval {
    kcb: Rc<RefCell<Kcb<KcpOutput>>>,
    token: Rc<RefCell<Timeout>>,
}

impl Stream for KcpInterval {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<()>, io::Error> {
        let mut token = self.token.borrow_mut();
        match token.poll() {
            Ok(Async::Ready(())) => {
                let mut kcb = self.kcb.borrow_mut();
                kcb.update(clock());
                let dur = kcb.check(clock());
                let next = Instant::now() + Duration::from_millis(dur as u64);
                token.reset(next);
                Ok(Async::Ready(Some(())))
            }
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(e) => Err(e),
        }
    }
}

struct KcpCore {
    kcb: Rc<RefCell<Kcb<KcpOutput>>>,
    registration: Registration,
    set_readiness: SetReadiness,
    token: Rc<RefCell<Timeout>>,
}

impl KcpCore {
    pub fn read_bufs(&self, bufs: &mut [&mut IoVec]) -> io::Result<usize> {
        unimplemented!()
    }

    pub fn write_bufs(&self, bufs: &[&IoVec]) -> io::Result<usize> {
        unimplemented!()
    }
}

impl Read for KcpCore {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let result = self.kcb.borrow_mut().recv(buf);
        match result {
            Err(e) => Err(io::Error::new(io::ErrorKind::WouldBlock, "would block")),
            Ok(n) => Ok(n),
        }
    }
}

impl Write for KcpCore {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut kcb = self.kcb.borrow_mut();
        let result = kcb.send(buf);
        kcb.update(clock());
        let dur = kcb.check(clock());
        kcb.flush();
        self.token.borrow_mut().reset(
            Instant::now() +
                Duration::from_millis(dur as u64),
        );
        result
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Evented for KcpCore {
    fn register(
        &self,
        poll: &mio::Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        self.registration.register(poll, token, interest, opts)
    }

    fn reregister(
        &self,
        poll: &mio::Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        self.registration.reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &mio::Poll) -> io::Result<()> {
        self.registration.deregister(poll)
    }
}

pub struct KcpStream {
    io: PollEvented<KcpCore>,
}

impl KcpStream {
    pub fn connect(addr: &SocketAddr, handle: &Handle) -> KcpStreamNew {
        let r: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let udp = UdpSocket::bind(&r, handle).unwrap();
        let udp = Rc::new(udp);
        let conv = rand::random::<u32>();
        let mut kcb = Kcb::new(
            conv,
            KcpOutput {
                udp: udp.clone(),
                peer: addr.clone(),
            },
        );
        kcb.wndsize(128, 128);
        kcb.nodelay(0, 10, 0, true);
        let kcb = Rc::new(RefCell::new(kcb));
        let (registration, set_readiness) = Registration::new2();
        let now = Instant::now();
        let token = Timeout::new_at(now, handle).unwrap();
        let token = Rc::new(RefCell::new(token));
        let core = KcpCore {
            kcb: kcb.clone(),
            registration: registration,
            set_readiness: set_readiness.clone(),
            token: token.clone(),
        };

        let interval = KcpInterval {
            kcb: kcb.clone(),
            token: token.clone(),
        };
        handle.spawn(interval.for_each(|_| Ok(())).then(|_| Ok(())));
        let io = PollEvented::new(core, handle).unwrap();
        let inner = KcpStream { io: io };
        handle.spawn(
            Server {
                socket: udp.clone(),
                buf: vec![0; 1024],
                to_send: None,
                kcb: kcb.clone(),
                set_readiness: set_readiness.clone(),
                token: token.clone(),
            }.then(|_| Ok(())),
        );
        KcpStreamNew { inner: Some(inner) }
    }


    pub fn poll_read(&self) -> Async<()> {
        self.io.poll_read()
    }

    pub fn poll_write(&self) -> Async<()> {
        self.io.poll_write()
    }
}

impl Read for KcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.io.read(buf)
    }
}

impl Write for KcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // TODO
        self.io.get_ref().set_readiness.set_readiness(
            mio::Ready::writable(),
        );
        self.io.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

impl AsyncRead for KcpStream {
    unsafe fn prepare_uninitialized_buffer(&self, _: &mut [u8]) -> bool {
        false
    }

    fn read_buf<B: BufMut>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        <&KcpStream>::read_buf(&mut &*self, buf)
    }
}

impl AsyncWrite for KcpStream {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        <&KcpStream>::shutdown(&mut &*self)
    }

    fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        <&KcpStream>::write_buf(&mut &*self, buf)
    }
}

impl<'a> Read for &'a KcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        unimplemented!()
    }
}

impl<'a> Write for &'a KcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        unimplemented!()
    }

    fn flush(&mut self) -> io::Result<()> {
        unimplemented!()
    }
}

impl<'a> AsyncRead for &'a KcpStream {
    unsafe fn prepare_uninitialized_buffer(&self, _: &mut [u8]) -> bool {
        false
    }

    fn read_buf<B: BufMut>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        if let Async::NotReady = <KcpStream>::poll_read(self) {
            return Ok(Async::NotReady);
        }
        let r = unsafe {
            let mut bufs: [_; 16] = Default::default();
            let n = buf.bytes_vec_mut(&mut bufs);
            self.io.get_ref().read_bufs(&mut bufs[..n])
        };

        match r {
            Ok(n) => {
                unsafe {
                    buf.advance_mut(n);
                }
                Ok(Async::Ready(n))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.io.need_read();
                Ok(Async::NotReady)
            }
            Err(e) => Err(e),
        }
    }
}

impl<'a> AsyncWrite for &'a KcpStream {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        Ok(().into())
    }

    fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        if let Async::NotReady = <KcpStream>::poll_write(self) {
            return Ok(Async::NotReady);
        }
        let r = {
            let mut bufs: [_; 16] = Default::default();
            let n = buf.bytes_vec(&mut bufs);
            self.io.get_ref().write_bufs(&bufs[..n])
        };
        match r {
            Ok(n) => {
                buf.advance(n);
                Ok(Async::Ready(n))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.io.need_write();
                Ok(Async::NotReady)
            }
            Err(e) => Err(e),
        }
    }
}

#[inline]
fn clock() -> u32 {
    let timespec = ctime::get_time();
    let mills = timespec.sec * 1000 + timespec.nsec as i64 / 1000 / 1000;
    mills as u32
}

pub struct KcpOutput {
    udp: Rc<UdpSocket>,
    peer: SocketAddr,
}

impl Write for KcpOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.udp.send_to(buf, &self.peer)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
