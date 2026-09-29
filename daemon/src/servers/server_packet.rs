use crate::primary_worker::{DeviceCommand, DeviceSender};
use anyhow::{Context, Result, anyhow};
use goxlr_ipc::{DaemonRequest, DaemonResponse};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::timeout;

// Upper bound for waiting on the primary worker, so a stalled worker produces an error
// response instead of hanging the client forever.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

// Device commands can apply a whole profile over USB, so give them longer.
const DEVICE_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

pub async fn await_response<T>(rx: oneshot::Receiver<T>, wait: Duration) -> Result<T> {
    timeout(wait, rx)
        .await
        .map_err(|_| anyhow!("Timed out waiting for the device task"))?
        .map_err(|e| anyhow!(e.to_string()))
}

pub async fn handle_packet(
    request: DaemonRequest,
    usb_tx: &mut DeviceSender,
) -> Result<DaemonResponse> {
    match request {
        DaemonRequest::Ping => Ok(DaemonResponse::Ok),
        DaemonRequest::GetStatus => {
            let (tx, rx) = oneshot::channel();
            usb_tx
                .send(DeviceCommand::SendDaemonStatus(tx))
                .await
                .map_err(|e| anyhow!(e.to_string()))
                .context("Could not communicate with the device task")?;
            Ok(DaemonResponse::Status(
                await_response(rx, RESPONSE_TIMEOUT)
                    .await
                    .context("Could not execute the command on the device task")?,
            ))
        }
        DaemonRequest::Daemon(command) => {
            let (tx, rx) = oneshot::channel();
            usb_tx
                .send(DeviceCommand::RunDaemonCommand(command, tx))
                .await
                .map_err(|e| anyhow!(e.to_string()))
                .context("Could not communicate with the GoXLR device")?;
            await_response(rx, RESPONSE_TIMEOUT)
                .await
                .context("Could not execute the command on the GoXLR device")??;
            Ok(DaemonResponse::Ok)
        }
        DaemonRequest::GetMicLevel(serial) => {
            let (tx, rx) = oneshot::channel();
            usb_tx
                .send(DeviceCommand::GetDeviceMicLevel(serial, tx))
                .await
                .map_err(|e| anyhow!(e.to_string()))
                .map_err(|e| anyhow!(e.to_string()))
                .context("Could not communicate with the GoXLR device")?;
            let result = await_response(rx, RESPONSE_TIMEOUT)
                .await
                .context("Could not execute the command on the GoXLR device")?;

            match result {
                Ok(value) => Ok(DaemonResponse::MicLevel(value)),
                Err(e) => Ok(DaemonResponse::Error(e.to_string())),
            }
        }

        DaemonRequest::GetMacOSAppLevels => {
            let (tx, rx) = oneshot::channel();
            usb_tx
                .send(DeviceCommand::GetMacOSAppLevels(tx))
                .await
                .map_err(|e| anyhow!(e.to_string()))
                .context("Could not communicate with the device task")?;
            let result = await_response(rx, RESPONSE_TIMEOUT)
                .await
                .context("Could not read the app levels")?;

            match result {
                Ok(levels) => Ok(DaemonResponse::MacOSAppLevels(levels)),
                Err(e) => Ok(DaemonResponse::Error(e.to_string())),
            }
        }

        DaemonRequest::Command(serial, command) => {
            let (tx, rx) = oneshot::channel();
            usb_tx
                .send(DeviceCommand::RunDeviceCommand(serial, command, tx))
                .await
                .map_err(|e| anyhow!(e.to_string()))
                .context("Could not communicate with the GoXLR device")?;
            await_response(rx, DEVICE_COMMAND_TIMEOUT)
                .await
                .context("Could not execute the command on the GoXLR device")??;
            Ok(DaemonResponse::Ok)
        }

        DaemonRequest::RunFirmwareUpdate(serial, path, force) => {
            let (tx, rx) = oneshot::channel();
            usb_tx
                .send(DeviceCommand::RunFirmwareUpdate(serial, path, force, tx))
                .await
                .map_err(anyhow::Error::msg)?;
            await_response(rx, RESPONSE_TIMEOUT)
                .await
                .context("Could not execute the command on the GoXLR device")??;
            Ok(DaemonResponse::Ok)
        }

        DaemonRequest::ContinueFirmwareUpdate(serial) => {
            let (tx, rx) = oneshot::channel();
            usb_tx
                .send(DeviceCommand::ContinueFirmwareUpdate(serial, tx))
                .await
                .map_err(|e| anyhow!(e.to_string()))
                .context("Could not communicate with the GoXLR device")?;
            await_response(rx, RESPONSE_TIMEOUT)
                .await
                .context("Could not execute the command on the GoXLR device")??;
            Ok(DaemonResponse::Ok)
        }

        DaemonRequest::ClearFirmwareState(serial) => {
            let (tx, rx) = oneshot::channel();
            usb_tx
                .send(DeviceCommand::ClearFirmwareState(serial, tx))
                .await
                .map_err(anyhow::Error::msg)?;
            await_response(rx, RESPONSE_TIMEOUT)
                .await
                .context("Could not execute the command on the GoXLR device")??;
            Ok(DaemonResponse::Ok)
        }
    }
}
