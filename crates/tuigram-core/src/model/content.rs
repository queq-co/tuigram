//! Structured, non-file message content: [`Location`], [`Venue`], [`Contact`],
//! [`Poll`].

use tdlib_rs::enums::PollType as TdPollType;
use tdlib_rs::types::{
    Contact as TdContact, Location as TdLocation, Poll as TdPoll, PollOption as TdPollOption,
    Venue as TdVenue,
};

use super::richtext::FormattedText;

/// A geographic point — tuigram's projection of `TDLib`'s `Location`. Reused by
/// both a [`MessageContent::Location`](super::message::MessageContent::Location)
/// message and a [`Venue`].
///
/// This is the static point only; `TDLib`'s live-location fields (update period,
/// heading, proximity radius) live on the message wrapper and are dropped — a
/// live location and a static one project alike here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Location {
    /// Latitude, in degrees.
    pub latitude: f64,
    /// Longitude, in degrees.
    pub longitude: f64,
    /// Estimated horizontal accuracy, in meters; `0.0` if the sender gave none.
    pub horizontal_accuracy: f64,
}

impl Location {
    /// Project `TDLib`'s `location`.
    #[must_use]
    pub fn from_tdlib(l: &TdLocation) -> Self {
        Self {
            latitude: l.latitude,
            longitude: l.longitude,
            horizontal_accuracy: l.horizontal_accuracy,
        }
    }
}

/// A venue — a named place at a [`Location`] — projecting `TDLib`'s `Venue`. The
/// provider-database fields (`provider`, `id`, `type`) are dropped; a client
/// shows the title, address, and point.
#[derive(Clone, Debug, PartialEq)]
pub struct Venue {
    /// Where the venue is.
    pub location: Location,
    /// Venue name, as given by the sender (may be empty).
    pub title: String,
    /// Venue address, as given by the sender (may be empty).
    pub address: String,
}

impl Venue {
    /// Project `TDLib`'s `venue`.
    #[must_use]
    pub fn from_tdlib(v: &TdVenue) -> Self {
        Self {
            location: Location::from_tdlib(&v.location),
            title: crate::sanitize::scrub_line(&v.title),
            address: crate::sanitize::scrub_line(&v.address),
        }
    }
}

/// A shared contact card — tuigram's projection of `TDLib`'s `Contact`. The vCard
/// blob is dropped; the model keeps the name, phone, and the Telegram user id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contact {
    /// First name (1–64 characters).
    pub first_name: String,
    /// Last name (may be empty).
    pub last_name: String,
    /// Phone number.
    pub phone_number: String,
    /// The contact's Telegram user id, or `0` if it is not a known user.
    pub user_id: i64,
}

impl Contact {
    /// Project `TDLib`'s `contact`.
    #[must_use]
    pub fn from_tdlib(c: &TdContact) -> Self {
        Self {
            first_name: crate::sanitize::scrub_line(&c.first_name),
            last_name: crate::sanitize::scrub_line(&c.last_name),
            phone_number: crate::sanitize::scrub_line(&c.phone_number),
            user_id: c.user_id,
        }
    }
}

/// One answer option in a [`Poll`] — tuigram's projection of `TDLib`'s
/// `pollOption`. Vote counts are meaningful only once the poll is voted in or
/// closed; the transient "being chosen by a pending request" flag is dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollOption {
    /// The option text.
    pub text: FormattedText,
    /// Number of voters who chose this option.
    pub voter_count: i32,
    /// Share of the vote for this option, 0–100.
    pub vote_percentage: i32,
    /// Whether this account chose this option.
    pub is_chosen: bool,
}

impl PollOption {
    /// Project `TDLib`'s `pollOption`.
    #[must_use]
    pub fn from_tdlib(o: &TdPollOption) -> Self {
        Self {
            text: FormattedText::from_tdlib(&o.text),
            voter_count: o.voter_count,
            vote_percentage: o.vote_percentage,
            is_chosen: o.is_chosen,
        }
    }
}

/// What kind of poll a [`Poll`] is — tuigram's projection of `TDLib`'s `PollType`.
/// Total over the enum, no catch-all: a new poll type fails to compile here
/// until it is classified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PollKind {
    /// A regular poll.
    Regular {
        /// Whether more than one option may be chosen at once.
        allow_multiple_answers: bool,
    },
    /// A quiz: exactly one correct option, answerable once.
    Quiz {
        /// 0-based index of the correct option; `-1` until answered.
        correct_option_id: i32,
        /// Text shown after an incorrect answer (may be empty).
        explanation: FormattedText,
    },
}

impl PollKind {
    /// Project `TDLib`'s `PollType`.
    #[must_use]
    pub fn from_tdlib(kind: &TdPollType) -> Self {
        match kind {
            TdPollType::Regular(r) => Self::Regular {
                allow_multiple_answers: r.allow_multiple_answers,
            },
            TdPollType::Quiz(q) => Self::Quiz {
                correct_option_id: q.correct_option_id,
                explanation: FormattedText::from_tdlib(&q.explanation),
            },
        }
    }
}

/// A poll or quiz — tuigram's projection of `TDLib`'s `Poll`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Poll {
    /// The poll question.
    pub question: FormattedText,
    /// The answer options, in the order `TDLib` lists them.
    pub options: Vec<PollOption>,
    /// Total number of voters across all options.
    pub total_voter_count: i32,
    /// Whether votes are anonymous.
    pub is_anonymous: bool,
    /// Whether the poll is closed (no longer accepting votes).
    pub is_closed: bool,
    /// Whether this is a regular poll or a quiz.
    pub kind: PollKind,
}

impl Poll {
    /// Project `TDLib`'s `poll`.
    #[must_use]
    pub fn from_tdlib(p: &TdPoll) -> Self {
        Self {
            question: FormattedText::from_tdlib(&p.question),
            options: p.options.iter().map(PollOption::from_tdlib).collect(),
            total_voter_count: p.total_voter_count,
            is_anonymous: p.is_anonymous,
            is_closed: p.is_closed,
            kind: PollKind::from_tdlib(&p.r#type),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests: panicking on a broken assumption is the point
mod tests {
    use super::*;
    use crate::model::message::MessageContent;
    use tdlib_rs::enums::MessageContent as TdMessageContent;
    use tdlib_rs::types::FormattedText as TdFormattedTextT;

    #[test]
    fn location_content_projects_coordinates() {
        // Live-location fields on the message wrapper are dropped; only the point
        // survives, so a live and a static location project identically.
        let content = TdMessageContent::MessageLocation(tdlib_rs::types::MessageLocation {
            location: TdLocation {
                latitude: 51.5,
                longitude: -0.12,
                horizontal_accuracy: 8.0,
            },
            live_period: 900,
            expires_in: 60,
            heading: 90,
            proximity_alert_radius: 0,
        });
        assert_eq!(
            MessageContent::from_tdlib(&content),
            MessageContent::Location(Location {
                latitude: 51.5,
                longitude: -0.12,
                horizontal_accuracy: 8.0,
            })
        );
    }

    #[test]
    fn venue_content_projects_title_address_and_point() {
        let content = TdMessageContent::MessageVenue(tdlib_rs::types::MessageVenue {
            venue: TdVenue {
                location: TdLocation {
                    latitude: 40.0,
                    longitude: -73.0,
                    horizontal_accuracy: 0.0,
                },
                title: "Central Park".to_owned(),
                address: "New York".to_owned(),
                provider: "foursquare".to_owned(),
                id: "abc123".to_owned(),
                r#type: "park".to_owned(),
            },
        });
        assert_eq!(
            MessageContent::from_tdlib(&content),
            MessageContent::Venue(Venue {
                location: Location {
                    latitude: 40.0,
                    longitude: -73.0,
                    horizontal_accuracy: 0.0,
                },
                title: "Central Park".to_owned(),
                address: "New York".to_owned(),
            })
        );
    }

    #[test]
    fn contact_content_projects_name_phone_and_user_id() {
        let content = TdMessageContent::MessageContact(tdlib_rs::types::MessageContact {
            contact: TdContact {
                phone_number: "+15551234".to_owned(),
                first_name: "Ada".to_owned(),
                last_name: "Lovelace".to_owned(),
                vcard: "BEGIN:VCARD…".to_owned(),
                user_id: 7,
            },
        });
        assert_eq!(
            MessageContent::from_tdlib(&content),
            MessageContent::Contact(Contact {
                first_name: "Ada".to_owned(),
                last_name: "Lovelace".to_owned(),
                phone_number: "+15551234".to_owned(),
                user_id: 7,
            })
        );
    }

    /// A `TDLib` `pollOption` with the fields a test cares about.
    fn td_poll_option(text: &str, voter_count: i32, percentage: i32, chosen: bool) -> TdPollOption {
        TdPollOption {
            text: TdFormattedTextT {
                text: text.to_owned(),
                entities: vec![],
            },
            voter_count,
            vote_percentage: percentage,
            is_chosen: chosen,
            is_being_chosen: false,
        }
    }

    #[test]
    fn poll_content_projects_question_options_and_votes() {
        let content = TdMessageContent::MessagePoll(tdlib_rs::types::MessagePoll {
            poll: TdPoll {
                id: 99,
                question: TdFormattedTextT {
                    text: "Tabs or spaces?".to_owned(),
                    entities: vec![],
                },
                options: vec![
                    td_poll_option("Tabs", 3, 30, false),
                    td_poll_option("Spaces", 7, 70, true),
                ],
                total_voter_count: 10,
                recent_voter_ids: vec![],
                is_anonymous: true,
                r#type: TdPollType::Regular(tdlib_rs::types::PollTypeRegular {
                    allow_multiple_answers: false,
                }),
                open_period: 0,
                close_date: 0,
                is_closed: false,
            },
        });
        assert_eq!(
            MessageContent::from_tdlib(&content),
            MessageContent::Poll(Poll {
                question: FormattedText {
                    text: "Tabs or spaces?".to_owned(),
                    entities: vec![],
                },
                options: vec![
                    PollOption {
                        text: FormattedText {
                            text: "Tabs".to_owned(),
                            entities: vec![],
                        },
                        voter_count: 3,
                        vote_percentage: 30,
                        is_chosen: false,
                    },
                    PollOption {
                        text: FormattedText {
                            text: "Spaces".to_owned(),
                            entities: vec![],
                        },
                        voter_count: 7,
                        vote_percentage: 70,
                        is_chosen: true,
                    },
                ],
                total_voter_count: 10,
                is_anonymous: true,
                is_closed: false,
                kind: PollKind::Regular {
                    allow_multiple_answers: false,
                },
            })
        );
    }

    #[test]
    fn quiz_poll_projects_quiz_kind_with_correct_option_and_explanation() {
        let content = TdMessageContent::MessagePoll(tdlib_rs::types::MessagePoll {
            poll: TdPoll {
                id: 1,
                question: TdFormattedTextT {
                    text: "2 + 2?".to_owned(),
                    entities: vec![],
                },
                options: vec![td_poll_option("4", 0, 0, false)],
                total_voter_count: 0,
                recent_voter_ids: vec![],
                is_anonymous: false,
                r#type: TdPollType::Quiz(tdlib_rs::types::PollTypeQuiz {
                    correct_option_id: 0,
                    explanation: TdFormattedTextT {
                        text: "basic arithmetic".to_owned(),
                        entities: vec![],
                    },
                }),
                open_period: 0,
                close_date: 0,
                is_closed: true,
            },
        });
        let MessageContent::Poll(poll) = MessageContent::from_tdlib(&content) else {
            panic!("expected a poll");
        };
        assert_eq!(
            poll.kind,
            PollKind::Quiz {
                correct_option_id: 0,
                explanation: FormattedText {
                    text: "basic arithmetic".to_owned(),
                    entities: vec![],
                },
            }
        );
        assert!(poll.is_closed);
    }
}
