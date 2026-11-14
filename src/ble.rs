// SPDX-License-Identifier: ISC

//! Real treadmill over BlueZ (bluer's bluetoothd D-Bus API), §4.1.

use crate::link::LinkError;
use crate::protocol::{NOTIFY_UUID, SERVICE_UUID, WRITE_UUID};
use bluer::gatt::WriteOp;
use bluer::gatt::remote::{Characteristic, CharacteristicWriteRequest};
use bluer::{
    Adapter, AdapterEvent, Address, AddressType, Device, DiscoveryFilter, DiscoveryTransport,
    Session,
};
use futures::{Stream, StreamExt, pin_mut};
use std::collections::HashSet;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

/// Look up the treadmill: pinned address if set, otherwise scan for the
/// PitPat advertising service and take the first match. No pairing.
pub struct BleLink {
    pub adapter_name: Option<String>,
    pub address: Option<String>,
    /// §4.5a/D1: directory used to remember the pinned device's LE
    /// address type (public/random) after the first successful
    /// ConnectDevice, so later wakes don't have to probe both.
    pub state_dir: Option<PathBuf>,
    /// Set once when ConnectDevice is missing from BlueZ (needs
    /// hardware.bluetooth Experimental), so the wake path warns instead
    /// of re-scanning silently.
    connect_device_ok: bool,
}

impl BleLink {
    pub fn new(
        adapter_name: Option<String>,
        address: Option<String>,
        state_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            adapter_name,
            address,
            state_dir,
            connect_device_ok: true,
        }
    }

    pub async fn connect(&mut self) -> Result<BleConn, LinkError> {
        let session = Session::new().await.map_err(perr)?;
        let adapter = match &self.adapter_name {
            Some(sel) => find_adapter(&session, sel).await?,
            None => session.default_adapter().await.map_err(perr)?,
        };
        adapter.set_powered(true).await.map_err(perr)?;
        let adapter_name = adapter.name();
        tracing::info!(adapter = adapter_name, "connecting over BlueZ");

        let device = match &self.address {
            Some(addr) => {
                connect_pinned(
                    &adapter,
                    addr,
                    self.state_dir.as_ref(),
                    &mut self.connect_device_ok,
                )
                .await?
            }
            None => scan_and_connect(&adapter).await?,
        };
        let dev_addr = device.address().to_string();
        tracing::info!(device = %dev_addr, "BLE connected");
        let chars = find_characteristics(&device).await?;
        let notify = chars.notify.notify().await.map_err(perr)?;
        tracing::debug!("notifications subscribed");
        Ok(BleConn {
            address: dev_addr,
            device,
            write_char: chars.write,
            notify: Box::pin(notify),
        })
    }
}

struct FoundChar {
    write: Characteristic,
    notify: Characteristic,
}

/// Resolve `--adapter` on every attempt, by name (`hci0`) or by the
/// controller's MAC address. Names are not stable: after a controller
/// reset BlueZ can re-register the same adapter as a different `hciN`, and
/// a pinned name then fails forever with "object not present". When it is
/// missing, say which adapters do exist.
async fn find_adapter(session: &Session, sel: &str) -> Result<Adapter, LinkError> {
    let mut seen = Vec::new();
    for name in session.adapter_names().await.map_err(perr)? {
        let adapter = session.adapter(&name).map_err(perr)?;
        let addr = adapter.address().await.map_err(perr)?;
        if name == sel || addr.to_string().eq_ignore_ascii_case(sel) {
            return Ok(adapter);
        }
        seen.push(format!("{name} ({addr})"));
    }
    let seen = if seen.is_empty() {
        "none".to_string()
    } else {
        seen.join(", ")
    };
    Err(LinkError::Dead(format!(
        "adapter {sel} not present; available: {seen}"
    )))
}

/// Max LE discovery window per attempt (REVIEW round-5 C1): some USB
/// dongles reset when scans overlap, so scanning must be short and
/// never back-to-back (the daemon backs off from the attempt's end).
const DISCOVERY_S: u64 = 10;

/// Start (or join) an LE discovery and yield adapter events. An
/// "Operation already in progress" from StartDiscovery is harmless:
/// someone else (bluetoothctl, the desktop) is scanning, so just watch.
async fn discovery_events(
    adapter: &Adapter,
) -> Result<Pin<Box<dyn Stream<Item = AdapterEvent> + Send>>, LinkError> {
    match adapter.discover_devices().await {
        Ok(stream) => Ok(Box::pin(stream)),
        Err(err) if err.kind == bluer::ErrorKind::InProgress => {
            tracing::info!("discovery already running elsewhere; watching for events");
            let events = adapter.events().await.map_err(perr)?;
            Ok(Box::pin(events))
        }
        Err(err) => Err(perr(err)),
    }
}

/// Pinned address (REVIEW rounds 6-7, D1/E3): never scan except when
/// BlueZ lacks ConnectDevice.
/// 1. BlueZ knows the device → plain connect (BlueZ does the LE
///    connection itself). Skip ConnectDevice afterwards: it could only
///    return AlreadyExists.
/// 2. BlueZ forgot the device (normal after a 5-min sleep): ConnectDevice
///    connects without a scan but needs BlueZ's experimental flag. With
///    a cached address type only that type is tried (each failed type
///    costs a BlueZ connect timeout); otherwise LePublic then LeRandom.
///    Any other failure returns the error — the supervisor's budget and
///    backoff handle the retries; scanning must not happen (the user's
///    RTL8761BU resets under overlapping scans). Only when ConnectDevice
///    is unsupported does a short discovery run.
async fn connect_pinned(
    adapter: &Adapter,
    addr: &str,
    state_dir: Option<&PathBuf>,
    connect_device_ok: &mut bool,
) -> Result<Device, LinkError> {
    let address: Address = addr
        .parse()
        .map_err(|_| LinkError::Dead("bad --address".into()))?;

    // 1. known device: plain connect, no scanning
    if let Ok(dev) = adapter.device(address) {
        match connect_with_retries(&dev).await {
            Ok(()) => return Ok(dev),
            Err(err) => {
                // step 2 can only fail with AlreadyExists now
                tracing::info!(error = %err, "direct connect failed; using ConnectDevice");
            }
        }
    }

    // 2. ConnectDevice without a scan (experimental BlueZ feature)
    if !*connect_device_ok {
        return Err(LinkError::NotFound);
    }
    let mut last_err = None;
    let cached = cached_type(state_dir, addr);
    let types: Vec<AddressType> = match cached {
        Some(ty) => vec![ty], // one failed type costs a BlueZ connect timeout
        None => vec![AddressType::LePublic, AddressType::LeRandom],
    };
    for ty in types {
        match adapter.connect_device(address, ty).await {
            Ok(dev) => {
                save_type(state_dir, addr, ty);
                tracing::info!(?ty, "connected via ConnectDevice without scanning");
                return Ok(dev);
            }
            Err(err) => {
                tracing::debug!(?ty, error = %err, "ConnectDevice failed");
                last_err = Some(err);
            }
        }
    }
    let err = last_err.expect("at least one address type tried");
    if err.kind == bluer::ErrorKind::NotSupported
        || err.kind == bluer::ErrorKind::NotAvailable
        || err.message.contains("ConnectDevice")
    {
        *connect_device_ok = false;
        tracing::warn!(
            "BlueZ lacks ConnectDevice (needs hardware.bluetooth.settings.\
                 General.Experimental = true): every wake will run a short scan"
        );
        // 3. discovery fallback, only for an unsupported BlueZ (short,
        // capped)
        let dev = discover_pinned(adapter, addr).await?;
        connect_with_retries(&dev).await.map_err(perr)?;
        return Ok(dev);
    }
    // the treadmill is likely off or out of reach: return the error and
    // let the supervisor's budget/backoff retry (E3)
    Err(perr(err))
}

/// Cached LE address type for a pinned address, from the state dir.
fn cached_type(state_dir: Option<&PathBuf>, addr: &str) -> Option<AddressType> {
    let dir = state_dir?;
    let text = std::fs::read_to_string(dir.join("address-type")).ok()?;
    let (mac, ty) = text.trim().split_once(' ')?;
    if mac != addr {
        return None;
    }
    Some(match ty {
        "public" => AddressType::LePublic,
        "random" => AddressType::LeRandom,
        _ => return None,
    })
}

/// Remember which LE address type connected, for future wake attempts.
fn save_type(state_dir: Option<&PathBuf>, addr: &str, ty: AddressType) {
    let Some(dir) = state_dir else { return };
    if let Err(err) = std::fs::create_dir_all(dir) {
        tracing::warn!(error = %err, "cannot create the state dir");
        return;
    }
    let ty = match ty {
        AddressType::LePublic => "public",
        AddressType::LeRandom => "random",
        AddressType::BrEdr => return,
    };
    if let Err(err) = std::fs::write(dir.join("address-type"), format!("{addr} {ty}\n")) {
        tracing::warn!(error = %err, "cannot cache the address type");
    }
}

/// Discovery fallback for a pinned address, capped at DISCOVERY_S.
async fn discover_pinned(adapter: &Adapter, addr: &str) -> Result<Device, LinkError> {
    let address: Address = addr
        .parse()
        .map_err(|_| LinkError::Dead("bad --address".into()))?;
    let filter = DiscoveryFilter {
        transport: DiscoveryTransport::Le,
        pattern: Some(addr.to_string()),
        ..Default::default()
    };
    adapter.set_discovery_filter(filter).await.map_err(perr)?;
    let events = discovery_events(adapter).await?;
    pin_mut!(events);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(DISCOVERY_S);
    let result = loop {
        let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(1), events.next()).await
        else {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(addr, secs = DISCOVERY_S, "treadmill not discovered in time");
                break Err(LinkError::NotFound);
            }
            continue;
        };
        if let AdapterEvent::DeviceAdded(a) = event {
            if a == address {
                break adapter.device(a).map_err(|_| LinkError::NotFound);
            }
        }
    };
    // explicit scope end: dropping the pinned stream ends our discovery
    // session before the caller starts its backoff sleep (C1)
    result
}

/// Auto-discovery: scan with the fba0 service UUID filter and take the
/// first match; same 10 s cap and explicit stop as the pinned fallback.
async fn scan_and_connect(adapter: &Adapter) -> Result<Device, LinkError> {
    let filter = DiscoveryFilter {
        transport: DiscoveryTransport::Le,
        uuids: HashSet::from([SERVICE_UUID]),
        ..Default::default()
    };
    adapter.set_discovery_filter(filter).await.map_err(perr)?;
    let events = discovery_events(adapter).await?;
    pin_mut!(events);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(DISCOVERY_S);
    let result = loop {
        let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(1), events.next()).await
        else {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(secs = DISCOVERY_S, "no PitPat advertisement seen in time");
                break Err(LinkError::NotFound);
            }
            continue;
        };
        if let AdapterEvent::DeviceAdded(addr) = event {
            let Ok(dev) = adapter.device(addr) else {
                continue;
            };
            let uuids = dev.uuids().await.unwrap_or_default().unwrap_or_default();
            if uuids.contains(&SERVICE_UUID) {
                break Ok(dev);
            }
        }
    };
    let dev = result?;
    connect_with_retries(&dev).await.map_err(perr)?;
    Ok(dev)
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
