// SPDX-License-Identifier: ISC

/// Errors a link can report.
#[derive(Debug)]
pub enum LinkError {
    /// Device not found / adapter absent (retry with backoff, same policy).
    NotFound,
    /// Connection established then died, or connect attempt failed transiently.
    Dead(String),
}

/// A single connection to the treadmill: notifications in, packets out.
pub enum Conn {
    Sim(crate::sim::SimConn),
    Ble(crate::ble::BleConn),
}

impl Conn {
    /// Wait for the next raw notification. `Err` ⇒ the connection is dead
    /// (the caller ends the session and reconnects).
    pub async fn recv_notification(&mut self) -> Result<Vec<u8>, LinkError> {
        match self {
            Conn::Sim(c) => c.recv_notification().await,
            Conn::Ble(c) => c.recv_notification().await,
        }
    }

    /// Write a command/heartbeat packet.
    pub async fn write_packet(&mut self, pkt: &[u8]) -> Result<(), LinkError> {
        match self {
            Conn::Sim(c) => c.write_packet(pkt).await,
            Conn::Ble(c) => c.write_packet(pkt).await,
        }
    }

    /// Address reported by the device, if known.
    pub fn address(&self) -> String {
        match self {
            Conn::Sim(_) => "00:11:22:33:44:55".to_string(),
            Conn::Ble(c) => c.address.clone(),
        }
    }
}

/// A connection-maker: connects (and is reconnectable by the daemon's retry loop).
pub enum Link {
    Sim(crate::sim::SimLink),
    Ble(crate::ble::BleLink),
}

impl Link {
    pub async fn connect(&mut self) -> Result<Conn, LinkError> {
        match self {
            Link::Sim(l) => l.connect().await.map(Conn::Sim),
            Link::Ble(l) => l.connect().await.map(Conn::Ble),
        }
    }

    pub fn is_sim(&self) -> bool {
        matches!(self, Link::Sim(_))
    }
}
