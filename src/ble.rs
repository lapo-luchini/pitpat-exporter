// SPDX-License-Identifier: ISC

//! Real treadmill over BlueZ (bluer's bluetoothd D-Bus API), §4.1.

use crate::link::LinkError;
use crate::protocol::{NOTIFY_UUID, SERVICE_UUID, WRITE_UUID};
use bluer::gatt::WriteOp;
use bluer::gatt::remote::{Characteristic, CharacteristicWriteRequest};
use bluer::{Adapter, AdapterEvent, Address, Device, DiscoveryFilter, DiscoveryTransport, Session};
use futures::{Stream, StreamExt, pin_mut};
use std::collections::HashSet;
use std::pin::Pin;
use std::time::Duration;

/// Look up the treadmill: pinned address if set, otherwise scan for the
/// PitPat advertising service and take the first match. No pairing.
pub struct BleLink {
    pub adapter_name: Option<String>,
    pub address: Option<String>,
}

impl BleLink {
    pub fn new(adapter_name: Option<String>, address: Option<String>) -> Self {
        Self {
            adapter_name,
            address,
        }
    }

    /// One connection attempt. `NotFound` (retried with backoff) only when
    /// nothing was ever seen.
    pub async fn connect(&mut self) -> Result<crate::link::Conn, LinkError> {
        let session = Session::new().await.map_err(perr)?;
        let adapter = match &self.adapter_name {
            Some(name) => session.adapter(name).map_err(perr)?,
            None => session.default_adapter().await.map_err(perr)?,
        };
        adapter.set_powered(true).await.map_err(perr)?;
        let adapter_name = adapter.name();
        tracing::info!(adapter = adapter_name, "connecting over BlueZ");

        let device = match &self.address {
            Some(addr) => find_pinned(&adapter, addr).await?,
            None => find_scanning(&adapter).await?,
        };
        let dev_addr = device.address().to_string();
        connect_with_retries(&device).await.map_err(perr)?;
        tracing::info!(device = %dev_addr, "BLE connected");
        let chars = find_characteristics(&device).await?;
        let notify = chars.notify.notify().await.map_err(perr)?;
        tracing::debug!("notifications subscribed");
        Ok(crate::link::Conn::Ble(BleConn {
            address: dev_addr,
            device,
            write_char: chars.write,
            notify: Box::pin(notify),
        }))
    }
}

struct FoundChar {
    write: Characteristic,
    notify: Characteristic,
}

/// Pinned address: discover until BlueZ reports the exact device.
async fn find_pinned(adapter: &Adapter, addr: &str) -> Result<Device, LinkError> {
    let address: Address = addr
        .parse()
        .map_err(|_| LinkError::Dead("bad --address".into()))?;
    let filter = DiscoveryFilter {
        transport: DiscoveryTransport::Le,
        pattern: Some(addr.to_string()),
        ..Default::default()
    };
    adapter.set_discovery_filter(filter).await.map_err(perr)?;
    let events = adapter.discover_devices().await.map_err(perr)?;
    pin_mut!(events);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(1), events.next()).await
        else {
            if std::time::Instant::now() > deadline {
                tracing::warn!(addr, "treadmill not discovered within 30 s");
                return Err(LinkError::NotFound);
            }
            continue;
        };
        if let AdapterEvent::DeviceAdded(a) = event {
            if a == address {
                return adapter.device(a).map_err(|_| LinkError::NotFound);
            }
        }
    }
}

/// Auto-discovery: scan with the fba0 service UUID filtered advertisement
/// and take the first match (§4.1).
async fn find_scanning(adapter: &Adapter) -> Result<Device, LinkError> {
    let filter = DiscoveryFilter {
        transport: DiscoveryTransport::Le,
        uuids: HashSet::from([SERVICE_UUID]),
        ..Default::default()
    };
    adapter.set_discovery_filter(filter).await.map_err(perr)?;
    let events = adapter.discover_devices().await.map_err(perr)?;
    pin_mut!(events);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(1), events.next()).await
        else {
            if std::time::Instant::now() > deadline {
                tracing::warn!("no PitPat advertisement seen within 30 s");
                return Err(LinkError::NotFound);
            }
            continue;
        };
        if let AdapterEvent::DeviceAdded(addr) = event {
            let Ok(dev) = adapter.device(addr) else {
                continue;
            };
            let uuids = dev.uuids().await.unwrap_or_default().unwrap_or_default();
            if uuids.contains(&SERVICE_UUID) {
                return Ok(dev);
            }
        }
    }
}

async fn connect_with_retries(device: &Device) -> bluer::Result<()> {
    if device.is_connected().await? {
        return Ok(());
    }
    let mut retries = 2;
    loop {
        match device.connect().await {
            Ok(()) => return Ok(()),
            Err(err) if retries > 0 => {
                tracing::warn!(error = %err, "BLE connect failed, retrying");
                tokio::time::sleep(Duration::from_millis(500)).await;
                retries -= 1;
            }
            Err(err) => return Err(err),
        }
    }
}

async fn find_characteristics(device: &Device) -> Result<FoundChar, LinkError> {
    for service in device.services().await.map_err(perr)? {
        if service.uuid().await.map_err(perr)? != SERVICE_UUID {
            continue;
        }
        let mut write = None;
        let mut notify = None;
        for char in service.characteristics().await.map_err(perr)? {
            match char.uuid().await.map_err(perr)? {
                u if u == WRITE_UUID => write = Some(char),
                u if u == NOTIFY_UUID => notify = Some(char),
                _ => {}
            }
        }
        if let (Some(write), Some(notify)) = (write, notify) {
            return Ok(FoundChar { write, notify });
        }
    }
    Err(LinkError::Dead(
        "fba0/fba1/fba2 characteristics not found".into(),
    ))
}

fn perr(err: bluer::Error) -> LinkError {
    LinkError::Dead(err.to_string())
}

// --------------------------------------------------------------- conn

/// A live BLE connection: notification stream + command writes.
pub struct BleConn {
    pub address: String,
    /// Kept so the LE link can be dropped explicitly: BlueZ keeps the
    /// connection alive after the notify stream dies, which would block
    /// the treadmill's own phone app (REVIEW item 6).
    device: Device,
    write_char: Characteristic,
    notify: Pin<Box<dyn Stream<Item = Vec<u8>> + Send>>,
}

impl BleConn {
    /// Drop the LE link explicitly (shutdown, watchdog reconnect).
    pub async fn disconnect(&mut self) {
        if let Err(err) = self.device.disconnect().await {
            tracing::warn!(error = %err, "BLE disconnect failed");
        }
    }

    /// Next notification's raw bytes; `Err` ⇒ the link is dead.
    pub async fn recv_notification(&mut self) -> Result<Vec<u8>, LinkError> {
        self.notify
            .next()
            .await
            .ok_or_else(|| LinkError::Dead("notification stream ended".into()))
    }

    /// Write a command/heartbeat packet as an ATT Write Request (with
    /// response). The treadmill's fba1 characteristic is `[read, write]`
    /// and silently ignores Write Commands (without response), which is
    /// what bluer's plain `write()` sends: its default `WriteOp` is
    /// `Command`. Write Commands are kept only as a fallback for a
    /// firmware that rejects requests.
    pub async fn write_packet(&mut self, pkt: &[u8]) -> Result<(), LinkError> {
        let request = CharacteristicWriteRequest {
            op_type: WriteOp::Request,
            ..Default::default()
        };
        match self.write_char.write_ext(pkt, &request).await {
            Ok(()) => Ok(()),
            Err(err) => {
                tracing::debug!(error = %err, "write request failed, trying write command");
                let command = CharacteristicWriteRequest {
                    op_type: WriteOp::Command,
                    ..Default::default()
                };
                self.write_char.write_ext(pkt, &command).await.map_err(perr)
            }
        }
    }
}
