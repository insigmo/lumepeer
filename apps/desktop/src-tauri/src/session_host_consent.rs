//! What the session host asks the person at the machine, in which words, and
//! what their answer does (ADR 0169 §5; pack 04).
//!
//! Every widening of this host is decided here, after a question in the
//! session host's own window (`lumepeer_service::consent_dialog`), which no
//! program the signed-in person runs can press:
//!
//! - **a guest coming in** without the device password (attended admission);
//! - **view only becoming control** in a running session;
//! - **`secure_desktop_input`** turned back on in a running session;
//! - **"trust this device"** (ADR 0120), as the dialog's check box or on its
//!   own.
//!
//! The first comes from the actor: a pending row in its status is a question
//! to put up ([`Consents::follow`]). The other three come from the client, on
//! the channel, as [`Widening`] — "show the person this question", never "do
//! it" ([`Consents::ask_to_widen`]). Either way the yes is the person's mouse
//! on a System-integrity window, and every other ending — a no, the window
//! closed, nobody there for [`CONSENT_DIALOG_TIMEOUT_SECS`] — changes nothing,
//! or refuses the guest that asked to come in ([`decide`]).
//!
//! What the person reads is plain text in Russian or English, the machine's
//! UI language. A peer's strings in it — device name, OS — are cut to
//! [`CONSENT_DIALOG_PEER_TEXT_MAX_CHARS`] and stripped of control and
//! direction-changing characters ([`plain`]); the window shows no hyperlinks.
//! The words follow the webview's consent window (`apps/desktop/src/locales`),
//! which the person already knows.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lumepeer_core::consent::{IndependentGrant, Role, SessionKind};
use lumepeer_core::constants::{CONSENT_DIALOG_PEER_TEXT_MAX_CHARS, CONSENT_DIALOG_TIMEOUT_SECS};
use lumepeer_runtime::network::{
    ActorError, ActorHandle, PeerDevice, SessionSnapshot, SessionStateDto,
};
use lumepeer_service::consent_dialog::{self, Outcome, Question};
use lumepeer_service::host_channel_protocol::Widening;

/// The button that says yes: the role asked for, or the widening.
const ID_ALLOW: i32 = 1001;
/// The button that lets a guest that asked for control watch instead.
const ID_VIEW_ONLY: i32 = 1002;
/// The button that says no, and the one Enter presses.
const ID_DENY: i32 = 1003;

/// The language the dialog speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    /// English, for every machine whose UI language is not Russian.
    En,
    /// Russian.
    Ru,
}

impl Language {
    /// This machine's.
    fn of_this_machine() -> Self {
        if consent_dialog::system_ui_is_russian() {
            Self::Ru
        } else {
            Self::En
        }
    }

    const fn texts(self) -> &'static Texts {
        match self {
            Self::En => &EN,
            Self::Ru => &RU,
        }
    }
}

/// Every word the dialog says, in one language. `{who}` is the guest.
#[derive(Debug)]
struct Texts {
    title: &'static str,
    wants_screen: &'static str,
    wants_terminal: &'static str,
    ask_control: &'static str,
    ask_secure_desktop_input: &'static str,
    ask_trust: &'static str,
    device: &'static str,
    system: &'static str,
    id: &'static str,
    asks_for: &'static str,
    unknown: &'static str,
    view_only: &'static str,
    control_limited: &'static str,
    full_control: &'static str,
    terminal: &'static str,
    about_full_control: &'static str,
    about_terminal: &'static str,
    about_secure_desktop_input: &'static str,
    about_trust: &'static str,
    allow: &'static str,
    allow_view: &'static str,
    allow_full: &'static str,
    allow_terminal: &'static str,
    deny: &'static str,
    remember: &'static str,
}

static EN: Texts = Texts {
    title: "Lumepeer",
    wants_screen: "{who} wants to connect",
    wants_terminal: "{who} wants a terminal on this computer",
    ask_control: "Give {who} control of this computer?",
    ask_secure_desktop_input: "Let {who} answer administrator prompts?",
    ask_trust: "Save {who} as a trusted device?",
    device: "Device",
    system: "System",
    id: "ID",
    asks_for: "Asks for",
    unknown: "unknown",
    view_only: "view only",
    control_limited: "limited control",
    full_control: "full control",
    terminal: "a terminal",
    about_full_control: "Full control is the keyboard and mouse, the clipboard and files, \
                         and answering administrator (UAC) prompts and the lock screen.",
    about_terminal: "A terminal is a command line on this computer, with no picture of the \
                     screen and no keyboard or mouse.",
    about_secure_desktop_input: "They will be able to approve programs that ask for \
                                 administrator rights, and use the lock screen.",
    about_trust: "A trusted device may sign in with this computer's device password without \
                  asking you. You can take this back in the address book.",
    allow: "Allow",
    allow_view: "Allow view only",
    allow_full: "Allow full control",
    allow_terminal: "Allow terminal",
    deny: "Deny",
    remember: "Remember this device",
};

static RU: Texts = Texts {
    title: "Lumepeer",
    wants_screen: "{who} хочет подключиться",
    wants_terminal: "{who} просит терминал на этом компьютере",
    ask_control: "Дать {who} управление этим компьютером?",
    ask_secure_desktop_input: "Разрешить {who} отвечать на запросы администратора?",
    ask_trust: "Сохранить {who} как доверенное устройство?",
    device: "Устройство",
    system: "Система",
    id: "ID",
    asks_for: "Просит",
    unknown: "неизвестно",
    view_only: "только просмотр",
    control_limited: "ограниченное управление",
    full_control: "полный доступ",
    terminal: "терминал",
    about_full_control: "Полный доступ — это клавиатура и мышь, буфер обмена и файлы, а также \
                         ответы на запросы администратора (UAC) и экран блокировки.",
    about_terminal: "Терминал — это командная строка на этом компьютере, без изображения \
                     экрана, клавиатуры и мыши.",
    about_secure_desktop_input: "Он сможет одобрять программы, которые просят права \
                                 администратора, и пользоваться экраном блокировки.",
    about_trust: "Доверенное устройство может входить по паролю этого компьютера, не \
                  спрашивая вас. Отменить это можно в адресной книге.",
    allow: "Разрешить",
    allow_view: "Разрешить только просмотр",
    allow_full: "Разрешить полный доступ",
    allow_terminal: "Разрешить терминал",
    deny: "Отклонить",
    remember: "Запомнить это устройство",
};

/// What the person is asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Subject {
    /// A guest asks to come in (a pending row).
    Admission {
        label: String,
        device: Option<PeerDevice>,
        role: Role,
        kind: SessionKind,
    },
    /// A running session may get one more thing, if the person says so.
    Widening {
        label: String,
        device: Option<PeerDevice>,
        widening: Widening,
    },
}

impl Subject {
    fn label(&self) -> &str {
        match self {
            Self::Admission { label, .. } | Self::Widening { label, .. } => label,
        }
    }

    fn device(&self) -> Option<&PeerDevice> {
        match self {
            Self::Admission { device, .. } | Self::Widening { device, .. } => device.as_ref(),
        }
    }

    const fn is_admission(&self) -> bool {
        matches!(self, Self::Admission { .. })
    }
}

/// What an answer does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// Let the guest in with `role`, and trust its device when ticked.
    Admit { role: Role, trust: bool },
    /// Refuse the guest that asked to come in.
    Refuse,
    /// Give the running session this.
    Widen(Widening),
    /// Nothing: the question went away by itself, or a widening was not
    /// agreed to.
    Nothing,
}

/// The admission question a pending row asks.
fn admission(row: &SessionSnapshot) -> Subject {
    Subject::Admission {
        label: row.label.clone(),
        device: row.device.clone(),
        role: row.role,
        kind: row.kind,
    }
}

/// The question `widening` asks of `row`'s session, or `None` when there is
/// nothing to ask: the session is not running, or already has it, or could
/// not use it.
fn widening_question(row: &SessionSnapshot, widening: Widening) -> Option<Subject> {
    if row.state != SessionStateDto::Active {
        return None;
    }
    let screen = row.kind == SessionKind::Screen;
    let worth_asking = match widening {
        Widening::Control => screen && row.role != Role::FullControl,
        // Nothing to aim at a prompt without the keyboard and mouse.
        Widening::SecureDesktopInput => screen && row.input && !row.grants.secure_desktop_input,
        Widening::TrustDevice => true,
    };
    worth_asking.then(|| Subject::Widening {
        label: row.label.clone(),
        device: row.device.clone(),
        widening,
    })
}

/// What `outcome` decides about `subject`. Only a press of a yes button
/// widens anything.
fn decide(subject: &Subject, outcome: Outcome) -> Decision {
    match (subject, outcome) {
        // Gone already: let in by its device password, or it left. Refusing
        // now could end the session the password just admitted.
        (_, Outcome::Withdrawn) => Decision::Nothing,
        (Subject::Admission { role, kind, .. }, Outcome::Pressed { button, checked }) => {
            match (button, kind) {
                // A shell is let in or not (ADR 0131): full control is the
                // only role that carries one, cut down to it by the host.
                (ID_ALLOW, SessionKind::Terminal) => Decision::Admit {
                    role: Role::FullControl,
                    trust: checked,
                },
                (ID_ALLOW, SessionKind::Screen) => Decision::Admit {
                    role: *role,
                    trust: checked,
                },
                (ID_VIEW_ONLY, SessionKind::Screen) if role.is_controller() => Decision::Admit {
                    role: Role::ViewOnly,
                    trust: checked,
                },
                _ => Decision::Refuse,
            }
        }
        // Denied, closed, unanswered, or never shown: a guest nobody let in
        // is refused, and told.
        (Subject::Admission { .. }, _) => Decision::Refuse,
        (
            Subject::Widening { widening, .. },
            Outcome::Pressed {
                button: ID_ALLOW, ..
            },
        ) => Decision::Widen(*widening),
        (Subject::Widening { .. }, _) => Decision::Nothing,
    }
}

/// A peer's `text`, fit for the dialog: no control characters, nothing that
/// turns the direction of the text around or hides in it, at most
/// [`CONSENT_DIALOG_PEER_TEXT_MAX_CHARS`] characters.
fn plain(text: &str) -> String {
    let kept: Vec<char> = text
        .chars()
        .filter(|&c| !c.is_control() && !is_invisible_format(c))
        .collect();
    let trimmed: String = kept.iter().collect::<String>().trim().to_owned();
    let count = trimmed.chars().count();
    if count <= CONSENT_DIALOG_PEER_TEXT_MAX_CHARS {
        return trimmed;
    }
    let mut cut: String = trimmed
        .chars()
        .take(CONSENT_DIALOG_PEER_TEXT_MAX_CHARS.saturating_sub(1))
        .collect();
    cut.push('…');
    cut
}

/// Characters that change how the text around them reads without being seen:
/// zero-width ones, the direction marks, embeddings, overrides and isolates,
/// and the byte-order mark.
const fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// How the guest is named: its machine's name when it said one, its label
/// otherwise.
fn who(subject: &Subject) -> String {
    subject
        .device()
        .map_or_else(|| plain(subject.label()), |device| plain(&device.name))
}

/// The operating system, as people write it.
fn system_name(os: Option<&str>, texts: &Texts) -> String {
    match os {
        Some("windows") => "Windows".to_owned(),
        Some("macos") => "macOS".to_owned(),
        Some("linux") => "Linux".to_owned(),
        Some(other) => plain(other),
        None => texts.unknown.to_owned(),
    }
}

const fn role_name(role: Role, texts: &Texts) -> &'static str {
    match role {
        Role::ViewOnly => texts.view_only,
        Role::ControlLimited => texts.control_limited,
        Role::FullControl => texts.full_control,
    }
}

/// One question, in words and buttons.
#[derive(Debug)]
struct Words {
    instruction: String,
    content: String,
    buttons: Vec<(i32, &'static str)>,
    check_box: Option<&'static str>,
}

/// What `subject` looks like in `language`.
fn words(subject: &Subject, language: Language) -> Words {
    let texts = language.texts();
    let who = who(subject);
    let facts = format!(
        "{}: {}\n{}: {}\n{}: {}",
        texts.device,
        subject
            .device()
            .map_or_else(|| texts.unknown.to_owned(), |device| plain(&device.name)),
        texts.system,
        system_name(
            subject.device().and_then(|device| device.os.as_deref()),
            texts
        ),
        texts.id,
        plain(subject.label()),
    );
    match subject {
        Subject::Admission {
            kind: SessionKind::Terminal,
            ..
        } => Words {
            instruction: texts.wants_terminal.replace("{who}", &who),
            content: format!(
                "{facts}\n{}: {}\n\n{}",
                texts.asks_for, texts.terminal, texts.about_terminal
            ),
            buttons: vec![(ID_ALLOW, texts.allow_terminal), (ID_DENY, texts.deny)],
            check_box: Some(texts.remember),
        },
        Subject::Admission {
            role,
            kind: SessionKind::Screen,
            ..
        } => {
            let mut content = format!("{facts}\n{}: {}", texts.asks_for, role_name(*role, texts));
            let buttons = match role {
                Role::FullControl => {
                    content.push_str("\n\n");
                    content.push_str(texts.about_full_control);
                    vec![
                        (ID_ALLOW, texts.allow_full),
                        (ID_VIEW_ONLY, texts.allow_view),
                        (ID_DENY, texts.deny),
                    ]
                }
                Role::ControlLimited => vec![
                    (ID_ALLOW, texts.allow),
                    (ID_VIEW_ONLY, texts.allow_view),
                    (ID_DENY, texts.deny),
                ],
                Role::ViewOnly => vec![(ID_ALLOW, texts.allow_view), (ID_DENY, texts.deny)],
            };
            Words {
                instruction: texts.wants_screen.replace("{who}", &who),
                content,
                buttons,
                check_box: Some(texts.remember),
            }
        }
        Subject::Widening { widening, .. } => {
            let (instruction, about) = match widening {
                Widening::Control => (texts.ask_control, texts.about_full_control),
                Widening::SecureDesktopInput => (
                    texts.ask_secure_desktop_input,
                    texts.about_secure_desktop_input,
                ),
                Widening::TrustDevice => (texts.ask_trust, texts.about_trust),
            };
            Words {
                instruction: instruction.replace("{who}", &who),
                content: format!("{facts}\n\n{about}"),
                buttons: vec![(ID_ALLOW, texts.allow), (ID_DENY, texts.deny)],
                check_box: None,
            }
        }
    }
}

/// Puts `subject` up in `language` and waits for how it ends.
fn show(subject: &Subject, language: Language, withdrawn: &AtomicBool) -> Outcome {
    let words = words(subject, language);
    consent_dialog::ask(
        &Question {
            title: language.texts().title,
            instruction: &words.instruction,
            content: &words.content,
            buttons: &words.buttons,
            default_button: ID_DENY,
            check_box: words.check_box,
            timeout: Duration::from_secs(CONSENT_DIALOG_TIMEOUT_SECS),
        },
        withdrawn,
    )
}

/// A question that is up.
#[derive(Debug)]
struct Asking {
    /// Whether it asks a guest in, rather than for a running session.
    admission: bool,
    /// Set when what it asks about is gone, to take it down.
    withdrawn: Arc<AtomicBool>,
}

/// The session host's questions to the person at the machine: at most one up
/// per guest, each on a thread of its own.
#[derive(Debug)]
pub struct Consents {
    actor: ActorHandle,
    runtime: tokio::runtime::Handle,
    language: Language,
    asking: Arc<Mutex<HashMap<String, Asking>>>,
}

impl Consents {
    /// Questions about `actor`'s guests, carried out on `runtime`.
    #[must_use]
    pub fn new(actor: ActorHandle, runtime: tokio::runtime::Handle) -> Self {
        Self {
            actor,
            runtime,
            language: Language::of_this_machine(),
            asking: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Brings the questions up to date with the actor's `sessions`: one for
    /// each guest asking to come in, and none left up about something that
    /// has gone.
    pub fn follow(&self, sessions: &[SessionSnapshot]) {
        let Ok(mut asking) = self.asking.lock() else {
            return;
        };
        for (label, question) in asking.iter() {
            let wanted = if question.admission {
                SessionStateDto::Pending
            } else {
                SessionStateDto::Active
            };
            if !sessions
                .iter()
                .any(|row| row.label == *label && row.state == wanted)
            {
                question.withdrawn.store(true, Ordering::SeqCst);
            }
        }
        for row in sessions
            .iter()
            .filter(|row| row.state == SessionStateDto::Pending)
        {
            if !asking.contains_key(&row.label) {
                self.put_up(&mut asking, admission(row));
            }
        }
    }

    /// Puts up the question the client asked for (ADR 0169 §5): whether
    /// `label`'s running session may have `widening`. Whether it is up; it
    /// gives nothing by itself.
    pub fn ask_to_widen(
        &self,
        sessions: &[SessionSnapshot],
        label: &str,
        widening: Widening,
    ) -> bool {
        let Some(subject) = sessions
            .iter()
            .find(|row| row.label == label)
            .and_then(|row| widening_question(row, widening))
        else {
            return false;
        };
        let Ok(mut asking) = self.asking.lock() else {
            return false;
        };
        if asking.contains_key(label) {
            return false;
        }
        self.put_up(&mut asking, subject);
        true
    }

    /// Shows `subject` on a thread of its own and carries out the answer.
    fn put_up(&self, asking: &mut HashMap<String, Asking>, subject: Subject) {
        let label = subject.label().to_owned();
        let withdrawn = Arc::new(AtomicBool::new(false));
        asking.insert(
            label.clone(),
            Asking {
                admission: subject.is_admission(),
                withdrawn: Arc::clone(&withdrawn),
            },
        );
        let actor = self.actor.clone();
        let runtime = self.runtime.clone();
        let all = Arc::clone(&self.asking);
        let language = self.language;
        let spawned = std::thread::Builder::new()
            .name("consent-dialog".to_owned())
            .spawn(move || {
                let outcome = show(&subject, language, &withdrawn);
                let decision = decide(&subject, outcome);
                tracing::info!(
                    peer = %subject.label(),
                    ?outcome,
                    ?decision,
                    "the consent dialog was answered"
                );
                runtime.block_on(carry_out(&actor, &subject, decision));
                if let Ok(mut asking) = all.lock() {
                    asking.remove(subject.label());
                }
            });
        if let Err(error) = spawned {
            tracing::error!(%error, peer = %label, "cannot put the consent dialog up");
            asking.remove(&label);
        }
    }
}

/// Does what `decision` says about `subject`, if what it was asked about is
/// still what is there: the same guest still asking for the same thing, or
/// the same session still running.
async fn carry_out(actor: &ActorHandle, subject: &Subject, decision: Decision) {
    let label = subject.label().to_owned();
    let rows = actor.status().await.unwrap_or_default();
    let row = rows.iter().find(|row| row.label == label);
    let result = match (decision, subject) {
        (Decision::Refuse, Subject::Admission { role, kind, .. }) => {
            if row.is_some_and(|row| still_asking(row, *role, *kind)) {
                actor.revoke(label).await
            } else {
                Ok(())
            }
        }
        (
            Decision::Admit { role, trust },
            Subject::Admission {
                role: asked, kind, ..
            },
        ) => {
            if row.is_some_and(|row| still_asking(row, *asked, *kind)) {
                let admitted = actor.grant(label.clone(), role).await;
                // A grant the core refused trusts nothing (ADR 0120).
                if admitted.is_ok() && trust {
                    trust_device(actor, label, subject.device()).await
                } else {
                    admitted
                }
            } else {
                Ok(())
            }
        }
        (Decision::Widen(widening), Subject::Widening { .. }) => {
            if row.is_some_and(|row| row.state == SessionStateDto::Active) {
                match widening {
                    // A re-grant of a running session keeps its id and what
                    // it was already allowed (`SessionManager::grant_as`).
                    Widening::Control => actor.grant(label, Role::FullControl).await,
                    Widening::SecureDesktopInput => {
                        actor
                            .set_grant(label, IndependentGrant::SecureDesktopInput, true)
                            .await
                    }
                    Widening::TrustDevice => trust_device(actor, label, subject.device()).await,
                }
            } else {
                Ok(())
            }
        }
        // Nothing to do — and `decide` gives an admission no widening and a
        // widening no admission, so neither pairing below ever comes.
        (Decision::Nothing, _)
        | (Decision::Admit { .. } | Decision::Refuse, Subject::Widening { .. })
        | (Decision::Widen(_), Subject::Admission { .. }) => Ok(()),
    };
    if let Err(error) = result {
        tracing::warn!(?error, peer = %subject.label(), ?decision, "the answer could not be carried out");
    }
}

/// Whether `row` is still the request the dialog asked about.
fn still_asking(row: &SessionSnapshot, role: Role, kind: SessionKind) -> bool {
    row.state == SessionStateDto::Pending && row.role == role && row.kind == kind
}

/// Saves `label`'s device in the address book if it is not there, and trusts
/// it (ADR 0120). An entry already there keeps the name the host gave it.
async fn trust_device(
    actor: &ActorHandle,
    label: String,
    device: Option<&PeerDevice>,
) -> Result<(), ActorError> {
    let saved = actor
        .address_book_list()
        .await?
        .iter()
        .any(|row| row.peer_label == label);
    if !saved {
        let name = device.map_or_else(|| label.clone(), |device| device.name.clone());
        actor
            .address_book_upsert(label.clone(), name, Vec::new(), String::new())
            .await?;
    }
    actor.address_book_set_trusted(label, true).await
}

#[cfg(test)]
mod tests {
    use lumepeer_core::consent::Grants;

    use super::*;

    fn row(state: SessionStateDto, role: Role, kind: SessionKind) -> SessionSnapshot {
        SessionSnapshot {
            label: "peer-1".to_owned(),
            device: Some(PeerDevice {
                name: "Laptop".to_owned(),
                os: Some("windows".to_owned()),
            }),
            state,
            role,
            input: state == SessionStateDto::Active && role == Role::FullControl,
            grants: if state == SessionStateDto::Active {
                Grants::from_role(role)
            } else {
                Grants::default()
            },
            recording_active: false,
            record_request: false,
            secure_desktop_active: false,
            terminal_active: false,
            kind,
            chat_unread: false,
        }
    }

    fn pressed(button: i32, checked: bool) -> Outcome {
        Outcome::Pressed { button, checked }
    }

    /// A guest is let in only by a yes; every other ending refuses it — and
    /// a question that went away by itself refuses nothing, because the
    /// device password may just have let that guest in.
    #[test]
    fn only_a_yes_admits_and_a_withdrawn_question_refuses_nothing() {
        let asking = admission(&row(
            SessionStateDto::Pending,
            Role::FullControl,
            SessionKind::Screen,
        ));
        assert_eq!(
            decide(&asking, pressed(ID_ALLOW, false)),
            Decision::Admit {
                role: Role::FullControl,
                trust: false
            }
        );
        assert_eq!(
            decide(&asking, pressed(ID_VIEW_ONLY, true)),
            Decision::Admit {
                role: Role::ViewOnly,
                trust: true
            }
        );
        for ending in [
            pressed(ID_DENY, true),
            pressed(4242, false),
            Outcome::Dismissed,
            Outcome::Expired,
            Outcome::Failed,
        ] {
            assert_eq!(decide(&asking, ending), Decision::Refuse, "{ending:?}");
        }
        assert_eq!(decide(&asking, Outcome::Withdrawn), Decision::Nothing);
    }

    /// "View only" is an answer to a request for control, not a way to let
    /// a shell in; a shell is let in with full control (ADR 0131).
    #[test]
    fn a_terminal_is_let_in_whole_or_not_at_all() {
        let shell = admission(&row(
            SessionStateDto::Pending,
            Role::FullControl,
            SessionKind::Terminal,
        ));
        assert_eq!(
            decide(&shell, pressed(ID_ALLOW, false)),
            Decision::Admit {
                role: Role::FullControl,
                trust: false
            }
        );
        assert_eq!(
            decide(&shell, pressed(ID_VIEW_ONLY, false)),
            Decision::Refuse
        );
        let viewer = admission(&row(
            SessionStateDto::Pending,
            Role::ViewOnly,
            SessionKind::Screen,
        ));
        assert_eq!(
            decide(&viewer, pressed(ID_VIEW_ONLY, false)),
            Decision::Refuse,
            "no view-only button is shown to a viewer"
        );
    }

    /// Pack 04: a widening happens only on the person's yes. Asking — the
    /// one thing the client's channel can do about it — changes nothing, and
    /// no other ending of the question does either.
    #[test]
    fn a_widening_happens_only_on_a_yes() {
        let watching = row(SessionStateDto::Active, Role::ViewOnly, SessionKind::Screen);
        let Some(control) = widening_question(&watching, Widening::Control) else {
            panic!("a watching guest can be asked about control");
        };
        assert_eq!(
            decide(&control, pressed(ID_ALLOW, false)),
            Decision::Widen(Widening::Control)
        );
        for ending in [
            pressed(ID_DENY, false),
            pressed(ID_VIEW_ONLY, false),
            Outcome::Dismissed,
            Outcome::Expired,
            Outcome::Withdrawn,
            Outcome::Failed,
        ] {
            assert_eq!(decide(&control, ending), Decision::Nothing, "{ending:?}");
        }
    }

    /// Nothing is asked that would not change anything, nor about a session
    /// that is not running.
    #[test]
    fn only_a_question_worth_asking_is_put_up() {
        let full = row(
            SessionStateDto::Active,
            Role::FullControl,
            SessionKind::Screen,
        );
        assert_eq!(widening_question(&full, Widening::Control), None);
        assert_eq!(
            widening_question(&full, Widening::SecureDesktopInput),
            None,
            "full control already carries it (ADR 0061)"
        );
        let mut lowered = full.clone();
        lowered.grants.secure_desktop_input = false;
        assert!(widening_question(&lowered, Widening::SecureDesktopInput).is_some());

        let watching = row(SessionStateDto::Active, Role::ViewOnly, SessionKind::Screen);
        assert_eq!(
            widening_question(&watching, Widening::SecureDesktopInput),
            None,
            "nothing to aim at a prompt without input"
        );
        assert!(widening_question(&watching, Widening::TrustDevice).is_some());

        let shell = row(
            SessionStateDto::Active,
            Role::FullControl,
            SessionKind::Terminal,
        );
        assert_eq!(widening_question(&shell, Widening::Control), None);

        let pending = row(
            SessionStateDto::Pending,
            Role::ViewOnly,
            SessionKind::Screen,
        );
        for widening in [
            Widening::Control,
            Widening::SecureDesktopInput,
            Widening::TrustDevice,
        ] {
            assert_eq!(widening_question(&pending, widening), None);
        }
    }

    /// A peer's string is shown as text: no control characters, no
    /// direction tricks, no markup made into anything, and bounded.
    #[test]
    fn a_peer_string_is_plain_and_bounded() {
        assert_eq!(plain("Lap\u{202E}top\r\nX\u{200B}"), "LaptopX");
        assert_eq!(
            plain("<a href=\"https://x\">click</a>"),
            "<a href=\"https://x\">click</a>",
            "kept as the characters it is; the window has no links"
        );
        let long = "я".repeat(CONSENT_DIALOG_PEER_TEXT_MAX_CHARS * 2);
        let cut = plain(&long);
        assert_eq!(cut.chars().count(), CONSENT_DIALOG_PEER_TEXT_MAX_CHARS);
        assert!(cut.ends_with('…'));
    }

    /// Deny is offered every time, the check box only when a guest comes in,
    /// and "view only" only to a guest that asked for more — in both
    /// languages, with the guest's name in the question.
    #[test]
    fn the_words_and_buttons_fit_the_question() {
        for language in [Language::En, Language::Ru] {
            let texts = language.texts();
            let control = admission(&row(
                SessionStateDto::Pending,
                Role::FullControl,
                SessionKind::Screen,
            ));
            let asked = words(&control, language);
            assert!(asked.instruction.contains("Laptop"));
            assert!(asked.content.contains("Windows"));
            assert!(asked.content.contains(texts.about_full_control));
            assert_eq!(
                asked.buttons.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                [ID_ALLOW, ID_VIEW_ONLY, ID_DENY]
            );
            assert_eq!(asked.check_box, Some(texts.remember));

            let viewer = admission(&row(
                SessionStateDto::Pending,
                Role::ViewOnly,
                SessionKind::Screen,
            ));
            assert_eq!(
                words(&viewer, language)
                    .buttons
                    .iter()
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>(),
                [ID_ALLOW, ID_DENY]
            );

            let Some(trust) = widening_question(
                &row(SessionStateDto::Active, Role::ViewOnly, SessionKind::Screen),
                Widening::TrustDevice,
            ) else {
                panic!("trust can be asked about");
            };
            let asked = words(&trust, language);
            assert_eq!(asked.check_box, None);
            assert_eq!(
                asked.buttons.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                [ID_ALLOW, ID_DENY]
            );
        }
    }

    /// The Russian buttons are the webview's consent window's
    /// (`apps/desktop/src/locales/ru.ts`), so the person reads the same words
    /// in both.
    #[test]
    fn the_buttons_say_what_the_webview_says() {
        assert_eq!(RU.deny, "Отклонить");
        assert_eq!(RU.allow_view, "Разрешить только просмотр");
        assert_eq!(RU.allow_full, "Разрешить полный доступ");
        assert_eq!(RU.allow_terminal, "Разрешить терминал");
        assert_eq!(EN.deny, "Deny");
        assert_eq!(EN.allow_view, "Allow view only");
        assert_eq!(EN.allow_full, "Allow full control");
        assert_eq!(EN.allow_terminal, "Allow terminal");
    }
}
