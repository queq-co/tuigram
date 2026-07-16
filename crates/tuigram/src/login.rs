//! The login screens (#86): `TDLib`'s auth state machine
//! ([`docs/login-flow.md`](../../../docs/login-flow.md)) rendered as one UI screen
//! per waiting state, replacing the stdin harness for the eventual product.
//!
//! Core's [`AuthState`] is the authority — it is a *total* projection of `TDLib`'s
//! authorization states — so the screen set here is closed and known: a screen for
//! each state the driver acts on (phone number, login code, masked 2FA password, a
//! scannable QR for `WaitOtherDeviceConfirmation`, email address, email code,
//! new-user registration, and the premium-purchase **dead end**), plus the passive
//! status screens (`WaitTdlibParameters`, `Ready`, `Closed`). [`LoginView`] holds
//! the current state and the field(s) the active screen edits; [`LoginView::on_key`]
//! maps a keystroke onto a [`LoginOutcome`], and a [`LoginOutcome::Submit`] carries
//! the [`LoginAnswer`] the owning loop dispatches through the core auth seam.
//!
//! Secrets follow the same rules as the harness: the login code and 2FA password
//! are taken **by value** out of the field on submit (nothing here retains them),
//! the password is rendered masked, and every transition resets the fields so an
//! entry never carries from one screen to the next.
//!
//! Phase 6 (#111) mounts these screens: [`run_login`] drives the TUI login loop
//! over the real bridge — feeding `TDLib`'s `updateAuthorizationState` into
//! [`LoginView::set_state`], dispatching each [`LoginAnswer`] through the core
//! [`Login`] / [`AuthRequests`](tuigram_core::AuthRequests) seam, and returning
//! once login reaches a terminal state so `main` gates the three-pane UI behind
//! `Ready`. The pure view-model is still exercised headlessly (unit tests for the
//! input handling and the answer→request mapping, snapshot tests for the screens);
//! the async loop itself is a real-TDLib lifecycle path verified via the REPL
//! (#123), not in CI.

use std::io;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use qrcodegen::{QrCode, QrCodeEcc};
use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use tokio_stream::StreamExt;

use tuigram_core::enums::Update;
use tuigram_core::types::Error as TdError;
use tuigram_core::{AuthRequests, AuthState, Bridge, Login, TgClient};

use crate::terminal::TerminalGuard;
use crate::textinput::TextInput;
use crate::ui::{hint_line, input_line};

/// An answer to a waiting login screen — the request the owning loop dispatches
/// through core's auth seam (`Login::*`) when a screen is submitted. Secret-bearing
/// variants own their value (moved out of the field on submit), so nothing in the
/// view-model retains them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginAnswer {
    /// The phone number to authenticate with (international format).
    Phone(String),
    /// Use QR login instead of a phone number — the alternative answer at the phone
    /// screen, which moves `TDLib` to `WaitOtherDeviceConfirmation`.
    RequestQr,
    /// The login code delivered to the account.
    Code(String),
    /// The 2FA password (taken by value; never persisted — it is the account
    /// password, not ours to keep).
    Password(String),
    /// The user's email address, for email login.
    Email(String),
    /// The code delivered to that email address.
    EmailCode(String),
    /// A new account's name, submitted after accepting the terms of service.
    Register {
        first_name: String,
        last_name: String,
    },
}

/// What a keystroke did to a login screen, mirroring the app's dirty-flag model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginOutcome {
    /// Nothing changed (an unbound key, or an incomplete submit); no repaint.
    Unchanged,
    /// Visible state changed (a field edit, a focus move); repaint.
    Dirty,
    /// The screen was submitted — dispatch this answer through the core auth seam.
    Submit(LoginAnswer),
    /// Tear down and exit (Ctrl-C, and the dead-end screens' only way out).
    Quit,
}

/// Which field the new-user registration screen is editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RegField {
    /// The first name (required).
    #[default]
    First,
    /// The last name (optional).
    Last,
    /// The terms-of-service acceptance checkbox.
    Accept,
}

/// The new-user registration screen's state: the two name fields, which one is
/// focused, and whether the terms of service have been accepted. `TDLib` needs a
/// first name and an accepted `ToS` before [`LoginAnswer::Register`] can be sent.
#[derive(Debug, Clone, Default)]
pub struct RegisterForm {
    first: TextInput,
    last: TextInput,
    field: RegField,
    accepted: bool,
}

impl RegisterForm {
    /// The first-name field.
    pub fn first(&self) -> &TextInput {
        &self.first
    }

    /// The last-name field.
    pub fn last(&self) -> &TextInput {
        &self.last
    }

    /// Which field is focused.
    pub fn field(&self) -> RegField {
        self.field
    }

    /// Whether the terms of service have been accepted.
    pub fn accepted(&self) -> bool {
        self.accepted
    }

    /// The focused name field, or `None` when the acceptance checkbox is focused.
    fn active_mut(&mut self) -> Option<&mut TextInput> {
        match self.field {
            RegField::First => Some(&mut self.first),
            RegField::Last => Some(&mut self.last),
            RegField::Accept => None,
        }
    }

    /// Advance the focus First → Last → Accept → First (Tab).
    fn cycle(&mut self) {
        self.field = match self.field {
            RegField::First => RegField::Last,
            RegField::Last => RegField::Accept,
            RegField::Accept => RegField::First,
        };
    }

    /// Whether the form can be submitted: the terms accepted and a first name given
    /// (`TDLib` requires the first name; the last name is optional).
    fn is_submittable(&self) -> bool {
        self.accepted && !self.first.text().trim().is_empty()
    }
}

/// The login flow's view-model: the current [`AuthState`] and the field(s) the
/// active screen edits. Built fresh at `WaitTdlibParameters`; the owning loop moves
/// it through the states with [`set_state`](Self::set_state) as `TDLib` reports them.
#[derive(Debug, Clone)]
pub struct LoginView {
    state: AuthState,
    /// The single editable field shared by the phone/code/password/email screens —
    /// only one is on screen at a time. Reset on every transition.
    input: TextInput,
    /// The registration screen's multi-field form.
    register: RegisterForm,
    /// A rejected-submit message to show under the field — a `TDLib` error *code*
    /// (e.g. `PHONE_CODE_INVALID`), never the user's input. `TDLib` does not emit a
    /// new state when it rejects an answer (it stays on the same waiting screen),
    /// so this is how a wrong code/password is surfaced before the retry. Cleared
    /// on every transition and at the start of each new submit.
    error: Option<String>,
}

impl Default for LoginView {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginView {
    /// A fresh login at the first state, every field empty.
    #[must_use]
    pub fn new() -> Self {
        Self::from_state(AuthState::WaitTdlibParameters)
    }

    /// A login sitting on `state` with empty fields — the seam Phase 6 (and the
    /// tests) use to render a specific screen.
    #[must_use]
    pub fn from_state(state: AuthState) -> Self {
        Self {
            state,
            input: TextInput::default(),
            register: RegisterForm::default(),
            error: None,
        }
    }

    /// The current auth state — the screen to render.
    #[must_use]
    pub fn state(&self) -> &AuthState {
        &self.state
    }

    /// The current rejected-submit message, if any — shown under the field.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Set or clear the rejected-submit message. The owning loop clears it
    /// (`None`) before each submit and sets it (the `TDLib` error code) when a
    /// submit is rejected; a transition clears it via [`set_state`](Self::set_state).
    pub fn set_error(&mut self, error: Option<String>) {
        self.error = error;
    }

    /// The single-field screens' editable input (phone/code/password/email).
    #[must_use]
    pub fn input(&self) -> &TextInput {
        &self.input
    }

    /// The registration screen's form.
    #[must_use]
    pub fn register(&self) -> &RegisterForm {
        &self.register
    }

    /// Move to a new auth state on a `TDLib` `updateAuthorizationState`, clearing the
    /// fields so a previous screen's entry (including any secret) never carries
    /// over to the next.
    pub fn set_state(&mut self, state: AuthState) {
        self.state = state;
        self.input = TextInput::default();
        self.register = RegisterForm::default();
        self.error = None;
    }

    /// Map a keystroke onto a [`LoginOutcome`] for the active screen. Pure over the
    /// view-model: edits mutate the field in place, a submit moves the entered value
    /// out by value, and `Ctrl-C` always quits so no screen can trap the user.
    pub fn on_key(&mut self, key: &KeyEvent) -> LoginOutcome {
        if key.kind == KeyEventKind::Release {
            return LoginOutcome::Unchanged;
        }
        if is_quit(key) {
            return LoginOutcome::Quit;
        }
        match category(&self.state) {
            Category::Phone => self.on_phone_key(key),
            Category::Code => self.edit_single(key, LoginAnswer::Code),
            Category::Password => self.edit_single(key, LoginAnswer::Password),
            Category::Email => self.edit_single(key, LoginAnswer::Email),
            Category::EmailCode => self.edit_single(key, LoginAnswer::EmailCode),
            Category::Register => self.on_register_key(key),
            // The QR, premium dead-end, and status screens take no input; the user
            // waits for the next update or quits with Ctrl-C (handled above).
            Category::Passive => LoginOutcome::Unchanged,
        }
    }

    /// The phone screen: edit the number, or `Tab` to switch to QR login instead.
    fn on_phone_key(&mut self, key: &KeyEvent) -> LoginOutcome {
        if key.code == KeyCode::Tab {
            return LoginOutcome::Submit(LoginAnswer::RequestQr);
        }
        self.edit_single(key, LoginAnswer::Phone)
    }

    /// A one-field screen: editing keys mutate [`input`](Self::input); `Enter`
    /// submits its value through `make` (a no-op while the field is blank).
    fn edit_single(&mut self, key: &KeyEvent, make: fn(String) -> LoginAnswer) -> LoginOutcome {
        match key.code {
            KeyCode::Enter => {
                if self.input.text().trim().is_empty() {
                    LoginOutcome::Unchanged
                } else {
                    LoginOutcome::Submit(make(self.input.take()))
                }
            }
            _ => edit_field(&mut self.input, key),
        }
    }

    /// The registration screen: `Tab` cycles the fields, `Space` toggles acceptance
    /// while the checkbox is focused, the name fields edit as usual, and `Enter`
    /// creates the account once the terms are accepted and a first name is given.
    fn on_register_key(&mut self, key: &KeyEvent) -> LoginOutcome {
        match key.code {
            KeyCode::Tab => {
                self.register.cycle();
                LoginOutcome::Dirty
            }
            KeyCode::Char(' ') if self.register.field == RegField::Accept => {
                self.register.accepted = !self.register.accepted;
                LoginOutcome::Dirty
            }
            KeyCode::Enter => {
                if self.register.is_submittable() {
                    LoginOutcome::Submit(LoginAnswer::Register {
                        first_name: self.register.first.take(),
                        last_name: self.register.last.take(),
                    })
                } else {
                    LoginOutcome::Unchanged
                }
            }
            _ => match self.register.active_mut() {
                Some(input) => edit_field(input, key),
                None => LoginOutcome::Unchanged,
            },
        }
    }
}

/// The broad input behaviour of a screen, derived from its [`AuthState`] — what
/// [`LoginView::on_key`] routes on without re-borrowing the state in each arm.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Category {
    Phone,
    Code,
    Password,
    Email,
    EmailCode,
    Register,
    /// QR, premium dead-end, and the passive status screens — no editable field.
    Passive,
}

/// Classify a state by how its screen takes input.
fn category(state: &AuthState) -> Category {
    match state {
        AuthState::WaitPhoneNumber => Category::Phone,
        AuthState::WaitCode => Category::Code,
        AuthState::WaitPassword { .. } => Category::Password,
        AuthState::WaitEmailAddress => Category::Email,
        AuthState::WaitEmailCode { .. } => Category::EmailCode,
        AuthState::WaitRegistration { .. } => Category::Register,
        AuthState::WaitTdlibParameters
        | AuthState::WaitOtherDeviceConfirmation { .. }
        | AuthState::WaitPremiumPurchase { .. }
        | AuthState::Ready
        | AuthState::Closed => Category::Passive,
    }
}

/// Apply a text-editing key (Backspace, the cursor moves, or a printable
/// character) to `input`, returning whether it changed anything.
fn edit_field(input: &mut TextInput, key: &KeyEvent) -> LoginOutcome {
    match key.code {
        KeyCode::Backspace => input.backspace(),
        KeyCode::Left => input.move_left(),
        KeyCode::Right => input.move_right(),
        KeyCode::Home => input.move_home(),
        KeyCode::End => input.move_end(),
        _ => match printable(key) {
            Some(c) => input.insert(c),
            None => return LoginOutcome::Unchanged,
        },
    }
    LoginOutcome::Dirty
}

/// Whether the key is `Ctrl-C` — the always-available quit on every screen.
fn is_quit(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
}

/// The character a key would insert, or `None` for a non-printable key or a
/// Ctrl/Alt chord (a command, not text).
fn printable(key: &KeyEvent) -> Option<char> {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL.union(KeyModifiers::ALT))
    {
        return None;
    }
    match key.code {
        KeyCode::Char(c) => Some(c),
        _ => None,
    }
}

// --- driver ----------------------------------------------------------------

/// How the TUI login loop ended, returned by [`run_login`] for `main` to gate on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginEnd {
    /// Login reached `Ready`: hand the bridge to the facade and run the TUI.
    Ready,
    /// The user quit (Ctrl-C, or stdin closed) before logging in.
    Quit,
    /// `TDLib` closed the session (logged out / shutting down) before it was ready.
    Closed,
}

/// Drive login inside the TUI: render one screen per waiting [`AuthState`], feed
/// each `updateAuthorizationState` into the [`LoginView`], and dispatch every
/// submitted [`LoginAnswer`] through the core [`Login`] seam — until login reaches
/// a terminal state (or the user quits), which is returned so `main` gates the
/// three-pane UI behind `Ready`.
///
/// The bridge is already initialized (`setTdlibParameters` sent in
/// [`bootstrap`](crate::bootstrap)); this only answers the *login* states. It
/// borrows the bridge for the duration, so once it returns the borrow is released
/// and the owned bridge can move to
/// [`Client::start`](tuigram_core::Client::start).
pub async fn run_login(guard: &mut TerminalGuard, bridge: &Bridge) -> io::Result<LoginEnd> {
    let login = Login::new(bridge);
    let mut view = LoginView::new();
    let mut updates = bridge.updates();
    let mut input = EventStream::new();

    // Prime the current state: TDLib's first login update may have fired before we
    // subscribed, or a persisted session may already be `Ready`. Every subsequent
    // transition arrives on the update stream below.
    match bridge.authorization_state().await {
        Ok(state) => view.set_state(AuthState::from_tdlib(&state)),
        // Failing the state query this early means the bridge is already gone;
        // treat it as a closed session rather than wedge on a screen that can't
        // advance.
        Err(_) => return Ok(LoginEnd::Closed),
    }
    if let Some(end) = terminal_end(view.state()) {
        return Ok(end);
    }

    let mut dirty = true;
    loop {
        if dirty {
            guard.terminal_mut().draw(|frame| login_ui(frame, &view))?;
            dirty = false;
        }

        tokio::select! {
            // Keystrokes drive the active screen.
            maybe_event = input.next() => match maybe_event {
                Some(Ok(Event::Key(key))) => match view.on_key(&key) {
                    LoginOutcome::Unchanged => {}
                    LoginOutcome::Dirty => dirty = true,
                    LoginOutcome::Submit(answer) => {
                        // Clear any prior rejection, then answer. A rejected submit
                        // surfaces the TDLib code under the field; TDLib emits no new
                        // state for it, so the same screen stays up for the retry.
                        view.set_error(None);
                        if let Err(e) = submit_answer(&login, answer).await {
                            view.set_error(Some(error_line(&e)));
                        }
                        dirty = true;
                    }
                    LoginOutcome::Quit => return Ok(LoginEnd::Quit),
                },
                // A resize must repaint against the new viewport.
                Some(Ok(Event::Resize(_, _))) => dirty = true,
                // Some(Err(_)): a transient read error; ignore and re-enter the loop.
                Some(Ok(_) | Err(_)) => {}
                // stdin closed: quit rather than spin on a dead stream.
                None => return Ok(LoginEnd::Quit),
            },
            // TDLib drives the state machine: each transition swaps the screen.
            maybe_update = updates.next() => match maybe_update {
                Some(Update::AuthorizationState(u)) => {
                    view.set_state(AuthState::from_tdlib(&u.authorization_state));
                    if let Some(end) = terminal_end(view.state()) {
                        return Ok(end);
                    }
                    dirty = true;
                }
                // Other updates don't matter until login is done; the router folds
                // them once the facade starts.
                Some(_) => {}
                // The bridge closed its update stream: the session is gone.
                None => return Ok(LoginEnd::Closed),
            }
        }
    }
}

/// Map a terminal [`AuthState`] onto the [`LoginEnd`] the loop returns, or `None`
/// while login is still in progress.
fn terminal_end(state: &AuthState) -> Option<LoginEnd> {
    match state {
        AuthState::Ready => Some(LoginEnd::Ready),
        AuthState::Closed => Some(LoginEnd::Closed),
        _ => None,
    }
}

/// Dispatch one submitted [`LoginAnswer`] through the core [`Login`] seam. Pure
/// routing — each answer maps to exactly one request — and generic over the
/// request seam so it is testable against a spy with no live `tdjson`. The secret
/// answers (code, password, email code) are moved straight into their request.
async fn submit_answer<C: AuthRequests>(
    login: &Login<'_, C>,
    answer: LoginAnswer,
) -> Result<(), TdError> {
    match answer {
        LoginAnswer::Phone(phone) => login.submit_phone_number(phone).await,
        LoginAnswer::RequestQr => login.request_qr_code().await,
        LoginAnswer::Code(code) => login.submit_code(code).await,
        LoginAnswer::Password(password) => login.submit_password(password).await,
        LoginAnswer::Email(email) => login.submit_email_address(email).await,
        LoginAnswer::EmailCode(code) => login.submit_email_code(code).await,
        LoginAnswer::Register {
            first_name,
            last_name,
        } => login.register(first_name, last_name).await,
    }
}

/// The single-line message for a rejected submit: `TDLib`'s error text (a stable
/// phrase like `PHONE_CODE_INVALID`), falling back to the numeric code when the
/// text is blank. Never the user's input — `TDLib` names the rejection, not the
/// value.
fn error_line(e: &TdError) -> String {
    if e.message.is_empty() {
        format!("login error (code {})", e.code)
    } else {
        e.message.clone()
    }
}

// --- rendering -------------------------------------------------------------

/// Render the active login screen full-frame: a bordered, centred card with the
/// screen's heading, its field(s) or message, and a key hint along the bottom.
pub fn login_ui(frame: &mut Frame, view: &LoginView) {
    let mut lines = login_lines(view);
    if let Some(error) = view.error() {
        lines.push(Line::from(""));
        lines.push(error_line_widget(error));
    }
    lines.push(Line::from(""));
    lines.push(hint_line(login_hint(view.state())));

    let block = Block::bordered()
        .title(" tuigram — Sign in ")
        .title_alignment(Alignment::Center);
    let card = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .block(block);
    frame.render_widget(Clear, frame.area());
    frame.render_widget(card, frame.area());
}

/// The body lines for the active screen (heading + fields/message), before the
/// trailing key hint.
fn login_lines(view: &LoginView) -> Vec<Line<'static>> {
    match view.state() {
        AuthState::WaitTdlibParameters => vec![
            heading("Connecting…"),
            Line::from(""),
            plain("Starting up and talking to Telegram."),
        ],
        AuthState::WaitPhoneNumber => {
            let mut lines = vec![heading("Sign in to Telegram"), Line::from("")];
            lines.extend(field_lines(
                "Phone number",
                view.input(),
                true,
                false,
                "+1 555 123 4567",
            ));
            lines
        }
        AuthState::WaitCode => {
            let mut lines = vec![
                heading("Enter your login code"),
                dim("Telegram sent a code to your account."),
                Line::from(""),
            ];
            lines.extend(field_lines("Code", view.input(), true, false, "12345"));
            lines
        }
        AuthState::WaitPassword { hint } => {
            let mut lines = vec![heading("Two-step verification")];
            if hint.is_empty() {
                lines.push(dim("Enter your cloud password."));
            } else {
                lines.push(dim(format!("Hint: {hint}")));
            }
            lines.push(Line::from(""));
            lines.extend(field_lines(
                "Password",
                view.input(),
                true,
                true,
                "(your 2FA password)",
            ));
            lines
        }
        AuthState::WaitOtherDeviceConfirmation { link } => {
            let mut lines = vec![
                heading("Scan to sign in"),
                Line::from(""),
                plain("On a phone already signed in to Telegram, open"),
                plain("Settings → Devices → Link Desktop Device and scan this code:"),
                Line::from(""),
            ];
            lines.extend(qr_lines(link));
            lines.push(Line::from(""));
            lines.push(dim("The code is single-use and expires shortly."));
            lines
        }
        AuthState::WaitEmailAddress => {
            let mut lines = vec![
                heading("Add your email address"),
                dim("Telegram will send a confirmation code to it."),
                Line::from(""),
            ];
            lines.extend(field_lines(
                "Email",
                view.input(),
                true,
                false,
                "you@example.com",
            ));
            lines
        }
        AuthState::WaitEmailCode { email_pattern } => {
            let mut lines = vec![
                heading("Enter the email code"),
                dim(format!("We sent a code to {email_pattern}.")),
                Line::from(""),
            ];
            lines.extend(field_lines("Code", view.input(), true, false, "12345"));
            lines
        }
        AuthState::WaitRegistration { terms_of_service } => register_lines(view, terms_of_service),
        AuthState::WaitPremiumPurchase { store_product_id } => vec![
            heading("Telegram Premium required"),
            Line::from(""),
            plain("This account needs an active Telegram Premium subscription"),
            plain("to sign in. Premium is an in-app App Store / Google Play"),
            plain("purchase, which a terminal client can't complete."),
            Line::from(""),
            plain("Buy Premium in the Telegram mobile app, then sign in again."),
            Line::from(""),
            dim(format!("store product: {store_product_id}")),
        ],
        AuthState::Ready => vec![
            heading("Signed in"),
            Line::from(""),
            plain("You're in — loading your chats…"),
        ],
        AuthState::Closed => vec![
            heading("Session closed"),
            Line::from(""),
            plain("You've been logged out."),
        ],
    }
}

/// The registration screen: the terms of service, the two name fields, and the
/// acceptance checkbox (reverse-video while focused).
fn register_lines(view: &LoginView, terms: &str) -> Vec<Line<'static>> {
    let register = view.register();
    let mut lines = vec![heading("Create your account"), Line::from("")];
    if !terms.is_empty() {
        lines.push(dim("Terms of Service:"));
        for line in terms.split('\n') {
            lines.push(plain(line.to_owned()));
        }
        lines.push(Line::from(""));
    }
    lines.extend(field_lines(
        "First name",
        register.first(),
        register.field() == RegField::First,
        false,
        "(required)",
    ));
    lines.extend(field_lines(
        "Last name",
        register.last(),
        register.field() == RegField::Last,
        false,
        "(optional)",
    ));

    let checkbox = if register.accepted() { "[x]" } else { "[ ]" };
    let label = format!("{checkbox} I accept the Terms of Service");
    let style = if register.field() == RegField::Accept {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new()
    };
    lines.push(Line::from(Span::styled(label, style)));
    lines
}

/// A labelled field as two lines: a bold label over its value. The focused field
/// shows the caret (via [`input_line`]); an empty unfocused field shows a dim
/// placeholder. A masked field renders its characters as bullets — never the
/// secret itself.
fn field_lines(
    label: &str,
    input: &TextInput,
    focused: bool,
    masked: bool,
    placeholder: &str,
) -> Vec<Line<'static>> {
    let value = if focused {
        let shown = if masked {
            mask(input)
        } else {
            input.text().to_owned()
        };
        input_line(&shown, input.cursor())
    } else if input.is_empty() {
        dim(placeholder.to_owned())
    } else if masked {
        plain(mask(input))
    } else {
        plain(input.text().to_owned())
    };
    vec![heading(label), value]
}

/// A field's text masked to bullets, one per character — what a password renders as.
fn mask(input: &TextInput) -> String {
    "•".repeat(input.text().chars().count())
}

/// The device-confirmation link drawn as a scannable QR code: each text cell packs
/// two vertical modules as a half-block, painted black-on-white so dark modules
/// scan against a light quiet zone regardless of the terminal's theme. A two-module
/// quiet zone frames it. Falls back to a note if the link is somehow too long to
/// encode.
fn qr_lines(link: &str) -> Vec<Line<'static>> {
    let Ok(code) = QrCode::encode_text(link, QrCodeEcc::Low) else {
        return vec![dim("(could not render QR code)")];
    };
    let size = code.size();
    let quiet = 2;
    let style = Style::new().fg(Color::Black).bg(Color::White);
    // A module is dark only inside the matrix; the quiet zone around it is light.
    let dark = |x: i32, y: i32| x >= 0 && x < size && y >= 0 && y < size && code.get_module(x, y);

    let mut lines = Vec::new();
    let mut y = -quiet;
    while y < size + quiet {
        let row: String = (-quiet..size + quiet)
            .map(|x| match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            })
            .collect();
        lines.push(Line::from(Span::styled(row, style)));
        y += 2;
    }
    lines
}

/// The key hint for a screen, shown dim along the bottom.
fn login_hint(state: &AuthState) -> &'static str {
    match state {
        AuthState::WaitPhoneNumber => "Enter submit · Tab use QR code · Ctrl-C quit",
        AuthState::WaitCode
        | AuthState::WaitPassword { .. }
        | AuthState::WaitEmailAddress
        | AuthState::WaitEmailCode { .. } => "Enter submit · Ctrl-C quit",
        AuthState::WaitRegistration { .. } => {
            "Tab next field · Space toggle accept · Enter create · Ctrl-C quit"
        }
        AuthState::WaitOtherDeviceConfirmation { .. } => "Waiting for confirmation… · Ctrl-C quit",
        AuthState::WaitPremiumPurchase { .. }
        | AuthState::WaitTdlibParameters
        | AuthState::Ready
        | AuthState::Closed => "Ctrl-C quit",
    }
}

/// A bold heading line.
fn heading(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_owned(),
        Style::new().add_modifier(Modifier::BOLD),
    ))
}

/// A plain line.
fn plain(text: impl Into<String>) -> Line<'static> {
    Line::from(text.into())
}

/// A dim line, for secondary copy.
fn dim(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        text.into(),
        Style::new().add_modifier(Modifier::DIM),
    ))
}

/// The rejected-submit message, drawn in red with the toast error marker so a
/// wrong code/password reads as a failure to act on, not as copy.
fn error_line_widget(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("✗ {text}"),
        Style::new().fg(Color::Red),
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests: panicking on a broken assumption is the point
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::{Buffer, Cell};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// Type each character of `text` into `view` through `on_key`.
    fn type_str(view: &mut LoginView, text: &str) {
        for c in text.chars() {
            view.on_key(&key(KeyCode::Char(c)));
        }
    }

    /// Render a login screen into an in-memory buffer at a fixed size.
    fn render(view: &LoginView, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| login_ui(frame, view)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// Whole-buffer text, for substring assertions on rendered content.
    fn flatten(buffer: &Buffer) -> String {
        buffer.content().iter().map(Cell::symbol).collect()
    }

    // --- input handling ---

    #[test]
    fn ctrl_c_quits_from_any_screen() {
        for state in [
            AuthState::WaitPhoneNumber,
            AuthState::WaitCode,
            AuthState::WaitPassword {
                hint: String::new(),
            },
            AuthState::WaitRegistration {
                terms_of_service: "terms".to_owned(),
            },
            AuthState::WaitPremiumPurchase {
                store_product_id: "p".to_owned(),
            },
            AuthState::WaitOtherDeviceConfirmation {
                link: "tg://login?token=x".to_owned(),
            },
        ] {
            let mut view = LoginView::from_state(state);
            assert_eq!(view.on_key(&ctrl('c')), LoginOutcome::Quit);
        }
    }

    #[test]
    fn key_release_is_ignored() {
        let mut view = LoginView::from_state(AuthState::WaitPhoneNumber);
        let mut release = key(KeyCode::Char('1'));
        release.kind = KeyEventKind::Release;
        assert_eq!(view.on_key(&release), LoginOutcome::Unchanged);
        assert!(view.input().is_empty());
    }

    #[test]
    fn the_phone_screen_types_then_submits_the_number() {
        let mut view = LoginView::from_state(AuthState::WaitPhoneNumber);
        type_str(&mut view, "+15551234567");
        assert_eq!(view.input().text(), "+15551234567");
        assert_eq!(
            view.on_key(&key(KeyCode::Enter)),
            LoginOutcome::Submit(LoginAnswer::Phone("+15551234567".to_owned()))
        );
        // The value was moved out by the submit — nothing retained.
        assert!(view.input().is_empty());
    }

    #[test]
    fn an_empty_submit_does_nothing() {
        let mut view = LoginView::from_state(AuthState::WaitCode);
        assert_eq!(view.on_key(&key(KeyCode::Enter)), LoginOutcome::Unchanged);
    }

    #[test]
    fn tab_on_the_phone_screen_requests_qr_login() {
        let mut view = LoginView::from_state(AuthState::WaitPhoneNumber);
        assert_eq!(
            view.on_key(&key(KeyCode::Tab)),
            LoginOutcome::Submit(LoginAnswer::RequestQr)
        );
    }

    #[test]
    fn the_password_screen_submits_by_value() {
        let mut view = LoginView::from_state(AuthState::WaitPassword {
            hint: "a pet".to_owned(),
        });
        type_str(&mut view, "hunter2");
        assert_eq!(
            view.on_key(&key(KeyCode::Enter)),
            LoginOutcome::Submit(LoginAnswer::Password("hunter2".to_owned()))
        );
        assert!(
            view.input().is_empty(),
            "password not retained after submit"
        );
    }

    #[test]
    fn transitioning_clears_a_half_typed_field() {
        let mut view = LoginView::from_state(AuthState::WaitPhoneNumber);
        type_str(&mut view, "+1555");
        view.set_state(AuthState::WaitCode);
        assert!(
            view.input().is_empty(),
            "the phone entry does not carry over"
        );
        assert_eq!(view.state(), &AuthState::WaitCode);
    }

    #[test]
    fn the_passive_screens_take_no_input() {
        for state in [
            AuthState::WaitTdlibParameters,
            AuthState::Ready,
            AuthState::Closed,
            AuthState::WaitPremiumPurchase {
                store_product_id: "p".to_owned(),
            },
            AuthState::WaitOtherDeviceConfirmation {
                link: "tg://login?token=x".to_owned(),
            },
        ] {
            let mut view = LoginView::from_state(state);
            assert_eq!(
                view.on_key(&key(KeyCode::Char('a'))),
                LoginOutcome::Unchanged
            );
        }
    }

    #[test]
    fn registration_needs_acceptance_and_a_first_name_to_submit() {
        let mut view = LoginView::from_state(AuthState::WaitRegistration {
            terms_of_service: "Be nice.".to_owned(),
        });
        // First field is the first name.
        type_str(&mut view, "Ada");
        // Enter without accepting the terms is a no-op.
        assert_eq!(view.on_key(&key(KeyCode::Enter)), LoginOutcome::Unchanged);

        // Tab to the last name, fill it, Tab to the checkbox, accept it.
        assert_eq!(view.on_key(&key(KeyCode::Tab)), LoginOutcome::Dirty);
        type_str(&mut view, "Lovelace");
        assert_eq!(view.on_key(&key(KeyCode::Tab)), LoginOutcome::Dirty);
        assert_eq!(view.register().field(), RegField::Accept);
        assert_eq!(view.on_key(&key(KeyCode::Char(' '))), LoginOutcome::Dirty);
        assert!(view.register().accepted());

        assert_eq!(
            view.on_key(&key(KeyCode::Enter)),
            LoginOutcome::Submit(LoginAnswer::Register {
                first_name: "Ada".to_owned(),
                last_name: "Lovelace".to_owned(),
            })
        );
    }

    #[test]
    fn space_only_toggles_acceptance_on_the_checkbox_field() {
        let mut view = LoginView::from_state(AuthState::WaitRegistration {
            terms_of_service: "terms".to_owned(),
        });
        // On the first-name field, space is just text.
        view.on_key(&key(KeyCode::Char(' ')));
        assert_eq!(view.register().first().text(), " ");
        assert!(!view.register().accepted());
    }

    // --- rendering ---

    #[test]
    fn the_phone_screen_renders_its_label_and_qr_hint() {
        let view = LoginView::from_state(AuthState::WaitPhoneNumber);
        let text = flatten(&render(&view, 60, 20));
        assert!(text.contains("Sign in"), "screen heading");
        assert!(text.contains("Phone number"), "field label");
        assert!(text.contains("use QR code"), "QR alternative in the hint");
    }

    #[test]
    fn the_password_screen_masks_the_typed_secret() {
        let mut view = LoginView::from_state(AuthState::WaitPassword {
            hint: "a pet".to_owned(),
        });
        type_str(&mut view, "secret");
        let text = flatten(&render(&view, 60, 20));
        assert!(text.contains("Hint: a pet"), "shows the password hint");
        assert!(text.contains('•'), "the secret is rendered as bullets");
        assert!(!text.contains("secret"), "never the plaintext password");
    }

    #[test]
    fn the_qr_screen_draws_a_code_and_the_scan_instructions() {
        let view = LoginView::from_state(AuthState::WaitOtherDeviceConfirmation {
            link: "tg://login?token=abc123".to_owned(),
        });
        let text = flatten(&render(&view, 80, 60));
        assert!(text.contains("Scan to sign in"), "heading");
        assert!(text.contains("Link Desktop Device"), "scan instructions");
        assert!(text.contains('█') || text.contains('▀'), "QR modules drawn");
    }

    #[test]
    fn the_premium_screen_explains_the_dead_end() {
        let view = LoginView::from_state(AuthState::WaitPremiumPurchase {
            store_product_id: "org.telegram.telegramPremium".to_owned(),
        });
        let text = flatten(&render(&view, 70, 20));
        assert!(text.contains("Premium required"), "heading");
        assert!(
            text.contains("mobile app"),
            "directs to the mobile purchase"
        );
        assert!(
            text.contains("org.telegram.telegramPremium"),
            "names the store product"
        );
    }

    #[test]
    fn the_registration_screen_shows_the_terms_and_fields() {
        let mut view = LoginView::from_state(AuthState::WaitRegistration {
            terms_of_service: "Be excellent to each other.".to_owned(),
        });
        type_str(&mut view, "Ada");
        let text = flatten(&render(&view, 70, 24));
        assert!(text.contains("Create your account"), "heading");
        assert!(text.contains("Be excellent to each other."), "terms text");
        assert!(text.contains("First name"), "first-name field");
        assert!(text.contains("Last name"), "last-name field");
        assert!(text.contains("accept the Terms"), "acceptance checkbox");
        assert!(text.contains("[ ]"), "checkbox starts unchecked");
    }

    #[test]
    fn the_email_code_screen_names_the_inbox() {
        let view = LoginView::from_state(AuthState::WaitEmailCode {
            email_pattern: "a***@example.com".to_owned(),
        });
        let text = flatten(&render(&view, 70, 20));
        assert!(text.contains("email code"), "heading");
        assert!(
            text.contains("a***@example.com"),
            "the masked inbox pattern"
        );
    }

    // --- driver: answer dispatch & error surfacing ---

    use std::cell::RefCell;
    use tuigram_core::ClientParameters;
    use tuigram_core::enums::AuthorizationState;

    /// Records which auth request the driver dispatched, so the answer→request
    /// mapping is asserted with no network and no live tdjson. The 2FA password is
    /// never recorded — only that the call happened, the same rule the library
    /// follows.
    #[derive(Default)]
    struct SpyAuth {
        calls: RefCell<Vec<String>>,
    }

    impl SpyAuth {
        fn last(&self) -> Option<String> {
            self.calls.borrow().last().cloned()
        }
    }

    impl AuthRequests for SpyAuth {
        async fn authorization_state(&self) -> Result<AuthorizationState, TdError> {
            Ok(AuthorizationState::WaitPhoneNumber)
        }
        async fn set_log_verbosity_level(&self, _level: i32) -> Result<(), TdError> {
            Ok(())
        }
        async fn set_log_stream(&self, _path: String) -> Result<(), TdError> {
            Ok(())
        }
        async fn set_tdlib_parameters(&self, _params: ClientParameters) -> Result<(), TdError> {
            Ok(())
        }
        async fn set_phone_number(&self, phone_number: String) -> Result<(), TdError> {
            self.calls
                .borrow_mut()
                .push(format!("phone({phone_number})"));
            Ok(())
        }
        async fn request_qr_code_authentication(&self) -> Result<(), TdError> {
            self.calls.borrow_mut().push("qr()".to_owned());
            Ok(())
        }
        async fn check_authentication_code(&self, code: String) -> Result<(), TdError> {
            self.calls.borrow_mut().push(format!("code({code})"));
            Ok(())
        }
        async fn check_authentication_password(&self, _password: String) -> Result<(), TdError> {
            // Record only that it was called, never the password value.
            self.calls
                .borrow_mut()
                .push("password(<redacted>)".to_owned());
            Ok(())
        }
        async fn register_user(
            &self,
            first_name: String,
            last_name: String,
        ) -> Result<(), TdError> {
            self.calls
                .borrow_mut()
                .push(format!("register({first_name},{last_name})"));
            Ok(())
        }
        async fn set_authentication_email_address(
            &self,
            email_address: String,
        ) -> Result<(), TdError> {
            self.calls
                .borrow_mut()
                .push(format!("email({email_address})"));
            Ok(())
        }
        async fn check_authentication_email_code(&self, code: String) -> Result<(), TdError> {
            self.calls.borrow_mut().push(format!("email_code({code})"));
            Ok(())
        }
        async fn log_out(&self) -> Result<(), TdError> {
            Ok(())
        }
        async fn close(&self) -> Result<(), TdError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn each_answer_dispatches_its_request() {
        let cases: Vec<(LoginAnswer, &str)> = vec![
            (
                LoginAnswer::Phone("+15551234567".to_owned()),
                "phone(+15551234567)",
            ),
            (LoginAnswer::RequestQr, "qr()"),
            (LoginAnswer::Code("12345".to_owned()), "code(12345)"),
            (
                LoginAnswer::Password("hunter2".to_owned()),
                "password(<redacted>)",
            ),
            (LoginAnswer::Email("a@b.com".to_owned()), "email(a@b.com)"),
            (
                LoginAnswer::EmailCode("424242".to_owned()),
                "email_code(424242)",
            ),
            (
                LoginAnswer::Register {
                    first_name: "Ada".to_owned(),
                    last_name: "Lovelace".to_owned(),
                },
                "register(Ada,Lovelace)",
            ),
        ];
        for (answer, expected) in cases {
            let spy = SpyAuth::default();
            let login = Login::new(&spy);
            submit_answer(&login, answer).await.unwrap();
            assert_eq!(spy.last().as_deref(), Some(expected));
        }
    }

    #[test]
    fn error_line_uses_the_tdlib_text_or_falls_back_to_the_code() {
        let coded = TdError {
            code: 400,
            message: "PHONE_CODE_INVALID".to_owned(),
        };
        assert_eq!(error_line(&coded), "PHONE_CODE_INVALID");

        // A blank message (no named code) still reads as an error, not an empty line.
        let blank = TdError {
            code: 420,
            message: String::new(),
        };
        assert_eq!(error_line(&blank), "login error (code 420)");
    }

    #[test]
    fn terminal_states_end_the_loop_others_continue() {
        assert_eq!(terminal_end(&AuthState::Ready), Some(LoginEnd::Ready));
        assert_eq!(terminal_end(&AuthState::Closed), Some(LoginEnd::Closed));
        assert_eq!(terminal_end(&AuthState::WaitPhoneNumber), None);
        assert_eq!(
            terminal_end(&AuthState::WaitOtherDeviceConfirmation {
                link: "tg://login?token=x".to_owned()
            }),
            None,
            "QR still waits on the other device"
        );
    }

    #[test]
    fn a_rejected_submit_message_shows_then_clears_on_transition() {
        let mut view = LoginView::from_state(AuthState::WaitCode);
        assert!(view.error().is_none());

        view.set_error(Some("PHONE_CODE_INVALID".to_owned()));
        assert_eq!(view.error(), Some("PHONE_CODE_INVALID"));
        // It renders under the field with the failure marker.
        let text = flatten(&render(&view, 60, 20));
        assert!(text.contains("✗ PHONE_CODE_INVALID"), "the rejection shows");

        // A transition (the next waiting state) clears it, like the fields.
        view.set_state(AuthState::WaitPassword {
            hint: String::new(),
        });
        assert!(view.error().is_none(), "the error does not carry over");
    }
}
