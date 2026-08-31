//! Runtime configuration (§5.1) and the OS directories the app owns.
//!
//! `config/default.toml` describes intent — a relay to prefer, where logs go,
//! whether direct paths are wanted — and until now nothing read it, so the
//! binary and the file were free to disagree (ADR 0020 recorded that; ADR 0026
//! is what fixes it). An installed client has no shell to set an environment
//! variable in, so the file is the only way its operator can point it at a
//! self-hosted relay (docs/relay-deployment.md).
//!
//! Nothing here authorizes anything: consent, roles and grants stay in
//! `lumepeer-core` (§2.3). The worst a bad config can do is send this client
//! to a relay that does not exist, and a malformed file is ignored rather than
//! obeyed halfway.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Application identifier; the same one `tauri.conf.json` bundles under.
pub const APP_IDENTIFIER: &str = "io.insigmo.lumepeer";

/// Environment variable naming one extra config file, read last so it wins.
const CONFIG_FILE_ENV: &str = "LUMEPEER_CONFIG";
/// Environment variable overriding the log directory outright.
const LOG_DIR_ENV: &str = "LUMEPEER_LOG_DIR";

/// Everything this binary reads out of the configuration files.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `[network]`.
    pub network: Network,
    /// `[logging]`.
    pub logging: Logging,
    /// `[updates]`.
    pub updates: Updates,
}

/// `[network]` of `config/default.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Network {
    /// Relay peers fall back to when hole punching fails. `None` means
    /// iroh's default public fleet (docs/relay-deployment.md).
    pub relay_url: Option<String>,
    /// Whether direct paths may be used at all. `false` is the relay-only
    /// mode of ADR 0020 — a deliberate WAN test, never a shipping default.
    pub prefer_direct: bool,
}

impl Default for Network {
    fn default() -> Self {
        Self {
            relay_url: None,
            prefer_direct: true,
        }
    }
}

/// `[logging]` of `config/default.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Logging {
    /// Where the rotating log file lives (§16.1). A relative path is resolved
    /// under the per-user data directory, because the directory an installed
    /// client is started from is not one it may write to.
    pub directory: Option<String>,
}

/// Which stream of releases this client updates from (ADR 0042).
///
/// A setting rather than a compiled-in constant, because the whole point of a
/// beta channel is that a person can move one machine onto it without a
/// special build. Default is stable, and moving off it takes an explicit edit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    /// Released builds only. GitHub's own "latest release" skips prereleases,
    /// so a stable client cannot be handed a beta by accident.
    #[default]
    Stable,
    /// Prereleases as well, from the rolling `beta` release.
    Beta,
}

/// `[updates]` of `config/default.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Updates {
    /// Release stream to follow.
    pub channel: UpdateChannel,
    /// Base URL update manifests are fetched from, without a trailing slash.
    ///
    /// Configurable so a self-hosting operator can serve their own builds the
    /// way `relay_url` lets them serve their own relay. `None` is the project's
    /// own GitHub releases.
    pub manifest_base_url: Option<String>,
}

/// Where the project publishes its own update manifests.
const DEFAULT_MANIFEST_BASE: &str = "https://github.com/insigmo/lumepeer/releases";

impl Settings {
    /// Loads every config file that exists, later files winning key by key.
    ///
    /// Returns the settings together with the notes worth logging once
    /// tracing exists: this runs before the subscriber is installed, so it
    /// cannot log anything itself, and a config problem that is only
    /// `eprintln`ed is invisible in a windowed release build.
    #[must_use]
    pub fn load() -> (Self, Vec<String>) {
        let mut settings = Self::default();
        let mut notes = Vec::new();
        for path in Self::search_path() {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            match toml::from_str::<Self>(&text) {
                Ok(parsed) => {
                    settings.merge(parsed);
                    notes.push(format!("configuration read from {}", path.display()));
                }
                // A file that does not parse is skipped whole rather than
                // applied in part: half a config is not a state anyone wrote
                // down deliberately (§2.1).
                Err(error) => notes.push(format!(
                    "ignoring {}: not valid configuration ({error})",
                    path.display()
                )),
            }
        }
        (settings, notes)
    }

    /// Config files in the order they are read; later ones win.
    fn search_path() -> Vec<PathBuf> {
        let mut paths = vec![
            // Development: the repository's own files, relative to the
            // working directory `cargo run` / `npm run tauri dev` starts in.
            PathBuf::from("config/default.toml"),
            PathBuf::from("config/local.toml"),
        ];
        // Installed: whatever ships next to the executable, then the per-user
        // file, which is the one an operator can actually edit.
        if let Some(dir) = exe_dir() {
            paths.push(dir.join("config").join("default.toml"));
        }
        if let Some(dir) = config_dir() {
            paths.push(dir.join("config.toml"));
        }
        if let Ok(explicit) = std::env::var(CONFIG_FILE_ENV) {
            paths.push(PathBuf::from(explicit));
        }
        paths
    }

    /// Overlays `other` onto `self`, key by key: an override file that sets
    /// one value must not blank out the rest.
    fn merge(&mut self, other: Self) {
        if other.network.relay_url.is_some() {
            self.network.relay_url = other.network.relay_url;
        }
        self.network.prefer_direct = other.network.prefer_direct;
        if other.logging.directory.is_some() {
            self.logging.directory = other.logging.directory;
        }
        self.updates.channel = other.updates.channel;
        if other.updates.manifest_base_url.is_some() {
            self.updates.manifest_base_url = other.updates.manifest_base_url;
        }
    }

    /// The update manifest this client checks, or `None` when updates are not
    /// configured at all (ADR 0042).
    ///
    /// One URL per channel, and the channel decides which. The stable manifest
    /// rides GitHub's `latest` redirect, which skips prereleases outright, so
    /// a stable client is not merely *asked* not to take a beta — it is never
    /// shown one. The beta manifest lives on a rolling `beta` release, because
    /// GitHub has no "newest including prereleases" download URL.
    ///
    /// A base URL that is not `https` is refused: an update manifest names
    /// what this machine is about to install, and while the artifact's own
    /// signature is what actually gates the install (§21), there is no reason
    /// to accept the manifest over a channel anyone can rewrite.
    #[must_use]
    pub fn update_manifest_url(&self) -> Option<String> {
        let base = self
            .updates
            .manifest_base_url
            .as_deref()
            .unwrap_or(DEFAULT_MANIFEST_BASE)
            .trim_end_matches('/');
        if !base.starts_with("https://") {
            return None;
        }
        Some(match self.updates.channel {
            UpdateChannel::Stable => format!("{base}/latest/download/latest.json"),
            UpdateChannel::Beta => format!("{base}/download/beta/beta.json"),
        })
    }

    /// The release stream this client follows.
    #[must_use]
    pub const fn update_channel(&self) -> UpdateChannel {
        self.updates.channel
    }

    /// Relay to hand `PeerEndpoint::bind`, if one is configured.
    /// `LUMEPEER_RELAY_URL` still overrides it.
    #[must_use]
    pub fn relay_url(&self) -> Option<&str> {
        self.network.relay_url.as_deref()
    }

    /// Whether this run must stay on the relay: either the config asked for
    /// it or `LUMEPEER_RELAY_ONLY` did.
    #[must_use]
    pub fn relay_only(&self) -> bool {
        !self.network.prefer_direct || lumepeer_net::endpoint::relay_only_enabled()
    }

    /// Directory the rotating log file lives in (§16.1), or `None` when no
    /// writable location can be derived at all — in which case logging stays
    /// on stdout rather than failing the start.
    #[must_use]
    pub fn log_dir(&self) -> Option<PathBuf> {
        if let Ok(explicit) = std::env::var(LOG_DIR_ENV) {
            return Some(PathBuf::from(explicit));
        }
        match self.logging.directory.as_deref() {
            // An absolute path is taken at its word; a relative one is
            // resolved under the data directory, never under the working
            // directory, which for an installed client is not writable.
            Some(dir) if Path::new(dir).is_absolute() => Some(PathBuf::from(dir)),
            Some(dir) => data_dir().map(|base| base.join(dir)),
            None => data_dir().map(|base| base.join("logs")),
        }
    }
}

/// Directory holding the running executable.
fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

/// Per-user configuration directory for this app.
///
/// Resolved from the environment rather than through Tauri's path API: the
/// config decides how tracing is set up, and tracing is installed before there
/// is an `AppHandle` to ask.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    let base = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        home().map(|home| home.join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|home| home.join(".config")))
    };
    base.map(|dir| dir.join(APP_IDENTIFIER))
}

/// Per-user data directory for this app.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    let base = if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        home().map(|home| home.join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|home| home.join(".local").join("share")))
    };
    base.map(|dir| dir.join(APP_IDENTIFIER))
}

/// Directory session recordings are written into (§17).
///
/// Under the per-user data directory, never anywhere the webview names: the
/// untrusted view layer says *whether* to record, and this says where (§2.3).
#[must_use]
pub fn recordings_dir() -> Option<PathBuf> {
    data_dir().map(|base| base.join("recordings"))
}

/// Directory a completed clipboard file receive lands in, so the paste it
/// exists to serve actually has something on disk to point at (docs/bugs/
/// 14-clipboard-files.md #3; ADR 0046).
///
/// Under the per-user data directory like `recordings_dir`, for the same
/// reason: this is application working storage, not a place the untrusted
/// view layer names. Ephemeral rather than a user-facing library — every
/// per-peer subdirectory under it is removed when that peer's session ends,
/// and the whole thing is swept once at startup in case a previous run never
/// got to.
#[must_use]
pub fn clipboard_files_dir() -> Option<PathBuf> {
    data_dir().map(|base| base.join("clipboard-files"))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failed assumption must fail the test"
    )]

    use super::*;

    #[test]
    fn an_empty_config_keeps_the_shipping_defaults() {
        let settings = Settings::default();
        assert!(settings.relay_url().is_none());
        assert!(
            settings.network.prefer_direct,
            "direct paths are the shipping default (ADR 0026)"
        );
    }

    #[test]
    fn the_repository_default_config_parses_into_these_settings() {
        // The file the design doc describes has to be readable by the code
        // that claims to read it; that agreement is exactly what ADR 0020
        // recorded as broken.
        let text = include_str!("../../../../config/default.toml");
        let parsed = toml::from_str::<Settings>(text).expect("config/default.toml must parse");
        assert!(parsed.network.prefer_direct);
        assert_eq!(parsed.logging.directory.as_deref(), Some("logs"));
    }

    #[test]
    fn a_later_file_overrides_only_the_keys_it_sets() {
        let mut settings = Settings::default();
        settings.network.relay_url = Some("https://relay.example.com".to_owned());
        let overlay = toml::from_str::<Settings>("[logging]\ndirectory = \"elsewhere\"\n")
            .expect("overlay parses");
        settings.merge(overlay);
        assert_eq!(
            settings.relay_url(),
            Some("https://relay.example.com"),
            "an override that says nothing about the relay must not drop it"
        );
        assert_eq!(settings.logging.directory.as_deref(), Some("elsewhere"));
    }

    #[test]
    fn a_relative_log_directory_never_lands_in_the_working_directory() {
        let settings = Settings {
            logging: Logging {
                directory: Some("logs".to_owned()),
            },
            ..Settings::default()
        };
        // Only meaningful where a data directory exists at all; where it does
        // not, `log_dir` is `None` and logging stays on stdout by design.
        if let Some(dir) = settings.log_dir() {
            assert!(
                dir.is_absolute(),
                "a relative directory must be resolved, not used as-is: {}",
                dir.display()
            );
        }
    }

    #[test]
    fn relay_only_is_off_unless_something_asks_for_it() {
        // The environment variable is not set in the test process, so this is
        // the config's answer alone.
        assert!(!Settings::default().relay_only());
        let mut settings = Settings::default();
        settings.network.prefer_direct = false;
        assert!(settings.relay_only());
    }

    /// A fresh install follows stable, and the two channels resolve to two
    /// different manifests (ADR 0042). The stable one goes through GitHub's
    /// `latest` redirect on purpose: that redirect skips prereleases, which is
    /// what keeps a stable client from ever being shown a beta.
    #[test]
    fn the_channel_decides_the_manifest() {
        let stable = Settings::default();
        assert_eq!(stable.update_channel(), UpdateChannel::Stable);
        let stable_url = stable.update_manifest_url().unwrap();
        assert!(stable_url.ends_with("/releases/latest/download/latest.json"));

        let mut beta = Settings::default();
        beta.updates.channel = UpdateChannel::Beta;
        let beta_url = beta.update_manifest_url().unwrap();
        assert!(beta_url.ends_with("/releases/download/beta/beta.json"));
        assert_ne!(stable_url, beta_url);
    }

    /// A self-hoster's own base URL is honoured, with or without a trailing
    /// slash, but only over https.
    #[test]
    fn a_custom_manifest_base_must_be_https() {
        let mut settings = Settings::default();
        settings.updates.manifest_base_url =
            Some("https://updates.example.com/lumepeer/".to_owned());
        assert_eq!(
            settings.update_manifest_url().as_deref(),
            Some("https://updates.example.com/lumepeer/latest/download/latest.json")
        );

        settings.updates.manifest_base_url = Some("http://updates.example.com".to_owned());
        assert_eq!(
            settings.update_manifest_url(),
            None,
            "a plaintext manifest URL is no configuration at all"
        );
    }

    /// `channel = "beta"` is written the way a person would write it.
    #[test]
    fn the_channel_parses_from_the_config_file() {
        let parsed: Settings = toml::from_str("[updates]\nchannel = \"beta\"\n").unwrap();
        assert_eq!(parsed.update_channel(), UpdateChannel::Beta);
    }
}
