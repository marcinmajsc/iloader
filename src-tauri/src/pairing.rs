use std::collections::HashMap;

// used https://github.com/jkcoxson/idevice_pair/ as a guide
use idevice::{
    IdeviceError, IdeviceService, RemoteXpcClient,
    core_device_proxy::CoreDeviceProxy,
    house_arrest::HouseArrestClient,
    installation_proxy::InstallationProxyClient,
    lockdown::LockdownClient,
    pairing_file::PairingFile,
    provider::IdeviceProvider,
    remote_pairing::{RemotePairingClient, RpPairingFile},
    rsd::RsdHandshake,
    usbmuxd::UsbmuxdConnection,
};
use serde::Serialize;
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;
use tracing::info;

use crate::device::{DeviceInfo, DeviceInfoMutex, get_provider, get_provider_from_connection};

const PAIRING_APPS: &[(&str, &str, bool, bool)] = &[
    (
        "SideStore",
        "ALTPairingFile.mobiledevicepairing",
        true,
        false,
    ),
    (
        "LiveContainer",
        "SideStore/Documents/ALTPairingFile.mobiledevicepairing",
        true,
        false,
    ),
    ("Feather", "pairingFile.plist", true, false),
    ("StikDebug", "pairingFile.plist", true, false),
    ("StikDebug (Sideloaded)", "pairingFile.plist", false, true),
    ("StikTest", "stiktest_pairing.plist", true, false),
    ("Protokolle", "pairingFile.plist", true, false),
    ("Antrag", "pairingFile.plist", true, false),
    ("SparseBox", "pairingFile.plist", true, false),
    ("StikStore", "pairingFile.plist", true, false),
    ("ByeTunes", "pairing file/pairingFile.plist", true, false),
];

pub async fn pairing_file(
    device: DeviceInfo,
    usbmuxd: &mut UsbmuxdConnection,
) -> Result<PairingFile, String> {
    let provider = get_provider(&device).await?;

    let mut pairing_file = usbmuxd.get_pair_record(&provider.udid).await.map_err(|e| {
        format!(
            "Failed to get pairing record for device {}: {}",
            device.name, e
        )
    })?;

    pairing_file.udid = Some(provider.udid.clone());

    let mut lc = LockdownClient::connect(&provider)
        .await
        .map_err(|e| format!("Failed to connect to lockdown: {}", e))?;

    lc.start_session(&pairing_file)
        .await
        .map_err(|e| format!("Failed to start lockdown session: {}", e))?;

    lc.set_value(
        "EnableWifiDebugging",
        true.into(),
        Some("com.apple.mobile.wireless_lockdown"),
    )
    .await
    .map_err(|e| format!("Failed to enable wifi debugging: {}", e))?;

    Ok(pairing_file)
}

async fn generate_rppairing_file(
    provider: &dyn IdeviceProvider,
    hostname: &str,
) -> Result<RpPairingFile, IdeviceError> {
    info!("Connecting to CoreDeviceProxy...");
    let proxy = CoreDeviceProxy::connect(provider).await?;
    let rsd_port = proxy.tunnel_info().server_rsd_port;
    info!("CDTunnel established, RSD port {rsd_port}");

    info!("Starting TCP stack...");
    let adapter = proxy.create_software_tunnel()?;
    let mut adapter = adapter.to_async_handle();

    info!("Performing RSD handshake...");
    let rsd_stream = adapter.connect(rsd_port).await?;
    let handshake = RsdHandshake::new(rsd_stream).await?;
    info!("RSD: {} services", handshake.services.len());
    let tunnel_service = handshake
        .services
        .get("com.apple.internal.dt.coredevice.untrusted.tunnelservice")
        .ok_or_else(|| IdeviceError::InternalError("Untrusted tunnel service not found".into()))?;

    info!("Connecting to untrusted tunnel service...");
    let tunnel_service_stream = adapter.connect(tunnel_service.port).await?;
    let mut remote_xpc = RemoteXpcClient::new(tunnel_service_stream).await?;
    remote_xpc.do_handshake().await?;
    let _ = remote_xpc.recv_root().await;

    info!("Starting RPPairing...");
    info!("(You may need to tap Trust on the device)");
    let mut pairing_file = RpPairingFile::generate(hostname);
    let mut pairing_client = RemotePairingClient::new(remote_xpc, hostname, &mut pairing_file);
    pairing_client
        .connect(async |_| "000000".to_string(), ())
        .await?;

    Ok(pairing_file)
}

pub async fn place_pairing(
    pairing: Vec<u8>,
    provider: &dyn IdeviceProvider,
    bundle_id: String,
    path: String,
) -> Result<(), String> {
    let house_arrest_client = HouseArrestClient::connect(provider)
        .await
        .map_err(|e| format!("Failed to connect to house arrest: {}", e))?;

    let mut afc_client = house_arrest_client
        .vend_documents(bundle_id)
        .await
        .map_err(|e| format!("Failed to vend documents: {}", e))?;

    afc_client
        .mk_dir(format!(
            "/Documents/{}",
            path.rsplit_once('/').map(|x| x.0).unwrap_or("")
        ))
        .await
        .map_err(|e| format!("Failed to create Documents directory: {}", e))?;

    let mut file = afc_client
        .open(
            format!("/Documents/{}", path),
            idevice::afc::opcode::AfcFopenMode::Wr,
        )
        .await
        .map_err(|e| format!("Failed to open file on device: {}", e))?;

    file.write_entire(&pairing)
        .await
        .map_err(|e| format!("Failed to write pairing file: {}", e))?;
    file.close()
        .await
        .map_err(|e| format!("Failed to close file: {}", e))?;

    Ok(())
}

#[tauri::command]
pub async fn place_lockdown_pairing(
    device_state: State<'_, DeviceInfoMutex>,
    bundle_id: String,
    path: String,
) -> Result<(), String> {
    let device = {
        let device_guard = device_state.lock().unwrap();
        match &*device_guard {
            Some(d) => d.clone(),
            None => return Err("No device selected".to_string()),
        }
    };

    let mut usbmuxd = UsbmuxdConnection::default()
        .await
        .map_err(|e| format!("Failed to connect to usbmuxd: {}", e))?;

    let provider = get_provider_from_connection(&device, &mut usbmuxd).await?;

    let pairing_file = pairing_file(device, &mut usbmuxd).await?;

    place_pairing(
        pairing_file
            .serialize()
            .map_err(|e| format!("Failed to serialize pairing file: {}", e))?,
        &provider,
        bundle_id,
        path,
    )
    .await
}

#[tauri::command]
pub async fn place_remote_pairing(
    device_state: State<'_, DeviceInfoMutex>,
    bundle_id: String,
    path: String,
) -> Result<(), String> {
    let device = {
        let device_guard = device_state.lock().unwrap();
        match &*device_guard {
            Some(d) => d.clone(),
            None => return Err("No device selected".to_string()),
        }
    };

    let mut usbmuxd = UsbmuxdConnection::default()
        .await
        .map_err(|e| format!("Failed to connect to usbmuxd: {}", e))?;

    let provider = get_provider_from_connection(&device, &mut usbmuxd).await?;

    let pairing_file = generate_rppairing_file(&provider, "iloader")
        .await
        .map_err(|e| format!("Failed to generate remote pairing file: {}", e))?;

    place_pairing(pairing_file.to_bytes(), &provider, bundle_id, path).await
}

// prompt for a location to save the pairing file, then export it there. This is for advanced users who want to use the pairing file with other tools, or just want a backup of it. Normal users should use the "Place" button next to the app they want to pair with instead, which will transfer the pairing file automatically.
#[tauri::command]
pub async fn export_pairing_cmd(
    device_state: State<'_, DeviceInfoMutex>,
    app: AppHandle,
) -> Result<(), String> {
    let device = {
        let device_guard = device_state.lock().unwrap();
        match &*device_guard {
            Some(d) => d.clone(),
            None => return Err("No device selected".to_string()),
        }
    };

    let pairing_file = {
        let mut usbmuxd = UsbmuxdConnection::default()
            .await
            .map_err(|e| format!("Failed to connect to usbmuxd: {}", e))?;

        pairing_file(device, &mut usbmuxd).await?
    };

    let save_path = app
        .dialog()
        .file()
        .add_filter("Pairing File", &["plist", "mobiledevicepairing"])
        .set_file_name("pairingFile.plist")
        .set_title("Export Pairing File")
        .blocking_save_file();

    if let Some(save_path) = save_path
        && let Some(save_path) = save_path.as_path()
    {
        tokio::fs::write(
            save_path,
            &pairing_file
                .serialize()
                .map_err(|e| format!("Failed to serialize pairing file: {}", e))?,
        )
        .await
        .map_err(|e| format!("Failed to write pairing file: {}", e))?;

        Ok(())
    } else {
        Err("Save cancelled".to_string())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingAppInfo {
    pub name: String,
    pub bundle_id: String,
    pub path: String,
    pub lockdown: bool,
    pub remote_pairing: bool,
}

#[tauri::command]
pub async fn installed_pairing_apps(
    device_state: State<'_, DeviceInfoMutex>,
) -> Result<Vec<PairingAppInfo>, String> {
    let device = {
        let device_guard = device_state.lock().unwrap();
        match &*device_guard {
            Some(d) => d.clone(),
            None => return Err("No device selected".to_string()),
        }
    };
    let provider = get_provider(&device).await?;
    let mut installation_proxy = InstallationProxyClient::connect(&provider)
        .await
        .map_err(|e| format!("Failed to connect to installation proxy: {}", e))?;

    let installed_apps = installation_proxy
        .get_apps(Some("User"), None)
        .await
        .map_err(|e| format!("Failed to get installed apps: {}", e))?;

    let mut installed = HashMap::new();
    for (bundle_id, app) in installed_apps {
        let n = app
            .as_dictionary()
            .and_then(|x| x.get("CFBundleDisplayName").and_then(|x| x.as_string()))
            .ok_or("Failed to parse installed apps".to_string())?;

        if PAIRING_APPS.iter().any(|(name, _, _, _)| name == &n) {
            if bundle_id.contains("com.stik.stikdebug") {
                installed.insert(format!("{} (Sideloaded)", n), bundle_id);
            } else {
                installed.insert(n.to_string(), bundle_id);
            }
        }
    }

    let mut result = Vec::new();
    for (name, path, lockdown, remote_pairing) in PAIRING_APPS {
        if let Some(bundle_id) = installed.get(*name) {
            result.push(PairingAppInfo {
                name: name.to_string(),
                bundle_id: bundle_id.to_string(),
                path: path.to_string(),
                lockdown: *lockdown,
                remote_pairing: *remote_pairing,
            });
        }
    }
    Ok(result)
}

pub async fn get_sidestore_info(
    device: DeviceInfo,
    live_container: bool,
) -> Result<Option<PairingAppInfo>, String> {
    let provider = get_provider(&device).await?;
    let mut installation_proxy = InstallationProxyClient::connect(&provider)
        .await
        .map_err(|e| format!("Failed to connect to installation proxy: {}", e))?;

    let installed_apps = installation_proxy
        .get_apps(Some("User"), None)
        .await
        .map_err(|e| format!("Failed to get installed apps: {}", e))?;

    for (bundle_id, app) in installed_apps {
        let n = app
            .as_dictionary()
            .and_then(|x| x.get("CFBundleDisplayName").and_then(|x| x.as_string()))
            .ok_or("Failed to parse installed apps".to_string())?;

        if n == "SideStore" || (live_container && n == "LiveContainer") {
            return Ok(Some(PairingAppInfo {
                name: n.to_string(),
                bundle_id: bundle_id.to_string(),
                path: PAIRING_APPS
                    .iter()
                    .find(|(name, _, _, _)| name == &n)
                    .map(|(_, path, _, _)| path.to_string())
                    .unwrap_or_default(),
                lockdown: true,
                remote_pairing: false,
            }));
        }
    }

    Ok(None)
}
