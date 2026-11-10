// SPDX-License-Identifier: ISC

use crate::link::LinkError;

/// Real treadmill over BlueZ (bluer, bluetoothd D-Bus API).
/// Implementation lands with the BLE connection commit.
pub struct BleLink {}

pub struct BleConn {
    /// MAC-like device address, reported via the API/metrics.
    pub address: String,
}

impl BleConn {
    pub async fn recv_notification(&mut self) -> Result<Vec<u8>, LinkError> {
        Err(LinkError::Dead("BLE not implemented yet".into()))
    }

    pub async fn write_packet(&mut self, _pkt: &[u8]) -> Result<(), LinkError> {
        Err(LinkError::Dead("BLE not implemented yet".into()))
    }
}

impl BleLink {
    pub async fn connect(&mut self) -> Result<BleConn, LinkError> {
        Err(LinkError::NotFound)
    }
}
