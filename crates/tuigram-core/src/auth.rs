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
//! Phase 2 scope is **phone number + login code + 2FA password**. QR login
//! (`waitOtherDeviceConfirmation`), new-user registration, email, and premium
//! purchase are surfaced as [`AuthState::Unsupported`] for follow-up issues
//! rather than handled here.
//!
//! Secrets are never retained: the login code and the 2FA password are taken by
//! value and moved straight into their TDLib request (see the threat model).

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

    /// Answer `WaitCode` — submit the login code the user received.
    async fn check_authentication_code(&self, code: String) -> Result<(), TdError>;

    /// Answer `WaitPassword` — submit the 2FA password.
    ///
    /// The password is moved straight into the TDLib request and never retained
    /// (see the threat model).
    async fn check_authentication_password(&self, password: String) -> Result<(), TdError>;
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

    async fn check_authentication_code(&self, code: String) -> Result<(), TdError> {
        tdlib_rs::functions::check_authentication_code(code, self.id()).await
    }

    async fn check_authentication_password(&self, password: String) -> Result<(), TdError> {
        tdlib_rs::functions::check_authentication_password(password, self.id()).await
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
    /// Logged in; normal updates flow.
    Ready,
    /// Logging out, closing, or closed — terminal; tear down the session.
    Closed,
    /// A login state outside Phase 2 scope (QR confirmation, new-user
    /// registration, email, premium purchase). Carries the TDLib state name so
    /// callers can report precisely. Tracked as follow-up issues.
    Unsupported(&'static str),
}

impl AuthState {
    /// Project a TDLib [`AuthorizationState`] onto tuigram's [`AuthState`].
    ///
    /// Total over TDLib's enum: every state maps to a handled variant or to
    /// [`AuthState::Unsupported`], so a new TDLib state can never silently
    /// masquerade as a handled one.
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
            AuthorizationState::WaitOtherDeviceConfirmation(_) => {
                Self::Unsupported("waitOtherDeviceConfirmation")
            }
            AuthorizationState::WaitRegistration(_) => Self::Unsupported("waitRegistration"),
            AuthorizationState::WaitEmailAddress(_) => Self::Unsupported("waitEmailAddress"),
            AuthorizationState::WaitEmailCode(_) => Self::Unsupported("waitEmailCode"),
            AuthorizationState::WaitPremiumPurchase(_) => Self::Unsupported("waitPremiumPurchase"),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use tdlib_rs::enums::AuthenticationCodeType;
    use tdlib_rs::types::{
        AuthenticationCodeInfo, AuthenticationCodeTypeSms, AuthorizationStateWaitCode,
        AuthorizationStateWaitOtherDeviceConfirmation, AuthorizationStateWaitPassword,
        AuthorizationStateWaitRegistration,
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
    fn out_of_scope_states_are_unsupported_not_misclassified() {
        let qr = AuthorizationState::WaitOtherDeviceConfirmation(
            AuthorizationStateWaitOtherDeviceConfirmation::default(),
        );
        assert_eq!(
            AuthState::from_tdlib(&qr),
            AuthState::Unsupported("waitOtherDeviceConfirmation")
        );

        let registration =
            AuthorizationState::WaitRegistration(AuthorizationStateWaitRegistration::default());
        assert_eq!(
            AuthState::from_tdlib(&registration),
            AuthState::Unsupported("waitRegistration")
        );
    }

    #[test]
    fn ready_and_closed_are_terminal_others_are_not() {
        assert!(AuthState::Ready.is_terminal());
        assert!(AuthState::Closed.is_terminal());
        assert!(!AuthState::WaitPhoneNumber.is_terminal());
        assert!(!AuthState::WaitCode.is_terminal());
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
}
