use super::queue::{Connecting, Interest, Readiness};
use crate::net::NetBackend;
use std::io;
use std::net::{SocketAddr, TcpStream};
use std::os::fd::RawFd;
use std::time::Duration;

pub(super) const BACKEND: NetBackend = NetBackend::Poll;

#[derive(Debug)]
pub(super) enum Selector {}

impl Selector {
    pub(super) fn new(_: usize) -> io::Result<Self> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    pub(super) fn want(&mut self, _: RawFd, _: Interest) -> io::Result<()> {
        match *self {}
    }

    pub(super) fn wait(&mut self, _: Option<Duration>, _: &mut Vec<Readiness>) -> io::Result<()> {
        match *self {}
    }
}

pub(super) fn connect_without_waiting(address: SocketAddr) -> io::Result<Connecting> {
    let socket = TcpStream::connect(address)?;
    socket.set_nonblocking(true)?;
    Ok(Connecting::Connected(socket))
}
