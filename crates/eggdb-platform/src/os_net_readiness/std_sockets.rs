use crate::os_net::OsStream;
use std::io;
use std::net::{TcpListener, TcpStream};

pub(crate) fn accept(listener: &TcpListener) -> io::Result<OsStream> {
    listener
        .accept()
        .and_then(|(stream, _)| OsStream::set_up(stream))
}

pub(crate) fn prepare_listener(_: &TcpListener) -> io::Result<()> {
    Ok(())
}

pub(crate) fn suppress_sigpipe(_: &TcpStream) -> io::Result<()> {
    Ok(())
}
