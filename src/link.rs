// SPDX-License-Identifier: ISC

/// Errors a link can report.
#[derive(Debug)]
pub enum LinkError {
    /// Device not found / adapter absent (retry with backoff, same policy).
    NotFound,
    /// Connection established then died, or connect attempt failed transiently.
    Dead(#[allow(dead_code)] String),
}

/// A single connection to the treadmill: notifications in, packets out.
pub enum Conn {
    Sim(crate::sim::SimConn),
    Ble(crate::ble::BleConn),
}

/// A connection-maker: connects (and is reconnectable by the daemon's
/// retry loop). Abstracted so the supervisor (§4.5a sleep/wake + §4.1
/// reconnect policy) can be tested with scripted links.
pub trait LinkT {
    type Conn: Notify;
    async fn connect(&mut self) -> Result<Self::Conn, LinkError>;
}

/// A connection-maker: connects (and is reconnectable by the daemon's retry loop).
pub enum Link {
    Sim(crate::sim::SimLink),
    Ble(crate::ble::BleLink),
}

impl LinkT for Link {
    type Conn = Conn;

    async fn connect(&mut self) -> Result<Conn, LinkError> {
        match self {
            Link::Sim(l) => Ok(Conn::Sim(l.connect().await?)),
            Link::Ble(l) => Ok(Conn::Ble(l.connect().await?)),
        }
    }
}

/// Abstract notification/packet pair, so the core pump and its tests can
/// drive any connection: real Link/Conn enums or scripted fake links.
pub trait Notify {
    /// Next notification, raw as the treadmill sent it. Must be
    /// cancel-safe: with a timer keep-alive (§3.7) the pump drops a
    /// pending call on every tick and calls again, so a dropped call must
    /// not lose a packet or restart its wait.
    async fn recv_notification(&mut self) -> Result<Vec<u8>, LinkError>;
    async fn write_packet(&mut self, pkt: &[u8]) -> Result<(), LinkError>;
    /// Device address for the API view (§6.2).
    fn address(&self) -> String;
    /// Drop the underlying LE link, if any (§4.6 / REVIEW round-2 A4).
    async fn disconnect(&mut self);
    /// Protocol family of the connected treadmill (§3.7).
    fn variant(&self) -> crate::protocol::Variant {
        crate::protocol::Variant::PitPat
    }
}

macro_rules! delegate {
    ($($v:ident),*) => {
        impl Notify for Conn {
            async fn recv_notification(&mut self) -> Result<Vec<u8>, LinkError> {
                match self { $(Conn::$v(c) => c.recv_notification().await,)* }
            }
            async fn write_packet(&mut self, pkt: &[u8]) -> Result<(), LinkError> {
                match self { $(Conn::$v(c) => c.write_packet(pkt).await,)* }
            }
            fn address(&self) -> String {
                match self { $(Conn::$v(c) => c.address.clone(),)* }
            }
            async fn disconnect(&mut self) {
                match self { $(Conn::$v(c) => c.disconnect().await,)* }
            }
            fn variant(&self) -> crate::protocol::Variant {
                match self { $(Conn::$v(c) => c.variant,)* }
            }
        }
    };
}

delegate!(Sim, Ble);
