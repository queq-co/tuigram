//! Login state machine mirroring TDLib's `updateAuthorizationState`.
//!
//! TDLib *is* the authority on login: it emits an [`AuthorizationState`] and
//! waits for the matching request, looping until it reaches `Ready`. This module
//! projects that stream onto a reduced [`AuthState`] the login UI is driven from
//! ([`AuthState::from_tdlib`]), and a [`Login`] driver that answers each waiting
//! state through the [`AuthRequests`] seam.
//!
//! [`AuthRequests`] is this module's slice of the request surface — the login
//! requests, and nothing else. It is owned here rather than in `bridge` so that
//! the bridge stays pure transport and a driver (or its test double) depends on
//! only the requests it makes. [`Bridge`] implements it via its public id; the
//! chats/messages modules own their own request traits the same way.
//!
//! Every TDLib authorization state is handled. The phone path — **phone number +
//! login code + 2FA password** — plus **QR login** (`waitOtherDeviceConfirmation`:
//! request a code, scan the link on an already-signed-in device), **new-user
//! registration** (`waitRegistration`: accept the terms of service and submit a
//! name), and **email login** (`waitEmailAddress` / `waitEmailCode`) are all driven
//! to completion. **Premium purchase** (`waitPremiumPurchase`) is modeled and
//! surfaced explicitly as a dead end — completing it needs an App Store / Play
//! in-store purchase a headless client can't perform — so the flow reports it
//! rather than hanging on a silent unknown.
//!
//! The projection is total by *exhaustive* match over TDLib's (closed)
//! [`AuthorizationState`] enum: there is no catch-all, so a state added by a future
//! TDLib version is a compile error here, never a silent misclassification.
//!
//! Secrets are never retained: the login code, the email code, and the 2FA
//! password are taken by value and moved straight into their TDLib request (see
//! the threat model).

use crate::bridge::{Bridge, ClientParameters};
use tdlib_rs::enums::AuthorizationState;
use tdlib_rs::types::Error as TdError;

/// The login request seam — tuigram's auth slice of the `tdlib_rs::functions`
/// surface, segregated from the chats/messages requests so the login driver and
/// its test double implement only these.
///
/// [`Bridge`] implements it over a live `tdjson` client (via [`Bridge::id`]);
/// tests implement it with a spy. Logic written against `C: AuthRequests` runs
/// unchanged on either, with no network and no live `tdjson`.
// Internal seam: every consumer is in-crate and generic over `C: AuthRequests`,
// so the lack of a caller-controllable `Send` bound (the reason this lint fires)
// is not a concern here.
#[allow(async_fn_in_trait)]
pub trait AuthRequests {
    /// Fetch the current authorization state.
    ///
    /// The canonical proof that request/response correlation works: TDLib
    /// answers it immediately from local state with no network, so a successful
    /// round-trip means the receive loop is correctly notifying the observer.
    async fn authorization_state(&self) -> Result<AuthorizationState, TdError>;

    /// Set TDLib's global log verbosity level (0 = fatal only … 3 = info,
    /// the default … 5 = verbose).
    ///
    /// Security-relevant: at the default level TDLib logs request and response
    /// payloads to stderr, which during login would include phone numbers,
    /// codes, and the 2FA password. The flow lowers this before any
    /// credential-bearing request (see [`Login::set_parameters`]).
    async fn set_log_verbosity_level(&self, level: i32) -> Result<(), TdError>;

    /// Answer `WaitTdlibParameters` — initialize the client.
    async fn set_tdlib_parameters(&self, params: ClientParameters) -> Result<(), TdError>;

    /// Answer `WaitPhoneNumber` — submit the phone number and request a code.
    async fn set_phone_number(&self, phone_number: String) -> Result<(), TdError>;

    /// Answer `WaitPhoneNumber` the other way — request QR-code authentication
    /// instead of a phone number.
    ///
    /// TDLib responds by moving to `WaitOtherDeviceConfirmation`, carrying a
    /// `tg://login` link to render as a QR code; scanning it on an already
    /// signed-in device completes the login (which may then still require the
    /// 2FA password). Carries no credential payload of its own.
    async fn request_qr_code_authentication(&self) -> Result<(), TdError>;

    /// Answer `WaitCode` — submit the login code the user received.
    async fn check_authentication_code(&self, code: String) -> Result<(), TdError>;

    /// Answer `WaitPassword` — submit the 2FA password.
    ///
    /// The password is moved straight into the TDLib request and never retained
    /// (see the threat model).
    async fn check_authentication_password(&self, password: String) -> Result<(), TdError>;

    /// Answer `WaitRegistration` — accept the terms of service and register the
    /// new user with a first and last name.
    ///
    /// Reached when the phone number isn't tied to an account yet. The names are
    /// not credentials; `disable_notification` is left `false`, letting TDLib
    /// notify contacts of the new account as it defaults to.
    async fn register_user(&self, first_name: String, last_name: String) -> Result<(), TdError>;

    /// Answer `WaitEmailAddress` — submit the user's email address, which TDLib
    /// then sends an authentication code to (moving to `WaitEmailCode`).
    async fn set_authentication_email_address(&self, email_address: String) -> Result<(), TdError>;

    /// Answer `WaitEmailCode` — submit the code delivered to the email address.
    ///
    /// Wraps the code in `EmailAddressAuthentication::Code`; the Apple/Google ID
    /// token answers TDLib also accepts here are out of scope for a headless
    /// client. The code is moved straight into the request and never retained.
    async fn check_authentication_email_code(&self, code: String) -> Result<(), TdError>;

    /// Log out of the current account.
    ///
    /// TDLib invalidates the session server-side and wipes its local database,
    /// then drives authorization back through `Closed` to `WaitPhoneNumber`, so a
    /// fresh login can follow. Carries no credential payload, and leaves the app's
    /// `api_id`/`api_hash` and the storage encryption key untouched — those are
    /// reused for the next login, not part of the account session.
    async fn log_out(&self) -> Result<(), TdError>;
}

impl AuthRequests for Bridge {
    async fn authorization_state(&self) -> Result<AuthorizationState, TdError> {
        tdlib_rs::functions::get_authorization_state(self.id()).await
    }

    async fn set_log_verbosity_level(&self, level: i32) -> Result<(), TdError> {
        tdlib_rs::functions::set_log_verbosity_level(level, self.id()).await
    }

    async fn set_tdlib_parameters(&self, params: ClientParameters) -> Result<(), TdError> {
        tdlib_rs::functions::set_tdlib_parameters(
            params.use_test_dc,
            params.database_directory,
            params.files_directory,
            params.database_encryption_key,
            true,  // use_file_database: persist downloaded/uploaded file info
            true,  // use_chat_info_database: cache users/groups across restarts
            true,  // use_message_database: cache chats/messages across restarts
            false, // use_secret_chats: out of scope for Phase 2
            params.api_id,
            params.api_hash,
            params.system_language_code,
            params.device_model,
            String::new(), // system_version: empty -> TDLib auto-detects
            params.application_version,
            self.id(),
        )
        .await
    }

    async fn set_phone_number(&self, phone_number: String) -> Result<(), TdError> {
        // None settings -> TDLib's defaults for code delivery.
        tdlib_rs::functions::set_authentication_phone_number(phone_number, None, self.id()).await
    }

    async fn request_qr_code_authentication(&self) -> Result<(), TdError> {
        // Empty other_user_ids: authenticate only this account, not extra users.
        tdlib_rs::functions::request_qr_code_authentication(vec![], self.id()).await
    }

    async fn check_authentication_code(&self, code: String) -> Result<(), TdError> {
        tdlib_rs::functions::check_authentication_code(code, self.id()).await
    }

    async fn check_authentication_password(&self, password: String) -> Result<(), TdError> {
        tdlib_rs::functions::check_authentication_password(password, self.id()).await
    }

    async fn register_user(&self, first_name: String, last_name: String) -> Result<(), TdError> {
        // disable_notification = false: let TDLib notify contacts of the new account.
        tdlib_rs::functions::register_user(first_name, last_name, false, self.id()).await
    }

    async fn set_authentication_email_address(&self, email_address: String) -> Result<(), TdError> {
        tdlib_rs::functions::set_authentication_email_address(email_address, self.id()).await
    }

    async fn check_authentication_email_code(&self, code: String) -> Result<(), TdError> {
        // Headless clients only deliver the emailed code; Apple/Google ID tokens
        // (the enum's other variants) aren't reachable here.
        let code = tdlib_rs::enums::EmailAddressAuthentication::Code(
            tdlib_rs::types::EmailAddressAuthenticationCode { code },
        );
        tdlib_rs::functions::check_authentication_email_code(code, self.id()).await
    }

    async fn log_out(&self) -> Result<(), TdError> {
        tdlib_rs::functions::log_out(self.id()).await
    }
}

/// TDLib log verbosity the login flow drops to before sending any request:
/// errors only, so request/response payloads (phone number, code, 2FA password)
/// are never written to TDLib's stderr log (see the threat model). `1` keeps
/// genuine errors visible while silencing the default info-level logging.
pub const SECURE_LOG_VERBOSITY: i32 = 1;

/// tuigram's view of the login flow — a reduced projection of TDLib's
/// [`AuthorizationState`] covering the states Phase 2 acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthState {
    /// Brand-new client; answer with `setTdlibParameters`.
    WaitTdlibParameters,
    /// Needs the user's phone number in international format.
    WaitPhoneNumber,
    /// A login code was delivered; needs the code.
    WaitCode,
    /// 2FA is enabled; needs the account password. Carries the user's hint
    /// (may be empty) for display — never the password itself.
    WaitPassword { hint: String },
    /// QR login was requested; TDLib is waiting for the link to be scanned on an
    /// already signed-in device. Carries the `tg://login` link to render as a QR
    /// code. No input is taken here — the confirmation happens on the other
    /// device — so the flow advances on the next `updateAuthorizationState`.
    WaitOtherDeviceConfirmation { link: String },
    /// New-user registration: the phone number isn't tied to an account yet, so
    /// TDLib needs a first and last name to create one. Carries the
    /// terms-of-service text the user must accept before [`Login::register`]
    /// submits the name.
    WaitRegistration { terms_of_service: String },
    /// Email-based login: TDLib needs the user's email address, answered with
    /// [`Login::submit_email_address`]. (Apple/Google ID sign-in — the other
    /// answers TDLib would take here — is out of scope for a headless client.)
    WaitEmailAddress,
    /// An email authentication code was sent; needs the code, answered with
    /// [`Login::submit_email_code`]. Carries the masked address pattern (e.g.
    /// `a***@example.com`) so the UI can show which inbox to check — never a code.
    WaitEmailCode { email_pattern: String },
    /// Login requires buying Telegram Premium as an in-store (App Store / Play)
    /// purchase, which a headless client can't perform. Modeled and surfaced
    /// explicitly — carrying the store product id — so the flow reports a dead end
    /// rather than hanging; there is no request that answers it here.
    WaitPremiumPurchase { store_product_id: String },
    /// Logged in; normal updates flow.
    Ready,
    /// Logging out, closing, or closed — terminal; tear down the session.
    Closed,
}

impl AuthState {
    /// Project a TDLib [`AuthorizationState`] onto tuigram's [`AuthState`].
    ///
    /// Total over TDLib's enum by *exhaustive* match — every state maps to a
    /// handled variant, with no catch-all — so a state added by a future TDLib
    /// version is a compile error here rather than a silent misclassification.
    #[must_use]
    pub fn from_tdlib(state: &AuthorizationState) -> Self {
        match state {
            AuthorizationState::WaitTdlibParameters => Self::WaitTdlibParameters,
            AuthorizationState::WaitPhoneNumber => Self::WaitPhoneNumber,
            AuthorizationState::WaitCode(_) => Self::WaitCode,
            AuthorizationState::WaitPassword(p) => Self::WaitPassword {
                hint: p.password_hint.clone(),
            },
            AuthorizationState::Ready => Self::Ready,
            AuthorizationState::LoggingOut
            | AuthorizationState::Closing
            | AuthorizationState::Closed => Self::Closed,
            AuthorizationState::WaitOtherDeviceConfirmation(c) => {
                Self::WaitOtherDeviceConfirmation {
                    link: c.link.clone(),
                }
            }
            AuthorizationState::WaitRegistration(r) => Self::WaitRegistration {
                terms_of_service: r.terms_of_service.text.text.clone(),
            },
            AuthorizationState::WaitEmailAddress(_) => Self::WaitEmailAddress,
            AuthorizationState::WaitEmailCode(c) => Self::WaitEmailCode {
                email_pattern: c.code_info.email_address_pattern.clone(),
            },
            AuthorizationState::WaitPremiumPurchase(p) => Self::WaitPremiumPurchase {
                store_product_id: p.store_product_id.clone(),
            },
        }
    }

    /// Whether login has reached a terminal state and no further input applies.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Ready | Self::Closed)
    }
}

/// Drives login over a [`TgClient`], tracking the current [`AuthState`].
///
/// The owning loop feeds each `updateAuthorizationState` to [`Login::on_update`]
/// and, when the state needs input, calls the matching handler. The driver does
/// not consume the update stream itself — that stays on the bridge so other
/// subsystems can observe it too.
pub struct Login<'a, C: AuthRequests> {
    client: &'a C,
    state: AuthState,
}

impl<'a, C: AuthRequests> Login<'a, C> {
    /// Start a login driver. A fresh `tdjson` client begins in
    /// [`AuthState::WaitTdlibParameters`].
    #[must_use]
    pub fn new(client: &'a C) -> Self {
        Self {
            client,
            state: AuthState::WaitTdlibParameters,
        }
    }

    /// The current login state.
    #[must_use]
    pub fn state(&self) -> &AuthState {
        &self.state
    }

    /// Advance the machine on a TDLib authorization-state update.
    pub fn on_update(&mut self, state: &AuthorizationState) {
        self.state = AuthState::from_tdlib(state);
    }

    /// Answer [`AuthState::WaitTdlibParameters`].
    ///
    /// `setTdlibParameters` is the first request of every login, so this is
    /// where we first silence TDLib's logging ([`SECURE_LOG_VERBOSITY`]) —
    /// before any credential-bearing request, including the `api_id`/`api_hash`
    /// in `params` itself.
    pub async fn set_parameters(&self, params: ClientParameters) -> Result<(), TdError> {
        self.client
            .set_log_verbosity_level(SECURE_LOG_VERBOSITY)
            .await?;
        self.client.set_tdlib_parameters(params).await
    }

    /// Answer [`AuthState::WaitPhoneNumber`]. The number is sent in international
    /// format; TDLib then delivers a code and transitions to `WaitCode`.
    pub async fn submit_phone_number(&self, phone_number: String) -> Result<(), TdError> {
        self.client.set_phone_number(phone_number).await
    }

    /// Answer [`AuthState::WaitPhoneNumber`] with QR login instead of a phone
    /// number. TDLib transitions to
    /// [`AuthState::WaitOtherDeviceConfirmation`], whose `link` is rendered as a
    /// QR code and scanned on an already signed-in device.
    pub async fn request_qr_code(&self) -> Result<(), TdError> {
        self.client.request_qr_code_authentication().await
    }

    /// Answer [`AuthState::WaitCode`] with the code the user received.
    pub async fn submit_code(&self, code: String) -> Result<(), TdError> {
        self.client.check_authentication_code(code).await
    }

    /// Answer [`AuthState::WaitPassword`] with the 2FA password.
    ///
    /// The password is moved straight into the request and never stored.
    pub async fn submit_password(&self, password: String) -> Result<(), TdError> {
        self.client.check_authentication_password(password).await
    }

    /// Answer [`AuthState::WaitRegistration`] — accept the terms of service and
    /// register a new account with the given first and last name.
    pub async fn register(&self, first_name: String, last_name: String) -> Result<(), TdError> {
        self.client.register_user(first_name, last_name).await
    }

    /// Answer [`AuthState::WaitEmailAddress`] with the user's email address; TDLib
    /// then sends a code and transitions to [`AuthState::WaitEmailCode`].
    pub async fn submit_email_address(&self, email_address: String) -> Result<(), TdError> {
        self.client
            .set_authentication_email_address(email_address)
            .await
    }

    /// Answer [`AuthState::WaitEmailCode`] with the code delivered to the email
    /// address. The code is moved straight into the request and never stored.
    pub async fn submit_email_code(&self, code: String) -> Result<(), TdError> {
        self.client.check_authentication_email_code(code).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use tdlib_rs::enums::AuthenticationCodeType;
    use tdlib_rs::types::{
        AuthenticationCodeInfo, AuthenticationCodeTypeSms, AuthorizationStateWaitCode,
        AuthorizationStateWaitEmailAddress, AuthorizationStateWaitEmailCode,
        AuthorizationStateWaitOtherDeviceConfirmation, AuthorizationStateWaitPassword,
        AuthorizationStateWaitPremiumPurchase, AuthorizationStateWaitRegistration,
        EmailAddressAuthenticationCodeInfo, FormattedText, TermsOfService,
    };

    /// A `WaitCode` state. Its payload is irrelevant to the projection (which
    /// matches `WaitCode(_)`), but the type has no `Default`, so build one.
    fn wait_code() -> AuthorizationState {
        AuthorizationState::WaitCode(AuthorizationStateWaitCode {
            code_info: AuthenticationCodeInfo {
                phone_number: String::new(),
                r#type: AuthenticationCodeType::Sms(AuthenticationCodeTypeSms::default()),
                next_type: None,
                timeout: 0,
            },
        })
    }

    #[test]
    fn projects_each_tdlib_state_onto_authstate() {
        let cases = [
            (
                AuthorizationState::WaitTdlibParameters,
                AuthState::WaitTdlibParameters,
            ),
            (
                AuthorizationState::WaitPhoneNumber,
                AuthState::WaitPhoneNumber,
            ),
            (wait_code(), AuthState::WaitCode),
            (AuthorizationState::Ready, AuthState::Ready),
            (AuthorizationState::LoggingOut, AuthState::Closed),
            (AuthorizationState::Closing, AuthState::Closed),
            (AuthorizationState::Closed, AuthState::Closed),
        ];
        for (input, expected) in cases {
            assert_eq!(AuthState::from_tdlib(&input), expected);
        }
    }

    #[test]
    fn wait_password_carries_the_hint_but_not_the_password() {
        let state = AuthorizationState::WaitPassword(AuthorizationStateWaitPassword {
            password_hint: "my cat's name".to_owned(),
            has_recovery_email_address: true,
            ..Default::default()
        });
        assert_eq!(
            AuthState::from_tdlib(&state),
            AuthState::WaitPassword {
                hint: "my cat's name".to_owned()
            }
        );
    }

    #[test]
    fn qr_login_state_surfaces_its_link() {
        let qr = AuthorizationState::WaitOtherDeviceConfirmation(
            AuthorizationStateWaitOtherDeviceConfirmation {
                link: "tg://login?token=abc".to_owned(),
            },
        );
        assert_eq!(
            AuthState::from_tdlib(&qr),
            AuthState::WaitOtherDeviceConfirmation {
                link: "tg://login?token=abc".to_owned()
            }
        );
    }

    #[test]
    fn registration_state_surfaces_the_terms_of_service() {
        let reg = AuthorizationState::WaitRegistration(AuthorizationStateWaitRegistration {
            terms_of_service: TermsOfService {
                text: FormattedText {
                    text: "Be excellent to each other.".to_owned(),
                    entities: vec![],
                },
                min_user_age: 0,
                show_popup: true,
            },
        });
        assert_eq!(
            AuthState::from_tdlib(&reg),
            AuthState::WaitRegistration {
                terms_of_service: "Be excellent to each other.".to_owned()
            }
        );
    }

    #[test]
    fn email_states_surface_the_prompt_and_the_masked_pattern() {
        assert_eq!(
            AuthState::from_tdlib(&AuthorizationState::WaitEmailAddress(
                AuthorizationStateWaitEmailAddress::default()
            )),
            AuthState::WaitEmailAddress
        );

        let code = AuthorizationState::WaitEmailCode(AuthorizationStateWaitEmailCode {
            code_info: EmailAddressAuthenticationCodeInfo {
                email_address_pattern: "a***@example.com".to_owned(),
                length: 6,
            },
            ..Default::default()
        });
        assert_eq!(
            AuthState::from_tdlib(&code),
            AuthState::WaitEmailCode {
                email_pattern: "a***@example.com".to_owned()
            }
        );
    }

    #[test]
    fn premium_purchase_is_modeled_as_a_dead_end_not_unsupported() {
        let premium =
            AuthorizationState::WaitPremiumPurchase(AuthorizationStateWaitPremiumPurchase {
                store_product_id: "tg_premium_monthly".to_owned(),
                ..Default::default()
            });
        let state = AuthState::from_tdlib(&premium);
        assert_eq!(
            state,
            AuthState::WaitPremiumPurchase {
                store_product_id: "tg_premium_monthly".to_owned()
            }
        );
        // A dead end for a headless client, but not a torn-down session.
        assert!(!state.is_terminal());
    }

    #[test]
    fn ready_and_closed_are_terminal_others_are_not() {
        assert!(AuthState::Ready.is_terminal());
        assert!(AuthState::Closed.is_terminal());
        assert!(!AuthState::WaitPhoneNumber.is_terminal());
        assert!(!AuthState::WaitCode.is_terminal());
        // QR confirmation still waits on the other device — not terminal.
        assert!(
            !AuthState::WaitOtherDeviceConfirmation {
                link: String::new()
            }
            .is_terminal()
        );
    }

    /// Records which handler the driver invoked and with what argument, so a
    /// full login flow can be asserted with no network.
    #[derive(Default)]
    struct SpyClient {
        calls: RefCell<Vec<String>>,
    }

    impl SpyClient {
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl AuthRequests for SpyClient {
        async fn authorization_state(&self) -> Result<AuthorizationState, TdError> {
            Ok(AuthorizationState::WaitTdlibParameters)
        }

        async fn set_log_verbosity_level(&self, level: i32) -> Result<(), TdError> {
            self.calls
                .borrow_mut()
                .push(format!("set_log_verbosity_level({level})"));
            Ok(())
        }
        async fn set_tdlib_parameters(&self, params: ClientParameters) -> Result<(), TdError> {
            self.calls
                .borrow_mut()
                .push(format!("set_tdlib_parameters(api_id={})", params.api_id));
            Ok(())
        }
        async fn set_phone_number(&self, phone_number: String) -> Result<(), TdError> {
            self.calls
                .borrow_mut()
                .push(format!("set_phone_number({phone_number})"));
            Ok(())
        }
        async fn request_qr_code_authentication(&self) -> Result<(), TdError> {
            self.calls
                .borrow_mut()
                .push("request_qr_code_authentication()".to_owned());
            Ok(())
        }
        async fn check_authentication_code(&self, code: String) -> Result<(), TdError> {
            self.calls
                .borrow_mut()
                .push(format!("check_authentication_code({code})"));
            Ok(())
        }
        async fn check_authentication_password(&self, password: String) -> Result<(), TdError> {
            // Record only that it was called, never the password value.
            let _ = password;
            self.calls
                .borrow_mut()
                .push("check_authentication_password(<redacted>)".to_owned());
            Ok(())
        }
        async fn register_user(
            &self,
            first_name: String,
            last_name: String,
        ) -> Result<(), TdError> {
            self.calls
                .borrow_mut()
                .push(format!("register_user({first_name},{last_name})"));
            Ok(())
        }
        async fn set_authentication_email_address(
            &self,
            email_address: String,
        ) -> Result<(), TdError> {
            self.calls
                .borrow_mut()
                .push(format!("set_authentication_email_address({email_address})"));
            Ok(())
        }
        async fn check_authentication_email_code(&self, code: String) -> Result<(), TdError> {
            self.calls
                .borrow_mut()
                .push(format!("check_authentication_email_code({code})"));
            Ok(())
        }
        async fn log_out(&self) -> Result<(), TdError> {
            self.calls.borrow_mut().push("log_out()".to_owned());
            Ok(())
        }
    }

    fn params() -> ClientParameters {
        ClientParameters {
            api_id: 42,
            api_hash: "hash".to_owned(),
            database_directory: "/tmp/db".to_owned(),
            files_directory: "/tmp/files".to_owned(),
            database_encryption_key: "key".to_owned(),
            system_language_code: "en".to_owned(),
            device_model: "tuigram-test".to_owned(),
            application_version: "0.0.0".to_owned(),
            use_test_dc: true,
        }
    }

    /// Drive the full waitTdlibParameters -> ... -> ready flow with synthetic
    /// updates and assert both the state transitions and the handler calls,
    /// with no network and no live tdjson.
    #[tokio::test]
    async fn full_login_flow_transitions_and_dispatches() {
        let client = SpyClient::default();
        let mut login = Login::new(&client);
        assert_eq!(*login.state(), AuthState::WaitTdlibParameters);

        login.set_parameters(params()).await.unwrap();

        login.on_update(&AuthorizationState::WaitPhoneNumber);
        assert_eq!(*login.state(), AuthState::WaitPhoneNumber);
        login
            .submit_phone_number("+15551234567".to_owned())
            .await
            .unwrap();

        login.on_update(&wait_code());
        assert_eq!(*login.state(), AuthState::WaitCode);
        login.submit_code("12345".to_owned()).await.unwrap();

        login.on_update(&AuthorizationState::WaitPassword(
            AuthorizationStateWaitPassword {
                password_hint: "hint".to_owned(),
                ..Default::default()
            },
        ));
        assert_eq!(
            *login.state(),
            AuthState::WaitPassword {
                hint: "hint".to_owned()
            }
        );
        login.submit_password("hunter2".to_owned()).await.unwrap();

        login.on_update(&AuthorizationState::Ready);
        assert_eq!(*login.state(), AuthState::Ready);
        assert!(login.state().is_terminal());

        // Logging is silenced before the very first request — ahead of any
        // credential, including the api_id/api_hash in setTdlibParameters.
        assert_eq!(
            client.calls(),
            vec![
                "set_log_verbosity_level(1)".to_owned(),
                "set_tdlib_parameters(api_id=42)".to_owned(),
                "set_phone_number(+15551234567)".to_owned(),
                "check_authentication_code(12345)".to_owned(),
                "check_authentication_password(<redacted>)".to_owned(),
            ]
        );
    }

    /// Logout threads through the auth request seam — the one call, no
    /// credential payload — so a driver can end a session over `C: AuthRequests`
    /// with no network and no live `tdjson`.
    #[tokio::test]
    async fn log_out_threads_through_the_seam() {
        let client = SpyClient::default();
        client.log_out().await.unwrap();
        assert_eq!(client.calls(), vec!["log_out()".to_owned()]);
    }

    /// A login without 2FA skips `WaitPassword` entirely.
    #[tokio::test]
    async fn flow_without_2fa_goes_code_straight_to_ready() {
        let client = SpyClient::default();
        let mut login = Login::new(&client);

        login.on_update(&wait_code());
        assert_eq!(*login.state(), AuthState::WaitCode);
        login.submit_code("99999".to_owned()).await.unwrap();

        login.on_update(&AuthorizationState::Ready);
        assert_eq!(*login.state(), AuthState::Ready);
        assert_eq!(
            client.calls(),
            vec!["check_authentication_code(99999)".to_owned()]
        );
    }

    /// QR login is the alternative answer to `WaitPhoneNumber`: request a QR
    /// code, surface the scan link from `WaitOtherDeviceConfirmation`, then let
    /// the confirmation on the other device carry the flow to `Ready` — with no
    /// further input here, and no network.
    #[tokio::test]
    async fn qr_login_flow_requests_a_code_then_confirms_on_the_other_device() {
        let client = SpyClient::default();
        let mut login = Login::new(&client);

        login.on_update(&AuthorizationState::WaitPhoneNumber);
        assert_eq!(*login.state(), AuthState::WaitPhoneNumber);

        // Choose QR instead of typing a phone number.
        login.request_qr_code().await.unwrap();

        // TDLib answers with the link to render and scan.
        login.on_update(&AuthorizationState::WaitOtherDeviceConfirmation(
            AuthorizationStateWaitOtherDeviceConfirmation {
                link: "tg://login?token=xyz".to_owned(),
            },
        ));
        assert_eq!(
            *login.state(),
            AuthState::WaitOtherDeviceConfirmation {
                link: "tg://login?token=xyz".to_owned()
            }
        );
        assert!(!login.state().is_terminal());

        // The other device confirms; login completes with no further input.
        login.on_update(&AuthorizationState::Ready);
        assert_eq!(*login.state(), AuthState::Ready);

        assert_eq!(
            client.calls(),
            vec!["request_qr_code_authentication()".to_owned()]
        );
    }

    /// A new-user login: an unregistered phone number routes through email
    /// verification and then registration. Each state surfaces through
    /// `AuthState` (the email-code one carrying its masked pattern, the
    /// registration one its terms text) and is answered through the seam — no
    /// network, no live `tdjson`.
    #[tokio::test]
    async fn new_user_flow_does_email_then_registration() {
        let client = SpyClient::default();
        let mut login = Login::new(&client);

        login.on_update(&AuthorizationState::WaitEmailAddress(
            AuthorizationStateWaitEmailAddress::default(),
        ));
        assert_eq!(*login.state(), AuthState::WaitEmailAddress);
        login
            .submit_email_address("user@example.com".to_owned())
            .await
            .unwrap();

        login.on_update(&AuthorizationState::WaitEmailCode(
            AuthorizationStateWaitEmailCode {
                code_info: EmailAddressAuthenticationCodeInfo {
                    email_address_pattern: "u***@example.com".to_owned(),
                    length: 6,
                },
                ..Default::default()
            },
        ));
        assert_eq!(
            *login.state(),
            AuthState::WaitEmailCode {
                email_pattern: "u***@example.com".to_owned()
            }
        );
        login.submit_email_code("424242".to_owned()).await.unwrap();

        login.on_update(&AuthorizationState::WaitRegistration(
            AuthorizationStateWaitRegistration {
                terms_of_service: TermsOfService {
                    text: FormattedText {
                        text: "tos".to_owned(),
                        entities: vec![],
                    },
                    min_user_age: 0,
                    show_popup: false,
                },
            },
        ));
        assert_eq!(
            *login.state(),
            AuthState::WaitRegistration {
                terms_of_service: "tos".to_owned()
            }
        );
        login
            .register("Ada".to_owned(), "Lovelace".to_owned())
            .await
            .unwrap();

        login.on_update(&AuthorizationState::Ready);
        assert_eq!(*login.state(), AuthState::Ready);

        assert_eq!(
            client.calls(),
            vec![
                "set_authentication_email_address(user@example.com)".to_owned(),
                "check_authentication_email_code(424242)".to_owned(),
                "register_user(Ada,Lovelace)".to_owned(),
            ]
        );
    }
}
