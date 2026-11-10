// SPDX-License-Identifier: ISC

/// Treadmill GATT service UUID (PitPat).
pub const SERVICE_UUID: bluer::Uuid = bluer::Uuid::from_u128(0x0000fba0_0000_1000_8000_00805f9b34fb);
/// Command write characteristic.
pub const WRITE_UUID: bluer::Uuid = bluer::Uuid::from_u128(0x0000fba1_0000_1000_8000_00805f9b34fb);
/// Status notify characteristic.
pub const NOTIFY_UUID: bluer::Uuid = bluer::Uuid::from_u128(0x0000fba2_0000_1000_8000_00805f9b34fb);

/// Minimal compile-only stub for the skeleton.
pub fn link_stub() {}
