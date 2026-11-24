//! Bounded, single-attempt native length-prefixed transport.
use std::{
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    time::Instant,
};

pub(crate) fn exchange(port: u16, body: &[u8], deadline: Instant) -> io::Result<Vec<u8>> {
    if body.is_empty() || body.len() > 1024 * 1024 {
        return Err(io::Error::other("invalid native request size"));
    }
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "native loopback exchange deadline expired",
                )
            })
    };
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, remaining()?)?;
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(body);
    let mut sent = 0;
    while sent < frame.len() {
        stream.set_write_timeout(Some(remaining()?))?;
        let n = stream.write(&frame[sent..])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "native loopback send closed",
            ));
        }
        sent += n;
    }
    let mut read = |bytes: &mut [u8]| -> io::Result<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            stream.set_read_timeout(Some(remaining()?))?;
            let n = stream.read(&mut bytes[offset..])?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "native loopback response closed",
                ));
            }
            offset += n;
        }
        remaining()?;
        Ok(())
    };
    let mut header = [0; 4];
    read(&mut header)?;
    let size = u32::from_be_bytes(header) as usize;
    if size == 0 || size > 1024 * 1024 {
        return Err(io::Error::other("invalid native loopback response size"));
    }
    let mut bytes = vec![0; size];
    read(&mut bytes)?;
    Ok(bytes)
}
