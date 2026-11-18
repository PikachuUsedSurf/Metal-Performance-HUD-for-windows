//! Everything that talks to the iPhone or iPad.
//!
//! How the connection works, in short:
//! * On Windows, iTunes / the Apple Devices app installs "Apple Mobile Device Service".
//!   It listens on 127.0.0.1:27015 and forwards our messages over the USB cable.
//! * On the device, a service called "lockdown" is the front door. After the device
//!   trusts this PC, lockdown lets us open other services (developer image mounter,
//!   app list, and so on).
//! * On iOS 17.4 and newer, the developer services hide behind an encrypted tunnel.
//!   The `idevice` library builds that tunnel inside this app, so no admin rights or
//!   extra drivers are needed.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use idevice::{
    IdeviceError, IdeviceService, RsdService,
    amfi::AmfiClient,
    core_device::AppServiceClient,
    core_device_proxy::CoreDeviceProxy,
    cryptexd::{Cryptex1Assets, install_ddi, installed_ddi},
    dvt::{process_control::ProcessControlClient, remote_server::RemoteServerClient},
    installation_proxy::InstallationProxyClient,
    lockdown::LockdownClient,
    mobile_image_mounter::ImageMounter,
    provider::{IdeviceProvider, UsbmuxdProvider},
    rsd::RsdHandshake,
    tcp::handle::AdapterHandle,
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice},
};

use crate::{ddi, worker::Reporter};

/// The name this app uses when it introduces itself to the device.
const LABEL: &str = "ios-hud-helper";

/// The environment variable that tells Apple's Metal graphics framework to draw the HUD.
const HUD_ENV_VAR: &str = "MTL_HUD_ENABLED";

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub udid: String,
    pub name: String,
    pub model: String,
    pub ios: String,
    pub major: u32,
    pub minor: u32,
    pub over_wifi: bool,
}

impl DeviceInfo {
    pub fn label(&self) -> String {
        let via = if self.over_wifi { " · Wi-Fi" } else { "" };
        format!("{} ({}, iOS {}{via})", self.name, self.model, self.ios)
    }
}

#[derive(Clone, Debug, Default)]
pub struct DeviceStatus {
    /// `None` means "couldn't tell".
    pub developer_mode: Option<bool>,
    pub menu_unlocked: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct AppInfo {
    pub name: String,
    pub bundle_id: String,
    pub version: String,
}

// ---------------------------------------------------------------------------
// Finding devices
// ---------------------------------------------------------------------------

async fn usbmuxd() -> Result<UsbmuxdConnection> {
    UsbmuxdConnection::default().await.map_err(|e| {
        anyhow!(
            "Couldn't reach Apple's device service ({e}).\n\
             Install iTunes or the \"Apple Devices\" app from Apple, then unplug and replug your device."
        )
    })
}

/// Lists every iPhone/iPad the PC can see.
pub async fn list_devices(rep: &Reporter) -> Result<Vec<DeviceInfo>> {
    let mut mux = usbmuxd().await?;
    let mut devices = mux.get_devices().await?;
    // The same device can show up twice (USB and Wi-Fi). Put USB first and skip repeats.
    devices.sort_by_key(|d| d.connection_type != Connection::Usb);

    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for dev in devices {
        if !seen.insert(dev.udid.clone()) {
            continue;
        }
        match describe(&dev, rep).await {
            Ok(info) => found.push(info),
            Err(e) => rep.error(format!("Found a device but couldn't talk to it: {e:#}")),
        }
    }
    Ok(found)
}

async fn describe(dev: &UsbmuxdDevice, rep: &Reporter) -> Result<DeviceInfo> {
    let provider = dev.to_provider(UsbmuxdAddr::default(), LABEL);
    ensure_paired(dev, &provider, rep).await?;
    let mut lockdown = session(&provider).await?;

    let ios = get_string(&mut lockdown, "ProductVersion").await.unwrap_or_default();
    let mut parts = ios.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);

    Ok(DeviceInfo {
        udid: dev.udid.clone(),
        name: get_string(&mut lockdown, "DeviceName").await.unwrap_or_else(|| "iOS device".into()),
        model: get_string(&mut lockdown, "ProductType").await.unwrap_or_default(),
        ios,
        major,
        minor,
        over_wifi: dev.connection_type != Connection::Usb,
    })
}

/// Makes sure the device trusts this PC. If it doesn't yet, the device shows the
/// "Trust This Computer?" prompt and we wait for the user to tap Trust.
async fn ensure_paired(dev: &UsbmuxdDevice, provider: &UsbmuxdProvider, rep: &Reporter) -> Result<()> {
    if provider.get_pairing_file().await.is_ok() {
        return Ok(());
    }
    rep.log("This PC isn't trusted by your device yet. Unlock it, tap \"Trust\" and enter your passcode…");

    let mut mux = usbmuxd().await?;
    let buid = mux.get_buid().await?;
    let mut lockdown = LockdownClient::connect(provider).await?;
    let host_id = uuid::Uuid::new_v4().to_string().to_uppercase();

    let pairing = tokio::time::timeout(
        Duration::from_secs(120),
        lockdown.pair(host_id, buid, Some("iOS HUD Helper")),
    )
    .await
    .map_err(|_| anyhow!("Timed out waiting for you to tap \"Trust\" on the device."))?;

    let mut pairing = match pairing {
        Ok(p) => p,
        Err(IdeviceError::PasswordProtected) => bail!("Your device is locked. Unlock it and press Refresh."),
        Err(IdeviceError::UserDeniedPairing) => {
            bail!("\"Don't Trust\" was tapped. Unplug and replug the device, then tap \"Trust\".")
        }
        Err(e) => return Err(e.into()),
    };
    pairing.udid = Some(dev.udid.clone());
    mux.save_pair_record(&dev.udid, pairing.serialize()?).await?;
    rep.success("Your device now trusts this PC.");
    Ok(())
}

async fn session(provider: &UsbmuxdProvider) -> Result<LockdownClient> {
    let mut lockdown = LockdownClient::connect(provider).await?;
    lockdown.start_session(&provider.get_pairing_file().await?).await?;
    Ok(lockdown)
}

async fn get_string(lockdown: &mut LockdownClient, key: &str) -> Option<String> {
    let value = lockdown.get_value(Some(key), None).await.ok()?;
    value.as_string().map(str::to_owned)
}

/// Opens a fresh connection to a device we listed earlier.
async fn provider_for(udid: &str) -> Result<UsbmuxdProvider> {
    let mut mux = usbmuxd().await?;
    let dev = mux
        .get_devices()
        .await?
        .into_iter()
        .filter(|d| d.udid == udid)
        .min_by_key(|d| d.connection_type != Connection::Usb)
        .ok_or_else(|| anyhow!("The device isn't connected any more. Plug it back in and press Refresh."))?;
    Ok(dev.to_provider(UsbmuxdAddr::default(), LABEL))
}

/// Builds the encrypted tunnel that iOS 17.4+ requires for developer services.
async fn open_tunnel(provider: &UsbmuxdProvider, info: &DeviceInfo) -> Result<(AdapterHandle, RsdHandshake)> {
    let proxy = CoreDeviceProxy::connect(provider).await.map_err(|e| {
        if info.major == 17 && info.minor < 4 {
            anyhow!("iOS {} needs a kind of connection this app can't make. Please update the device to iOS 17.4 or newer.", info.ios)
        } else {
            anyhow!("Couldn't open the developer tunnel: {e}")
        }
    })?;
    let rsd_port = proxy.tunnel_info().server_rsd_port;
    let mut adapter = proxy.create_software_tunnel()?.to_async_handle();
    let stream = adapter.connect(rsd_port).await?;
    let handshake = RsdHandshake::new(stream).await?;
    Ok((adapter, handshake))
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

pub async fn status(info: &DeviceInfo) -> Result<DeviceStatus> {
    let provider = provider_for(&info.udid).await?;
    Ok(DeviceStatus {
        developer_mode: developer_mode(&provider, info).await,
        menu_unlocked: menu_unlocked(&provider, info).await.ok(),
    })
}

async fn developer_mode(provider: &UsbmuxdProvider, info: &DeviceInfo) -> Option<bool> {
    if info.major < 16 {
        return Some(true); // Developer Mode didn't exist before iOS 16, so nothing to turn on.
    }
    if let Ok(mut lockdown) = session(provider).await
        && let Ok(value) = lockdown
            .get_value(Some("DeveloperModeStatus"), Some("com.apple.security.mac.amfi"))
            .await
        && let Some(on) = value.as_boolean()
    {
        return Some(on);
    }
    let mut mounter = ImageMounter::connect(provider).await.ok()?;
    mounter.query_developer_mode_status().await.ok()
}

/// The Developer menu shows up while a Developer Disk Image is mounted.
async fn menu_unlocked(provider: &UsbmuxdProvider, info: &DeviceInfo) -> Result<bool> {
    let mut mounter = ImageMounter::connect(provider).await?;
    if !mounter.copy_devices().await?.is_empty() {
        return Ok(true);
    }
    // Newer iOS can also install the image as a "cryptex", which is listed elsewhere.
    if info.major >= 26
        && let Ok((mut adapter, mut handshake)) = open_tunnel(provider, info).await
        && let Ok(Some(_)) = installed_ddi(&mut adapter, &mut handshake).await
    {
        return Ok(true);
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Step 1: Developer Mode
// ---------------------------------------------------------------------------

pub async fn reveal_developer_mode(info: &DeviceInfo, rep: &Reporter) -> Result<()> {
    if info.major < 16 {
        rep.success("Your iOS version doesn't use Developer Mode, so you can skip this step.");
        return Ok(());
    }
    let provider = provider_for(&info.udid).await?;
    let mut amfi = AmfiClient::connect(&provider).await?;
    amfi.reveal_developer_mode_option_in_ui()
        .await
        .context("The device refused to show the Developer Mode switch")?;
    rep.success(
        "Done! On your device open Settings → Privacy & Security → Developer Mode, turn it on, \
         and let it restart. After it restarts, unlock it and tap \"Turn On\".",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 2: unlock the Developer menu (mount the Developer Disk Image)
// ---------------------------------------------------------------------------

pub async fn unlock_developer_menu(info: &DeviceInfo, rep: &Reporter) -> Result<()> {
    let provider = provider_for(&info.udid).await?;

    if developer_mode(&provider, info).await == Some(false) {
        bail!("Developer Mode is off. Do step 1 first, then try again.");
    }
    if menu_unlocked(&provider, info).await.unwrap_or(false) {
        rep.success("The Developer menu is already unlocked.");
        return Ok(());
    }

    if info.major < 17 {
        let image = ddi::classic(info.major, info.minor, rep).await?;
        rep.log("Sending the developer image to your device…");
        let mut mounter = ImageMounter::connect(&provider).await?;
        mounter.mount_developer(&image.image, image.signature).await.map_err(mount_error)?;
    } else {
        // iOS 27 introduced a new way to install the image. Try it first, and fall back if it fails.
        let mut done = false;
        if info.major >= 27 {
            match install_cryptex(&provider, info, rep).await {
                Ok(()) => done = true,
                Err(e) => rep.log(format!("The newer install method didn't work ({e:#}). Trying the standard one…")),
            }
        }
        if !done {
            mount_personalized(&provider, rep).await?;
        }
    }

    rep.success(
        "Developer menu unlocked! On your device open Settings → Developer → Graphics HUD \
         and turn on \"Show Graphics HUD\". (After the device restarts, press this button again.)",
    );
    Ok(())
}

async fn mount_personalized(provider: &UsbmuxdProvider, rep: &Reporter) -> Result<()> {
    let image = ddi::personalized(rep).await?;

    let mut lockdown = session(provider).await?;
    let chip_id = lockdown
        .get_value(Some("UniqueChipID"), None)
        .await?
        .as_unsigned_integer()
        .ok_or_else(|| anyhow!("The device didn't report its chip ID"))?;

    rep.log("Asking Apple to sign the developer image for your device, then sending it…");
    let mut mounter = ImageMounter::connect(provider).await?;
    mounter
        .mount_personalized_with_callback(
            provider,
            image.image,
            image.trust_cache,
            &image.build_manifest,
            None,
            chip_id,
            |((sent, total), rep): ((usize, usize), Reporter)| async move {
                rep.progress("Sending image to device", sent as f32 / total.max(1) as f32);
            },
            rep.clone(),
        )
        .await
        .map_err(mount_error)?;
    rep.clear_progress();
    Ok(())
}

async fn install_cryptex(provider: &UsbmuxdProvider, info: &DeviceInfo, rep: &Reporter) -> Result<()> {
    let image = ddi::cryptex(rep).await?;
    let manifest: plist::Dictionary = plist::from_bytes(&image.build_manifest)?;
    let identity = idevice::tss::select_cryptex_build_identity(&manifest)?.clone();
    let assets = Cryptex1Assets::from_parts(image.image, image.trust_cache, image.info, image.root_hash, identity);

    rep.log("Asking Apple to sign the developer image for your device, then installing it…");
    let (mut adapter, mut handshake) = open_tunnel(provider, info).await?;
    install_ddi(&mut adapter, &mut handshake, &assets).await?;
    Ok(())
}

fn mount_error(e: IdeviceError) -> anyhow::Error {
    match e {
        IdeviceError::DeveloperModeNotEnabled => anyhow!("Developer Mode is off. Do step 1 first."),
        IdeviceError::DeviceLocked => anyhow!("Your device is locked. Unlock it and try again."),
        other => anyhow!("Couldn't unlock the Developer menu: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Step 3: launch a game with the HUD
// ---------------------------------------------------------------------------

pub async fn list_apps(info: &DeviceInfo) -> Result<Vec<AppInfo>> {
    let provider = provider_for(&info.udid).await?;
    let mut installer = InstallationProxyClient::connect(&provider).await?;
    // "User" = apps you installed yourself (App Store, TestFlight…), not Apple's built-in apps.
    let apps = installer.get_apps(Some("User"), None).await?;

    let mut list: Vec<AppInfo> = apps
        .into_iter()
        .map(|(bundle_id, value)| {
            let dict = value.as_dictionary();
            let field = |key: &str| {
                dict.and_then(|d| d.get(key))
                    .and_then(|v| v.as_string())
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            };
            AppInfo {
                name: field("CFBundleDisplayName")
                    .or_else(|| field("CFBundleName"))
                    .unwrap_or_else(|| bundle_id.clone()),
                version: field("CFBundleShortVersionString").unwrap_or_default(),
                bundle_id,
            }
        })
        .collect();
    list.sort_by_key(|a| a.name.to_lowercase());
    Ok(list)
}

pub async fn launch_with_hud(info: &DeviceInfo, app: &AppInfo, close_first: bool, rep: &Reporter) -> Result<()> {
    let provider = provider_for(&info.udid).await?;

    // The developer tools that launch apps only exist while the Developer menu is unlocked.
    if !menu_unlocked(&provider, info).await.unwrap_or(false) {
        rep.log("The Developer menu is locked. Unlocking it first…");
        unlock_developer_menu(info, rep).await?;
    }

    let mut env = plist::Dictionary::new();
    env.insert(HUD_ENV_VAR.into(), plist::Value::String("1".into()));

    rep.log(format!("Launching {} with the HUD…", app.name));
    if info.major >= 17 {
        let (mut adapter, mut handshake) = open_tunnel(&provider, info).await?;
        // Same method Xcode's `devicectl device process launch` uses.
        let app_service = async {
            let mut client: AppServiceClient<Box<dyn idevice::ReadWrite>> =
                AppServiceClient::connect_rsd(&mut adapter, &mut handshake).await?;
            client
                .launch_application(&app.bundle_id, &[], close_first, false, Some(env.clone()), None, None)
                .await?;
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = app_service {
            rep.log(format!("First launch method didn't work ({e:#}). Trying the Instruments method…"));
            let mut remote: RemoteServerClient<Box<dyn idevice::ReadWrite>> =
                RemoteServerClient::connect_rsd(&mut adapter, &mut handshake).await?;
            remote.read_message(0).await?;
            dvt_launch(&mut remote, app, env, close_first).await?;
        }
    } else {
        let mut remote: RemoteServerClient<Box<dyn idevice::ReadWrite>> = RemoteServerClient::connect(&provider)
            .await
            .context("Couldn't reach the developer tools. Try step 2 again.")?;
        dvt_launch(&mut remote, app, env, close_first).await?;
    }

    rep.success(format!(
        "{} is starting on your device. If no HUD appears, make sure \"Show Graphics HUD\" is on in \
         Settings → Developer → Graphics HUD. Some games block it.",
        app.name
    ));
    Ok(())
}

/// Launches through Instruments' "process control" service.
async fn dvt_launch<R: idevice::ReadWrite>(
    remote: &mut RemoteServerClient<R>,
    app: &AppInfo,
    env: plist::Dictionary,
    close_first: bool,
) -> Result<()> {
    let mut process_control = ProcessControlClient::new(remote).await?;
    process_control
        .launch_app(&app.bundle_id, Some(env), None, false, close_first)
        .await
        .with_context(|| format!("The device wouldn't launch {}", app.name))?;
    Ok(())
}
