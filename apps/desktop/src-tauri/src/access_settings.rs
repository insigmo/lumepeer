//! This application as the access-settings window only (ADR 0169 §5).
//!
//! Where the session host of ADR 0169 hosts this machine, the client the
//! person runs is a guest and the host's panel, and none of its programs may
//! change who gets in: the device password, its second factor, the
//! unattended role, the logon screen and the trusted devices live where only
//! `LocalSystem` and administrators write (ADR 0123 §5), and the channel to the
//! session host cannot say any of it (ADR 0169 §4). Changing them takes this
//! binary started again elevated, with [`lumepeer_service::ACCESS_SETTINGS_ARG`]
//! and the SID of the account whose policy it is — one UAC prompt, at the
//! moment of the change (`lumepeer_service::access_settings::open_elevated`).
//!
//! That process is the elevated client with everything but this taken out: no
//! host role, no actor, no endpoint, no tray, no guest window, no outgoing
//! connection. One window, `access.html`, whose commands are the main
//! window's own `unattended_*` and `address_book_*` — they find an
//! [`AccessSettings`] managed here instead of an actor, and act on it. It
//! writes the account's protected stores through the same store types the
//! actor uses (`UnattendedStore`, `AddressBookStore`), audits a trust change
//! as the actor does, and after every write raises the account's
//! policy-changed event so the session host reads the policy again without
//! restarting.

use std::sync::Mutex;

use lumepeer_core::NodeId;
use lumepeer_core::address_book::AddressEntry;
use lumepeer_core::audit::{AuditEvent, AuditRecord, AuditSink, peer_hash};
use lumepeer_core::consent::Role;
use lumepeer_core::unattended::{UnattendedAccess, UnattendedError};
use lumepeer_net::keystore::Keystore;
use lumepeer_runtime::address_book_store::AddressBookStore;
use lumepeer_runtime::network::{
    ActorError, AddressBookRow, TotpProvisioning, UnattendedSettings, peer_tag,
};
use lumepeer_runtime::unattended_store::UnattendedStore;

/// The account's access policy, open for editing in this process.
pub struct AccessSettings {
    /// The SID of the account whose policy this is.
    account: String,
    policy: Mutex<Policy>,
    /// Tells the session host the policy was written.
    told: fn(&str),
}

impl std::fmt::Debug for AccessSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessSettings")
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

/// What the stores hold, as of the last read.
struct Policy {
    unattended: UnattendedAccess,
    store: UnattendedStore,
    book: AddressBookStore,
    /// Salt of this process's device labels: they are labels for this window
    /// only, never identities (§15).
    labels: [u8; 32],
    audit: Option<(Box<dyn AuditSink>, [u8; 32])>,
}

impl AccessSettings {
    /// The policy of `account`, kept in `keystore` and the address book at
    /// `address_book`; trust changes are audited into `audit` when there is
    /// one, and `told` is called after every write.
    #[must_use]
    pub fn new(
        account: String,
        keystore: Box<dyn Keystore>,
        address_book: Option<std::path::PathBuf>,
        audit: Option<(Box<dyn AuditSink>, [u8; 32])>,
        told: fn(&str),
    ) -> Self {
        use rand::Rng as _;

        let store = UnattendedStore::new(keystore);
        let mut unattended = UnattendedAccess::new();
        store.restore(&mut unattended);
        let mut labels = [0u8; 32];
        rand::rng().fill_bytes(&mut labels);
        Self {
            account,
            policy: Mutex::new(Policy {
                unattended,
                store,
                book: AddressBookStore::open(address_book),
                labels,
                audit,
            }),
            told,
        }
    }

    /// The SID of the account whose policy this is.
    #[must_use]
    pub fn account(&self) -> &str {
        &self.account
    }

    /// The policy as it stands, read again first: the session host may have
    /// turned unattended access off over the client's channel meanwhile.
    fn fresh(&self) -> Result<std::sync::MutexGuard<'_, Policy>, ActorError> {
        let mut policy = self.policy.lock().map_err(|_| ActorError::ChannelClosed)?;
        let Policy {
            unattended,
            store,
            book,
            ..
        } = &mut *policy;
        store.reload(unattended);
        book.reload();
        Ok(policy)
    }

    fn tell(&self) {
        (self.told)(&self.account);
    }

    /// Whether unattended access is on, and how.
    ///
    /// # Errors
    /// [`ActorError::ChannelClosed`] when an earlier call panicked.
    pub fn status(&self) -> Result<UnattendedSettings, ActorError> {
        let policy = self.fresh()?;
        Ok(UnattendedSettings {
            enabled: policy.unattended.enabled(),
            totp_enabled: policy.unattended.code_required(),
            role: policy.unattended.role(),
        })
    }

    /// Sets or replaces the device password, as the actor's own setter does:
    /// a store that refuses the write undoes the change.
    ///
    /// # Errors
    /// [`ActorError::Unattended`] for a password outside the policy,
    /// [`ActorError::Net`] when the store refuses it.
    pub fn set_password(&self, password: &str) -> Result<(), ActorError> {
        let mut policy = self.fresh()?;
        let previous = policy.unattended.stored_secret().map(ToOwned::to_owned);
        policy
            .unattended
            .set_password(password)
            .map_err(ActorError::Unattended)?;
        if let Err(error) = policy.store.save_password(&policy.unattended) {
            match previous {
                Some(phc) => policy.unattended.restore_password_hash(&phc),
                None => policy.unattended.disable(),
            }
            return Err(ActorError::Net(error));
        }
        drop(policy);
        tracing::info!("the unattended device password was set (access settings)");
        self.tell();
        Ok(())
    }

    /// Turns unattended access off and forgets both factors.
    ///
    /// # Errors
    /// [`ActorError::Net`] when the store refuses to drop them.
    pub fn disable(&self) -> Result<(), ActorError> {
        let mut policy = self.fresh()?;
        policy.unattended.disable();
        policy.store.clear().map_err(ActorError::Net)?;
        drop(policy);
        tracing::info!("unattended access turned off (access settings)");
        self.tell();
        Ok(())
    }

    /// Turns the second factor on — returning its provisioning once — or off.
    ///
    /// # Errors
    /// [`ActorError::Unattended`] when there is no password to add it to,
    /// [`ActorError::Net`] when the store refuses it.
    pub fn set_totp(&self, enabled: bool) -> Result<Option<TotpProvisioning>, ActorError> {
        use rand::Rng as _;

        let mut policy = self.fresh()?;
        if !enabled {
            policy.unattended.clear_totp_secret();
            policy
                .store
                .save_totp(&policy.unattended)
                .map_err(ActorError::Net)?;
            drop(policy);
            tracing::info!("the unattended second factor was turned off (access settings)");
            self.tell();
            return Ok(None);
        }
        if !policy.unattended.enabled() {
            return Err(ActorError::Unattended(UnattendedError::NotConfigured));
        }
        let previous = policy.unattended.stored_totp_secret().copied();
        let mut secret = [0u8; 20];
        rand::rng().fill_bytes(&mut secret);
        policy.unattended.set_totp_secret(secret);
        if let Err(error) = policy.store.save_totp(&policy.unattended) {
            match previous {
                Some(old) => policy.unattended.set_totp_secret(old),
                None => policy.unattended.clear_totp_secret(),
            }
            return Err(ActorError::Net(error));
        }
        let totp = policy
            .unattended
            .totp()
            .ok_or(ActorError::Unattended(UnattendedError::NotConfigured))?;
        drop(policy);
        tracing::info!("the unattended second factor was turned on (access settings)");
        self.tell();
        Ok(Some(TotpProvisioning {
            secret_base32: totp.secret_base32(),
            // A fixed account name, never this machine's, as the actor's.
            uri: totp.provisioning_uri("device"),
        }))
    }

    /// Chooses the role a successful unattended admission is granted.
    ///
    /// # Errors
    /// [`ActorError::Net`] when the store refuses it.
    pub fn set_role(&self, role: Role) -> Result<(), ActorError> {
        let mut policy = self.fresh()?;
        let previous = policy.unattended.role();
        policy.unattended.set_role(role);
        if let Err(error) = policy.store.save_role(&policy.unattended) {
            policy.unattended.set_role(previous);
            return Err(ActorError::Net(error));
        }
        drop(policy);
        tracing::info!(?role, "the unattended role was set (access settings)");
        self.tell();
        Ok(())
    }

    /// The saved devices, labelled for this window.
    ///
    /// # Errors
    /// [`ActorError::ChannelClosed`] when an earlier call panicked.
    pub fn book(&self) -> Result<Vec<AddressBookRow>, ActorError> {
        let policy = self.fresh()?;
        Ok(policy
            .book
            .book()
            .peers()
            .map(|(peer, entry)| AddressBookRow {
                peer_label: peer_tag(&policy.labels, &peer),
                name: entry.label.clone(),
                tags: entry.tags.clone(),
                notes: entry.notes.clone(),
                trusted: entry.trusted,
                // Nothing connects to this process.
                connected: false,
            })
            .collect())
    }

    /// Renames, retags or re-notes one saved device, keeping its trust.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] when `label` names no saved device.
    pub fn upsert(
        &self,
        label: &str,
        name: String,
        tags: Vec<String>,
        notes: String,
    ) -> Result<(), ActorError> {
        let mut policy = self.fresh()?;
        let peer = policy.resolve(label)?;
        let trusted = policy.book.book().is_trusted(&peer);
        policy.book.upsert(
            &peer,
            AddressEntry {
                label: name,
                tags,
                notes,
                trusted,
            },
        );
        drop(policy);
        self.tell();
        Ok(())
    }

    /// Forgets one saved device, and any trust it held.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] when `label` names no saved device.
    pub fn remove(&self, label: &str) -> Result<(), ActorError> {
        let mut policy = self.fresh()?;
        let peer = policy.resolve(label)?;
        if !policy.book.remove(&peer) {
            return Err(ActorError::UnknownPeer);
        }
        drop(policy);
        tracing::info!(peer = %label, "address book entry removed (access settings)");
        self.tell();
        Ok(())
    }

    /// Marks a saved device trusted, or withdraws that, and audits it as the
    /// actor does (§15; ADR 0034).
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] when `label` names no saved device.
    pub fn set_trusted(&self, label: &str, trusted: bool) -> Result<(), ActorError> {
        let mut policy = self.fresh()?;
        let peer = policy.resolve(label)?;
        let Some(now) = policy.book.set_trusted(&peer, trusted) else {
            return Err(ActorError::UnknownPeer);
        };
        policy.audit(&peer, AuditEvent::DeviceTrustChanged { trusted: now });
        drop(policy);
        tracing::info!(peer = %label, trusted = now, "device trust changed (access settings)");
        self.tell();
        Ok(())
    }
}

impl Policy {
    fn resolve(&self, label: &str) -> Result<NodeId, ActorError> {
        self.book
            .book()
            .peers()
            .map(|(peer, _)| peer)
            .find(|peer| peer_tag(&self.labels, peer) == label)
            .ok_or(ActorError::UnknownPeer)
    }

    fn audit(&mut self, peer: &NodeId, event: AuditEvent) {
        let Some((sink, salt)) = self.audit.as_mut() else {
            return;
        };
        let at_unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        sink.append(AuditRecord {
            peer_hash: peer_hash(salt, peer),
            at_unix_secs,
            event,
        });
    }
}

/// Runs this process as the access-settings window and never returns
/// (ADR 0169 §5).
#[cfg(target_os = "windows")]
pub fn run(args: &[String], mut context: tauri::Context<tauri::Wry>) -> ! {
    let (settings, notes) = lumepeer_runtime::config::Settings::load();
    let _log = crate::logging::init(&settings);
    for note in notes {
        tracing::info!("{note}");
    }
    let code = match open(args) {
        Ok(editor) => {
            for window in &mut context.config_mut().app.windows {
                if window.label == "main" {
                    window.url = tauri::WebviewUrl::App("access.html".into());
                    "Lumepeer".clone_into(&mut window.title);
                    window.width = 640.0;
                    window.height = 720.0;
                    // The ordinary frame: this window is not the main
                    // screen, and has no title bar of its own to draw.
                    window.decorations = true;
                }
            }
            match tauri::Builder::default()
                .manage(editor)
                .invoke_handler(crate::invoke_handler())
                .run(context)
            {
                Ok(()) => 0,
                Err(error) => {
                    tracing::error!(%error, "the access settings window failed");
                    1
                }
            }
        }
        Err(code) => code,
    };
    std::process::exit(code);
}

/// The policy this run edits, or the exit code to leave with.
#[cfg(target_os = "windows")]
fn open(args: &[String]) -> Result<AccessSettings, i32> {
    let Some(Some(account)) = lumepeer_service::access_settings::requested_account(args) else {
        tracing::error!("--access-settings needs the SID of the account to edit");
        return Err(2);
    };
    if !lumepeer_service::program_data::this_process().is_some_and(|(_, elevated)| elevated) {
        tracing::error!(
            "the access settings window runs only elevated: what it writes is where only \
             administrators can"
        );
        return Err(3);
    }
    // A development build hosts in its own window and holds this policy in its
    // own actor, which would not hear a change made here: it is edited there.
    if !crate::placement::session_host_guest() {
        tracing::error!(
            "this build hosts in the client, not in the Lumepeer service; its access settings \
             are in the client"
        );
        return Err(4);
    }
    let Some(placement) = crate::placement::for_system_host(&account) else {
        tracing::error!("cannot open this account's protected store");
        return Err(5);
    };
    let audit = tauri::async_runtime::block_on(crate::bootstrap::open_audit_log(
        placement.audit.clone(),
        placement.keystore.as_ref(),
    ))
    .map(|store| {
        let salt = *store.salt();
        (Box::new(store) as Box<dyn AuditSink>, salt)
    });
    tracing::info!("editing this machine's access settings (ADR 0169 §5)");
    Ok(AccessSettings::new(
        account,
        placement.keystore,
        placement.address_book,
        audit,
        tell_the_session_host,
    ))
}

/// Raises the account's policy-changed event, and says what came of it.
#[cfg(target_os = "windows")]
fn tell_the_session_host(account: &str) {
    use lumepeer_service::access_settings::{SignalError, signal_policy_changed};

    match signal_policy_changed(account) {
        Ok(()) => tracing::info!("told the session host the access policy changed"),
        Err(SignalError::NobodyListening) => {
            tracing::info!("no session host is running; it reads the policy when it starts");
        }
        Err(error) => {
            tracing::warn!(
                ?error,
                "cannot tell the session host the access policy changed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::atomic::{AtomicUsize, Ordering};

    use lumepeer_net::keystore::MemoryKeystore;

    use super::*;

    static TOLD: AtomicUsize = AtomicUsize::new(0);

    fn count(_account: &str) {
        TOLD.fetch_add(1, Ordering::SeqCst);
    }

    fn peer(n: u8) -> NodeId {
        iroh::SecretKey::from_bytes(&[n; 32]).public()
    }

    fn editor(book: Option<std::path::PathBuf>) -> AccessSettings {
        AccessSettings::new(
            "S-1-5-21-1-2-3-1001".to_owned(),
            Box::new(MemoryKeystore::new()),
            book,
            None,
            count,
        )
    }

    /// Every write reaches the store and tells the session host; a refused
    /// one tells nobody.
    #[test]
    fn every_write_is_stored_and_told() {
        let editor = editor(None);
        let before = TOLD.load(Ordering::SeqCst);
        assert!(!editor.status().unwrap().enabled);

        assert!(editor.set_password("short").is_err(), "the policy of §8");
        editor.set_password("a long enough password").unwrap();
        assert!(editor.status().unwrap().enabled);
        assert!(editor.set_totp(true).unwrap().is_some());
        assert!(editor.status().unwrap().totp_enabled);
        editor.set_role(Role::ControlLimited).unwrap();
        assert_eq!(editor.status().unwrap().role, Role::ControlLimited);

        editor.disable().unwrap();
        let status = editor.status().unwrap();
        assert!(!status.enabled && !status.totp_enabled);
        assert_eq!(
            status.role,
            Role::ViewOnly,
            "the role goes with the password"
        );
        assert!(
            TOLD.load(Ordering::SeqCst) >= before + 4,
            "each of the four writes told the session host"
        );
    }

    /// A second factor needs a first.
    #[test]
    fn no_second_factor_without_a_password() {
        let editor = editor(None);
        assert!(matches!(
            editor.set_totp(true),
            Err(ActorError::Unattended(UnattendedError::NotConfigured))
        ));
    }

    /// Saved devices are listed under labels of this window, which name them
    /// back; trust moves only through `set_trusted`, and a label nothing
    /// carries resolves to nothing.
    #[test]
    fn saved_devices_are_labelled_and_trusted_here() {
        let dir = std::env::temp_dir().join(format!(
            "lumepeer-access-settings-book-{}",
            std::process::id()
        ));
        let path = dir.join("address_book.json");
        let mut seeded = AddressBookStore::open(Some(path.clone()));
        seeded.upsert(
            &peer(1),
            AddressEntry {
                label: "office".to_owned(),
                tags: Vec::new(),
                notes: String::new(),
                trusted: false,
            },
        );

        let editor = editor(Some(path.clone()));
        let rows = editor.book().unwrap();
        assert_eq!(rows.len(), 1);
        let label = rows[0].peer_label.clone();
        assert!(!rows[0].trusted);

        editor
            .upsert(&label, "front desk".to_owned(), Vec::new(), String::new())
            .unwrap();
        assert!(!editor.book().unwrap()[0].trusted, "renaming never trusts");
        editor.set_trusted(&label, true).unwrap();
        assert!(
            AddressBookStore::open(Some(path.clone()))
                .book()
                .is_trusted(&peer(1))
        );
        assert!(matches!(
            editor.set_trusted("not-a-label", true),
            Err(ActorError::UnknownPeer)
        ));
        editor.remove(&label).unwrap();
        assert!(editor.book().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
