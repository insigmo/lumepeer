//! The session host keeps this machine up to date (ADR 0169 §8).
//!
//! Once the client runs unelevated it cannot run a `perMachine` installer
//! without a UAC prompt, and an update that waits for somebody to answer one
//! never reaches a machine nobody sits at. So the `LocalSystem` session host
//! does what `tauri-plugin-updater` does for the elevated client today, with
//! the same inputs and none of the window:
//!
//! - **The same manifest**: the stable channel's `latest.json` the client's
//!   own check reads by default (ADR 0042), entry `windows-<arch>-nsis`.
//!   The person's own config file is theirs to write and is not read here,
//!   as `system_host.rs` reads none of it either: a `LocalSystem` installer
//!   does not take its source from a file any program of theirs can edit.
//! - **The same key**: the updater public key of `tauri.conf.json`, compiled
//!   into this binary ([`public_key`]). A file whose minisign signature does
//!   not verify against it is deleted and never started, whatever the
//!   manifest said; a signature that names a version (the Tauri CLI writes
//!   `version:` into the trusted comment) must name the one announced, as the
//!   plugin checks it.
//! - **A place nobody else can write**: `%ProgramData%\Lumepeer\updates`, `SY`
//!   and `BA` only, secured by `program_data` with its reparse-point and
//!   foreign-owner checks. The file name is a constant: nothing about the
//!   path comes from the manifest, and nothing at all from the client.
//! - **Silent, and on its own**: the NSIS installer with `/S /UPDATE`, started
//!   detached, so it outlives this process and the service it stops. Every
//!   guest is told first that this host is restarting (ADR 0148).
//!
//! When: by schedule once the host has no session and the console has seen
//! no input for [`UPDATE_INSTALL_IDLE_SECS`] — nobody is at the machine — or
//! at once when a client asks ([`Updates::ask_to_install`]). That request is
//! not a widening one: the caller names no package, the host installs only
//! what it found and verified itself (ADR 0169 §4, §8).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lumepeer_core::constants::{
    UPDATE_CHECK_INTERVAL_SECS, UPDATE_DOWNLOAD_TIMEOUT_SECS, UPDATE_FIRST_CHECK_DELAY_SECS,
    UPDATE_INSTALL_IDLE_SECS, UPDATE_INSTALLER_MAX_BYTES, UPDATE_MANIFEST_MAX_BYTES,
    UPDATE_MANIFEST_TIMEOUT_SECS,
};
use semver::Version;
use serde::Deserialize;

use crate::host_panel::{HostUpdate, Served};

/// `tauri.conf.json`, compiled in: the updater's public key is read from the
/// very file `tauri-plugin-updater` takes its own from, so the two cannot
/// drift apart.
const TAURI_CONF: &str = include_str!("../tauri.conf.json");

/// The installer's file name in the updates directory. A constant: nothing
/// about where the file lands comes from the manifest.
const INSTALLER_FILE: &str = "lumepeer-update-setup.exe";

/// What the installer is started with: NSIS's silent switch, and the
/// update-mode flag `tauri-plugin-updater` passes too. No `/R`: the installer
/// runs as `LocalSystem`, and what it would start is the person's client —
/// the next session host does that, with the person's token
/// ([`relaunch_client_if_asked`]).
const INSTALLER_ARGS: [&str; 2] = ["/S", "/UPDATE"];

/// A file in the updates directory that tells the next session host a client
/// was attached when this one started the installer, which closes it: start
/// it again. Its presence is the whole message.
const RELAUNCH_CLIENT_FILE: &str = "relaunch-client";

/// `DETACHED_PROCESS`: the installer gets no console of this process's.
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// `CREATE_NEW_PROCESS_GROUP`: nothing this process's group receives reaches
/// the installer.
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// How often the loop looks at a client's request and at whether nobody is at
/// the machine.
const TICK: Duration = Duration::from_secs(1);

/// Why an update was not checked, fetched or started. Each is logged; none
/// reaches the client beyond the state it leaves behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The manifest is not one: not JSON of the expected shape, no version.
    Manifest,
    /// The manifest has no entry for this platform's NSIS installer.
    NoEntry,
    /// The manifest points somewhere that is not `https`.
    NotHttps,
    /// The manifest or the installer is larger than the bound.
    TooLarge,
    /// The channel could not be reached, or answered with an error.
    Network,
    /// The signature is not minisign, or does not verify against the key.
    Signature,
    /// The signature verified and names another version than announced.
    SignedVersion,
    /// The updates directory could not be secured, written or read.
    Disk,
    /// The installer could not be started.
    Launch,
}

/// A release newer than this one, as the manifest offers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// The announced version.
    pub version: Version,
    /// Its release notes, or empty.
    pub notes: String,
    /// Where the NSIS installer is.
    pub url: String,
    /// Its minisign signature, base64 of the `.sig` file, as the manifest
    /// carries it.
    pub signature: String,
}

/// `latest.json` as `.github/workflows/release.yml` writes it.
#[derive(Debug, Deserialize)]
struct Manifest {
    version: String,
    #[serde(default)]
    notes: Option<String>,
    platforms: HashMap<String, Entry>,
}

/// One platform's line of the manifest.
#[derive(Debug, Deserialize)]
struct Entry {
    url: String,
    signature: String,
}

/// The updater public key this binary was built with: base64 of the
/// minisign public key file, as `tauri.conf.json`'s `plugins.updater.pubkey`
/// holds it.
#[must_use]
pub fn public_key() -> Option<String> {
    let conf: serde_json::Value = serde_json::from_str(TAURI_CONF).ok()?;
    conf.pointer("/plugins/updater/pubkey")?
        .as_str()
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
}

/// The manifest key of this platform's NSIS installer, as `release.yml`
/// spells it.
#[must_use]
pub const fn platform_key() -> Option<&'static str> {
    if cfg!(target_arch = "x86_64") {
        Some("windows-x86_64-nsis")
    } else if cfg!(target_arch = "aarch64") {
        Some("windows-aarch64-nsis")
    } else {
        None
    }
}

/// The release `manifest` offers to a machine running `current`, if it offers
/// a newer one for `key`.
///
/// # Errors
/// [`Refusal::Manifest`], [`Refusal::NoEntry`] or [`Refusal::NotHttps`] for a
/// manifest that names no usable installer for a newer release.
pub fn offer_in(manifest: &[u8], current: &Version, key: &str) -> Result<Option<Offer>, Refusal> {
    let manifest: Manifest = serde_json::from_slice(manifest).map_err(|_| Refusal::Manifest)?;
    let version =
        Version::parse(manifest.version.trim_start_matches('v')).map_err(|_| Refusal::Manifest)?;
    if version <= *current {
        return Ok(None);
    }
    let entry = manifest.platforms.get(key).ok_or(Refusal::NoEntry)?;
    if !entry.url.starts_with("https://") {
        return Err(Refusal::NotHttps);
    }
    if entry.signature.is_empty() {
        return Err(Refusal::Signature);
    }
    Ok(Some(Offer {
        version,
        notes: manifest.notes.unwrap_or_default(),
        url: entry.url.clone(),
        signature: entry.signature.clone(),
    }))
}

/// Whether `bytes` carry `signature` by `public_key` — both base64 of the
/// minisign file, as the manifest and `tauri.conf.json` hold them — for
/// `announced`.
///
/// What `tauri-plugin-updater` checks, the same way: the signature verifies
/// (legacy and pre-hashed minisign both), and only then is its trusted
/// comment read; a `version:` there must be `announced`.
///
/// # Errors
/// [`Refusal::Signature`] or [`Refusal::SignedVersion`].
pub fn verify(
    bytes: &[u8],
    signature: &str,
    public_key: &str,
    announced: &Version,
) -> Result<(), Refusal> {
    let decode = |text: &str| {
        data_encoding::BASE64
            .decode(text.trim().as_bytes())
            .ok()
            .and_then(|raw| String::from_utf8(raw).ok())
    };
    let key = decode(public_key)
        .and_then(|text| minisign_verify::PublicKey::decode(&text).ok())
        .ok_or(Refusal::Signature)?;
    let signature = decode(signature)
        .and_then(|text| minisign_verify::Signature::decode(&text).ok())
        .ok_or(Refusal::Signature)?;
    key.verify(bytes, &signature, true)
        .map_err(|_| Refusal::Signature)?;
    // Only now is the trusted comment worth reading: the global signature
    // that covers it is what `verify` just checked.
    let signed = signature
        .trusted_comment()
        .split('\t')
        .find_map(|field| field.strip_prefix("version:"));
    match signed {
        None => Ok(()),
        Some(signed) => match Version::parse(signed.trim_start_matches('v')) {
            Ok(signed) if signed == *announced => Ok(()),
            _ => Err(Refusal::SignedVersion),
        },
    }
}

/// The installer in `directory`, once it has verified against `offer` and
/// `public_key`; a file that does not is deleted.
///
/// Read from where it will be started, right before it is: the directory
/// admits only `LocalSystem` and administrators, so nobody can swap it
/// between this check and the start.
///
/// # Errors
/// [`Refusal::Disk`] when it cannot be read, [`Refusal::TooLarge`], or what
/// [`verify`] says.
pub fn verified_installer(
    directory: &Path,
    offer: &Offer,
    public_key: &str,
) -> Result<PathBuf, Refusal> {
    let path = directory.join(INSTALLER_FILE);
    let checked = std::fs::metadata(&path)
        .map_err(|_| Refusal::Disk)
        .and_then(|meta| {
            if usize::try_from(meta.len()).map_or(true, |len| len > UPDATE_INSTALLER_MAX_BYTES) {
                Err(Refusal::TooLarge)
            } else {
                std::fs::read(&path).map_err(|_| Refusal::Disk)
            }
        })
        .and_then(|bytes| verify(&bytes, &offer.signature, public_key, &offer.version));
    if let Err(refusal) = checked {
        tracing::warn!(
            ?refusal,
            path = %path.display(),
            version = %offer.version,
            "the downloaded installer did not verify; deleted, not started"
        );
        let _ = std::fs::remove_file(&path);
        return Err(refusal);
    }
    Ok(path)
}

/// What the session host's update loop and its channel share.
#[derive(Debug, Default)]
pub struct Updates {
    state: Mutex<HostUpdate>,
    /// A client asked to install now.
    asked: AtomicBool,
    /// The installer is running: this host leaves.
    leaving: AtomicBool,
}

impl Updates {
    /// What the client is shown.
    #[must_use]
    pub fn state(&self) -> HostUpdate {
        self.state
            .lock()
            .map_or_else(|_| HostUpdate::Unknown, |state| state.clone())
    }

    fn set(&self, state: HostUpdate) {
        if let Ok(mut slot) = self.state.lock() {
            *slot = state;
        }
    }

    /// A client's "install now". `true` when there may be something to
    /// install: the host checks again, and installs what it finds and
    /// verifies — never anything the caller names.
    pub fn ask_to_install(&self) -> bool {
        let worth_it = !matches!(self.state(), HostUpdate::Off | HostUpdate::Current);
        if worth_it {
            self.asked.store(true, Ordering::SeqCst);
        }
        worth_it
    }

    /// Whether the installer has been started and this host should leave.
    #[must_use]
    pub fn leaving(&self) -> bool {
        self.leaving.load(Ordering::SeqCst)
    }
}

/// Everything a check and an install need, fixed for this process's life.
struct Source {
    manifest_url: String,
    key: &'static str,
    public_key: String,
    current: Version,
    client: reqwest::Client,
}

impl Source {
    /// The source, or why this host installs nothing: a build that did not
    /// come from the NSIS installer (a development build, the `.msi`) is left
    /// to update the way it was installed.
    fn new() -> Result<Self, &'static str> {
        if tauri::utils::platform::bundle_type() != Some(tauri::utils::config::BundleType::Nsis) {
            return Err("this copy was not installed by the NSIS installer");
        }
        let manifest_url = lumepeer_runtime::config::Settings::default()
            .update_manifest_url()
            .ok_or("no update channel")?;
        let public_key = public_key().ok_or("no updater public key in this build")?;
        let key = platform_key().ok_or("no installer is published for this architecture")?;
        let current =
            Version::parse(env!("CARGO_PKG_VERSION")).map_err(|_| "this build's version")?;
        // `rustls` without a provider, as `tauri-plugin-updater` builds it:
        // the process-wide one is installed once, here, the way the plugin
        // does before its first request.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .https_only(true)
            .user_agent(concat!("lumepeer-session-host/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| "no HTTP client")?;
        Ok(Self {
            manifest_url,
            key,
            public_key,
            current,
            client,
        })
    }

    /// Asks the channel; the newer release it offers, if any.
    async fn check(&self) -> Result<Option<Offer>, Refusal> {
        let manifest = self
            .fetch(
                &self.manifest_url,
                UPDATE_MANIFEST_MAX_BYTES,
                UPDATE_MANIFEST_TIMEOUT_SECS,
            )
            .await?;
        offer_in(&manifest, &self.current, self.key)
    }

    /// `url`'s body, refused past `max` bytes or `timeout_secs`.
    async fn fetch(&self, url: &str, max: usize, timeout_secs: u64) -> Result<Vec<u8>, Refusal> {
        let mut response = self
            .client
            .get(url)
            .timeout(Duration::from_secs(timeout_secs))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| {
                tracing::warn!(%error, "the update channel did not answer");
                Refusal::Network
            })?;
        if response
            .content_length()
            .is_some_and(|len| usize::try_from(len).map_or(true, |len| len > max))
        {
            return Err(Refusal::TooLarge);
        }
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > max {
                        return Err(Refusal::TooLarge);
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => return Ok(body),
                Err(error) => {
                    tracing::warn!(%error, "the download broke off");
                    return Err(Refusal::Network);
                }
            }
        }
    }
}

/// Checks the channel now and then, and installs what it finds when nobody
/// is at the machine or a client asks; returns when this process is about to
/// be replaced. Run it on the session host's runtime for as long as it lives.
pub async fn keep_up_to_date(served: Arc<Served>) {
    let updates = Arc::clone(served.updates());
    let source = match Source::new() {
        Ok(source) => source,
        Err(why) => {
            tracing::info!(why, "this host installs no updates");
            updates.set(HostUpdate::Off);
            return;
        }
    };
    let first = Instant::now() + Duration::from_secs(UPDATE_FIRST_CHECK_DELAY_SECS);
    while Instant::now() < first && !updates.asked.load(Ordering::SeqCst) {
        tokio::time::sleep(TICK).await;
    }
    // A version whose install failed: not tried again by schedule until the
    // next check, so a broken release is not downloaded every second.
    let mut failed: Option<Version> = None;
    loop {
        let offer = check(&source, &updates).await;
        let next_check = Instant::now() + Duration::from_secs(UPDATE_CHECK_INTERVAL_SECS);
        loop {
            let asked = updates.asked.swap(false, Ordering::SeqCst);
            let scheduled = offer
                .as_ref()
                .is_some_and(|offer| failed.as_ref() != Some(&offer.version))
                && nobody_here(&served).await;
            if asked || scheduled {
                match install(&source, &served, asked).await {
                    Ok(()) => return,
                    Err(version) => failed = version,
                }
                // Whatever happened, what the channel offers now is the next
                // question.
                break;
            }
            if Instant::now() >= next_check {
                failed = None;
                break;
            }
            tokio::time::sleep(TICK).await;
        }
    }
}

/// One check, its outcome put where the client reads it.
async fn check(source: &Source, updates: &Updates) -> Option<Offer> {
    match source.check().await {
        Ok(Some(offer)) => {
            tracing::info!(version = %offer.version, "an update is available");
            updates.set(HostUpdate::Available {
                version: offer.version.to_string(),
                notes: offer.notes.clone(),
            });
            Some(offer)
        }
        Ok(None) => {
            updates.set(HostUpdate::Current);
            None
        }
        Err(refusal) => {
            // The state stays what it was: a channel that did not answer this
            // time says nothing about what it offers.
            tracing::warn!(?refusal, "the update check failed");
            None
        }
    }
}

/// Whether nobody is at this machine: no guest on this host, and no input on
/// the console for [`UPDATE_INSTALL_IDLE_SECS`]. Unknown counts as somebody.
async fn nobody_here(served: &Served) -> bool {
    let guests = match served.actor() {
        Some(actor) => actor.status().await.map_or(1, |sessions| sessions.len()),
        None => 0,
    };
    guests == 0
        && lumepeer_service::console_idle::idle_ms()
            .is_some_and(|ms| ms >= UPDATE_INSTALL_IDLE_SECS.saturating_mul(1_000))
}

/// Checks again, downloads, verifies, tells every guest and starts the
/// installer; `Err` with the version that failed, if one did.
async fn install(source: &Source, served: &Served, asked: bool) -> Result<(), Option<Version>> {
    let updates = served.updates();
    let Some(offer) = check(source, updates).await else {
        return Err(None);
    };
    let back = |updates: &Updates| {
        updates.set(HostUpdate::Available {
            version: offer.version.to_string(),
            notes: offer.notes.clone(),
        });
    };
    let Some(directory) =
        tokio::task::spawn_blocking(lumepeer_service::program_data::updates_directory)
            .await
            .ok()
            .flatten()
    else {
        tracing::error!("cannot secure the updates directory; not installing");
        back(updates);
        return Err(Some(offer.version));
    };
    tracing::info!(version = %offer.version, asked, "downloading an update");
    updates.set(HostUpdate::Downloading {
        version: offer.version.to_string(),
    });
    let path = match download(source, &offer, &directory).await {
        Ok(path) => path,
        Err(refusal) => {
            tracing::warn!(?refusal, version = %offer.version, "the update was not downloaded");
            back(updates);
            return Err(Some(offer.version));
        }
    };
    // By schedule, the machine has to be as empty now as when it started.
    if !asked && !nobody_here(served).await {
        tracing::info!("somebody came back while the update downloaded; installing later");
        back(updates);
        return Err(None);
    }
    updates.set(HostUpdate::Installing {
        version: offer.version.to_string(),
    });
    let relaunch = served.anybody_attached();
    if relaunch && !mark_relaunch(&directory) {
        tracing::warn!("cannot leave word to start the client again after the update");
    }
    if let Some(actor) = served.actor()
        && let Err(error) = actor.announce_restart().await
    {
        tracing::warn!(?error, "could not tell the guests this host is restarting");
    }
    match launch(&path) {
        Ok(()) => {
            tracing::info!(version = %offer.version, "the installer is running; leaving for it");
            updates.leaving.store(true, Ordering::SeqCst);
            Ok(())
        }
        Err(refusal) => {
            tracing::error!(?refusal, "the installer did not start");
            let _ = std::fs::remove_file(directory.join(RELAUNCH_CLIENT_FILE));
            back(updates);
            Err(Some(offer.version))
        }
    }
}

/// Downloads `offer` into `directory` and verifies it there; the installer's
/// path.
async fn download(source: &Source, offer: &Offer, directory: &Path) -> Result<PathBuf, Refusal> {
    let bytes = source
        .fetch(
            &offer.url,
            UPDATE_INSTALLER_MAX_BYTES,
            UPDATE_DOWNLOAD_TIMEOUT_SECS,
        )
        .await?;
    // Checked before a byte is written, so a bad download never lands at all;
    // checked again from the disk right before it is started.
    verify(&bytes, &offer.signature, &source.public_key, &offer.version)?;
    let path = directory.join(INSTALLER_FILE);
    if !lumepeer_service::program_data::discard_untrusted_file(&path) {
        return Err(Refusal::Disk);
    }
    tokio::fs::write(&path, &bytes)
        .await
        .map_err(|_| Refusal::Disk)?;
    verified_installer(directory, offer, &source.public_key)
}

/// Starts the installer at `path`, silent and detached: it stops the service
/// and replaces this binary, so it must outlive both.
fn launch(path: &Path) -> Result<(), Refusal> {
    use std::os::windows::process::CommandExt as _;

    std::process::Command::new(path)
        .args(INSTALLER_ARGS)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|error| {
            tracing::error!(%error, "cannot start the installer");
            Refusal::Launch
        })
}

/// Leaves word for the next session host to start the person's client again.
fn mark_relaunch(directory: &Path) -> bool {
    let path = directory.join(RELAUNCH_CLIENT_FILE);
    lumepeer_service::program_data::discard_untrusted_file(&path)
        && std::fs::write(&path, b"").is_ok()
}

/// Starts the person's client in `session` if the session host before this
/// one closed it to install an update; takes the word back either way.
pub async fn relaunch_client_if_asked(session: u32) {
    let asked = tokio::task::spawn_blocking(|| {
        let directory = lumepeer_service::program_data::updates_directory()?;
        take_relaunch_word(&directory).then_some(())
    })
    .await
    .ok()
    .flatten()
    .is_some();
    if !asked {
        return;
    }
    tracing::info!("an update closed the client; starting it again");
    let started =
        tokio::task::spawn_blocking(move || lumepeer_service::client_launch::start_client(session))
            .await
            .unwrap_or(false);
    if !started {
        tracing::warn!("the client did not start again after the update");
    }
}

/// Whether `directory` holds the word to start the client again; removes it.
/// Only a file `LocalSystem` or administrators wrote counts.
fn take_relaunch_word(directory: &Path) -> bool {
    let path = directory.join(RELAUNCH_CLIENT_FILE);
    if std::fs::symlink_metadata(&path).is_err() {
        return false;
    }
    // `discard_untrusted_file` removes a file somebody else made and keeps
    // one this service wrote; only the second is word from a session host.
    let trusted = lumepeer_service::program_data::discard_untrusted_file(&path) && path.exists();
    let _ = std::fs::remove_file(&path);
    trusted
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use ed25519_dalek::{Signer as _, SigningKey};

    use super::*;

    const KEY_ID: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

    /// A minisign key pair: the signing half, and the public half as
    /// `tauri.conf.json` holds one — base64 of the `.pub` file.
    fn key(seed: u8) -> (SigningKey, String) {
        let signing = SigningKey::from_bytes(&[seed; 32]);
        let mut raw = b"Ed".to_vec();
        raw.extend_from_slice(&KEY_ID);
        raw.extend_from_slice(&signing.verifying_key().to_bytes());
        let file = format!(
            "untrusted comment: minisign public key: test\n{}\n",
            data_encoding::BASE64.encode(&raw)
        );
        (signing, data_encoding::BASE64.encode(file.as_bytes()))
    }

    /// A minisign signature of `data` with `trusted` as its trusted comment,
    /// as the manifest holds one — base64 of the `.sig` file. Legacy mode:
    /// the data itself is signed, which needs no `BLAKE2b` here.
    fn sign(signing: &SigningKey, data: &[u8], trusted: &str) -> String {
        let signature = signing.sign(data).to_bytes();
        let mut raw = b"Ed".to_vec();
        raw.extend_from_slice(&KEY_ID);
        raw.extend_from_slice(&signature);
        let mut global = signature.to_vec();
        global.extend_from_slice(trusted.as_bytes());
        let global = signing.sign(&global).to_bytes();
        let file = format!(
            "untrusted comment: signature\n{}\ntrusted comment: {trusted}\n{}\n",
            data_encoding::BASE64.encode(&raw),
            data_encoding::BASE64.encode(&global)
        );
        data_encoding::BASE64.encode(file.as_bytes())
    }

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    /// A directory of the test's own under the temp folder, emptied first.
    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "lumepeer-host-update-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    /// The key this binary checks with is the one `tauri-plugin-updater` is
    /// configured with, and it is a minisign public key.
    #[test]
    fn the_compiled_in_key_is_the_updaters() {
        let key = public_key().expect("tauri.conf.json carries an updater key");
        let text =
            String::from_utf8(data_encoding::BASE64.decode(key.as_bytes()).unwrap()).unwrap();
        assert!(minisign_verify::PublicKey::decode(&text).is_ok());
    }

    /// A signature by the right key over the right bytes verifies; another
    /// key, other bytes, or garbage do not.
    #[test]
    fn only_the_right_key_over_the_right_bytes_verifies() {
        let (signing, public) = key(7);
        let (_, other) = key(9);
        let data = b"an installer";
        let signature = sign(&signing, data, "timestamp:1\tfile:setup.exe");
        let v = version("0.0.166");
        assert_eq!(verify(data, &signature, &public, &v), Ok(()));
        assert_eq!(
            verify(b"another installer", &signature, &public, &v),
            Err(Refusal::Signature)
        );
        assert_eq!(
            verify(data, &signature, &other, &v),
            Err(Refusal::Signature)
        );
        assert_eq!(
            verify(data, "not base64!", &public, &v),
            Err(Refusal::Signature)
        );
        assert_eq!(verify(data, "", &public, &v), Err(Refusal::Signature));
    }

    /// A signature that names a version must name the announced one: a
    /// manifest cannot pair a new number with an older signed release.
    #[test]
    fn a_signed_version_must_be_the_announced_one() {
        let (signing, public) = key(7);
        let data = b"an installer";
        let signature = sign(
            &signing,
            data,
            "timestamp:1\tfile:setup.exe\tversion:0.0.150",
        );
        assert_eq!(
            verify(data, &signature, &public, &version("0.0.150")),
            Ok(())
        );
        assert_eq!(
            verify(data, &signature, &public, &version("0.0.166")),
            Err(Refusal::SignedVersion)
        );
    }

    /// The manifest offers only a newer release, only through this platform's
    /// NSIS entry, and only over `https`.
    #[test]
    fn the_manifest_offers_only_a_newer_nsis_installer_over_https() {
        let manifest = |version: &str, url: &str| {
            format!(
                r#"{{"version":"{version}","notes":"n","pub_date":"2026-10-28T00:00:00Z","platforms":{{
                    "windows-x86_64":{{"signature":"s","url":"https://example.invalid/a.msi"}},
                    "windows-x86_64-nsis":{{"signature":"s","url":"{url}"}}}}}}"#
            )
        };
        let current = version("0.0.165");
        let key = "windows-x86_64-nsis";
        let offer = offer_in(
            manifest("v0.0.166", "https://example.invalid/setup.exe").as_bytes(),
            &current,
            key,
        )
        .unwrap()
        .expect("a newer release is offered");
        assert_eq!(offer.version, version("0.0.166"));
        assert_eq!(offer.url, "https://example.invalid/setup.exe");
        assert_eq!(offer.notes, "n");

        for same_or_older in ["0.0.165", "0.0.100"] {
            assert_eq!(
                offer_in(
                    manifest(same_or_older, "https://example.invalid/setup.exe").as_bytes(),
                    &current,
                    key
                ),
                Ok(None)
            );
        }
        assert_eq!(
            offer_in(
                manifest("0.0.166", "http://example.invalid/setup.exe").as_bytes(),
                &current,
                key
            ),
            Err(Refusal::NotHttps)
        );
        assert_eq!(
            offer_in(
                manifest("0.0.166", "https://example.invalid/setup.exe").as_bytes(),
                &current,
                "windows-aarch64-nsis"
            ),
            Err(Refusal::NoEntry)
        );
        assert_eq!(offer_in(b"<html>", &current, key), Err(Refusal::Manifest));
    }

    /// The pack's first item: a file put in the updates directory with a
    /// signature that does not verify is deleted and never handed to the
    /// launcher; one that verifies is.
    #[test]
    fn a_planted_installer_with_a_wrong_signature_is_deleted_not_started() {
        let directory = scratch("planted");
        let (signing, public) = key(7);
        let path = directory.join(INSTALLER_FILE);
        let offer = Offer {
            version: version("0.0.166"),
            notes: String::new(),
            url: "https://example.invalid/setup.exe".to_owned(),
            signature: sign(&signing, b"the real installer", "timestamp:1"),
        };

        std::fs::write(&path, b"something else entirely").unwrap();
        assert_eq!(
            verified_installer(&directory, &offer, &public),
            Err(Refusal::Signature)
        );
        assert!(!path.exists(), "a file that did not verify is deleted");

        assert_eq!(
            verified_installer(&directory, &offer, &public),
            Err(Refusal::Disk),
            "no file, nothing to start"
        );

        std::fs::write(&path, b"the real installer").unwrap();
        assert_eq!(verified_installer(&directory, &offer, &public), Ok(path));
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The installer's name and arguments are constants: nothing about what
    /// is started comes from the manifest or the client.
    #[test]
    fn the_installer_is_a_fixed_name_started_silently() {
        assert!(!INSTALLER_FILE.contains(['\\', '/']));
        assert_eq!(INSTALLER_ARGS, ["/S", "/UPDATE"]);
        assert!(
            !INSTALLER_ARGS.contains(&"/R"),
            "the installer runs as LocalSystem: it starts nothing"
        );
        assert_eq!(platform_key(), Some("windows-x86_64-nsis"));
    }

    /// A client's request is taken unless the host knows there is nothing to
    /// install, and it carries no package.
    #[test]
    fn install_now_is_taken_unless_there_is_nothing_to_install() {
        let updates = Updates::default();
        assert!(updates.ask_to_install(), "not checked yet: the host checks");
        updates.set(HostUpdate::Off);
        assert!(!updates.ask_to_install());
        updates.set(HostUpdate::Current);
        assert!(!updates.ask_to_install());
        updates.set(HostUpdate::Available {
            version: "0.0.166".to_owned(),
            notes: String::new(),
        });
        assert!(updates.ask_to_install());
        assert!(!updates.leaving());
    }

    /// The word to start the client again counts once, and only when it is
    /// there.
    #[test]
    fn the_relaunch_word_is_taken_once() {
        let directory = scratch("relaunch");
        assert!(!take_relaunch_word(&directory));
        assert!(mark_relaunch(&directory));
        // Unelevated, the file this test wrote is the ordinary user's, which
        // is exactly what must not count; elevated, it is administrators'.
        let elevated =
            lumepeer_service::program_data::this_process().is_some_and(|(_, elevated)| elevated);
        assert_eq!(take_relaunch_word(&directory), elevated);
        assert!(!directory.join(RELAUNCH_CLIENT_FILE).exists());
        assert!(!take_relaunch_word(&directory));
        let _ = std::fs::remove_dir_all(&directory);
    }
}
