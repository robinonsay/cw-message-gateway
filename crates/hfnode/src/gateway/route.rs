//! How a TX reaches a contact.
//!
//! In this order, the first that applies:
//! 1. iMessage, if the contact has an `imessage` handle and Messages on this Mac is
//!    ready to send.
//! 2. A text from the node's Google Voice number, if the contact has a `phone` and
//!    has texted that number, so the node knows the conversation's reply address.
//! 3. Email to `address`; never a carrier email-to-SMS gateway when `phone` is set.
//!
//! At most one of 2 and 3 is tried: both go out through the same mail server, and
//! trying the other after an error could deliver the message twice. iMessage falls
//! through to them only when it failed before anything could have been sent.

use super::email::is_carrier_address;
use super::google_voice::GvStore;
use super::RouteKind;
use crate::config::{Contact, Handle, Phone};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    IMessage(Handle),
    /// The contact's Google Voice reply address.
    GoogleVoice(String),
    Email(String),
}

impl Route {
    pub fn kind(&self) -> &'static str {
        match self {
            Route::IMessage(_) => "iMessage",
            Route::GoogleVoice(_) => "Google Voice",
            Route::Email(_) => "email",
        }
    }
}

/// What this node can send through right now.
#[derive(Debug, Clone, Copy)]
pub struct Avail<'a> {
    /// `Err` says why iMessage cannot send from here now.
    pub imessage: Result<(), &'a str>,
    /// The node's Google Voice number, when `[google_voice]` is set and there is a
    /// mailer to answer texts with.
    pub gv_number: Option<&'a Phone>,
    /// There is a mailer.
    pub email: bool,
}

/// The routes to try for `c`, in order, or why there is none.
pub fn plan(c: &Contact, a: &Avail, gv: &GvStore) -> Result<Vec<Route>, String> {
    let mut routes = Vec::new();
    let mut notes = Vec::new();
    if let Some(h) = c.imessage.first() {
        match a.imessage {
            Ok(()) => routes.push(Route::IMessage(h.clone())),
            Err(w) => notes.push(format!("iMessage cannot send from here: {w}")),
        }
    }
    let mut smtp = false;
    match (&c.phone, a.gv_number) {
        (Some(p), Some(n)) => match gv.reply_address(p, n) {
            Some(addr) => {
                routes.push(Route::GoogleVoice(addr));
                smtp = true;
            }
            None => notes.push(format!(
                "no Google Voice reply address yet for {p}: have {} text the node's number {n} once",
                c.name
            )),
        },
        (Some(_), None) => notes.push("phone is set but [google_voice] is not configured here".into()),
        (None, _) => {}
    }
    if let (false, Some(addr)) = (smtp, &c.address) {
        if c.phone.is_some() && is_carrier_address(addr) {
            notes.push(format!(
                "{addr} is a carrier email-to-SMS gateway, not used when phone is set"
            ));
        } else if a.email {
            routes.push(Route::Email(addr.clone()));
        } else {
            notes.push("[email] is not configured here".into());
        }
    }
    if routes.is_empty() {
        return Err(notes.join("; "));
    }
    Ok(routes)
}

/// [`plan`], keeping only the routes of kind `only` when it is given: for `hfnode
/// messages send --via`.
pub fn plan_only(
    c: &Contact,
    mut a: Avail,
    gv: &GvStore,
    only: Option<RouteKind>,
) -> Result<Vec<Route>, String> {
    if only == Some(RouteKind::Email) {
        // Email is planned only when there is no Google Voice route; asked for by
        // name, it is the one route tried.
        a.gv_number = None;
    }
    let mut routes = plan(c, &a, gv)?;
    if let Some(k) = only {
        routes.retain(|r| RouteKind::of(r) == k);
        if routes.is_empty() {
            return Err(format!(
                "{} cannot be reached that way from here now",
                c.name
            ));
        }
    }
    Ok(routes)
}

/// One line on how TX would reach `c`: for `hfnode messages check`, the code table
/// and the node's start-up log.
pub fn describe(c: &Contact, a: &Avail, gv: &GvStore) -> String {
    match plan(c, a, gv) {
        Err(why) => format!("NO ROUTE: {why}"),
        Ok(routes) => routes
            .iter()
            .map(|r| match r {
                Route::IMessage(h) => format!("iMessage {h}"),
                Route::GoogleVoice(_) => {
                    let learned = c
                        .phone
                        .as_ref()
                        .and_then(|p| gv.contacts.get(p.as_str()))
                        .map(|e| format!(" (learned {})", super::date(e.learned_unix)))
                        .unwrap_or_default();
                    format!("Google Voice{learned}")
                }
                Route::Email(a) => format!("email {a}"),
            })
            .collect::<Vec<_>>()
            .join(", then "),
    }
}

#[cfg(test)]
mod tests {
    use super::super::google_voice::GvEntry;
    use super::*;

    const ADDR: &str = "15550001111.15551234567.AbCdEf1234@txt.voice.google.com";

    fn gv_number() -> Phone {
        Phone::parse("+15550001111").unwrap()
    }

    fn learned() -> GvStore {
        let mut s = GvStore::default();
        s.contacts.insert(
            "+15551234567".into(),
            GvEntry {
                address: ADDR.into(),
                learned_unix: 1_790_000_000,
                uid: 4,
            },
        );
        s
    }

    fn contact(imessage: bool, phone: bool, address: Option<&str>) -> Contact {
        Contact {
            name: "MOM".into(),
            address: address.map(str::to_string),
            phone: phone.then(|| Phone::parse("+15551234567").unwrap()),
            imessage: if imessage {
                vec![Handle::parse("+1 555 123 4567").unwrap()]
            } else {
                Vec::new()
            },
        }
    }

    #[test]
    fn every_combination() {
        let n = gv_number();
        let store = learned();
        let empty = GvStore::default();
        for im in [false, true] {
            for phone in [false, true] {
                for address in [None, Some("mom@example.com"), Some("5551234567@vtext.com")] {
                    for im_ok in [false, true] {
                        for gv_on in [false, true] {
                            for is_learned in [false, true] {
                                for email in [false, true] {
                                    let c = contact(im, phone, address);
                                    if !im && !phone && address.is_none() {
                                        continue;
                                    }
                                    let a = Avail {
                                        imessage: if im_ok { Ok(()) } else { Err("not ready") },
                                        gv_number: (gv_on && email).then_some(&n),
                                        email,
                                    };
                                    let gv = if is_learned { &store } else { &empty };
                                    let r = plan(&c, &a, gv);
                                    let case = format!(
                                        "im {im} phone {phone} {address:?} im_ok {im_ok} gv {gv_on} learned {is_learned} email {email}: {r:?}"
                                    );
                                    let routes = r.clone().unwrap_or_default();
                                    // iMessage first, when it can be used at all.
                                    assert_eq!(
                                        routes
                                            .first()
                                            .is_some_and(|r| matches!(r, Route::IMessage(_))),
                                        im && im_ok,
                                        "{case}"
                                    );
                                    // At most one route through the mail server.
                                    let smtp = routes
                                        .iter()
                                        .filter(|r| !matches!(r, Route::IMessage(_)))
                                        .count();
                                    assert!(smtp <= 1, "{case}");
                                    let gv_route = phone && gv_on && email && is_learned;
                                    assert_eq!(
                                        routes
                                            .iter()
                                            .any(|r| *r == Route::GoogleVoice(ADDR.into())),
                                        gv_route,
                                        "{case}"
                                    );
                                    // Never a carrier gateway when phone is set.
                                    if phone {
                                        assert!(
                                            !routes.contains(&Route::Email(
                                                "5551234567@vtext.com".into()
                                            )),
                                            "{case}"
                                        );
                                    }
                                    let email_route = !gv_route
                                        && email
                                        && match address {
                                            Some(a) => !(phone && is_carrier_address(a)),
                                            None => false,
                                        };
                                    assert_eq!(
                                        routes.iter().any(|r| matches!(r, Route::Email(_))),
                                        email_route,
                                        "{case}"
                                    );
                                    assert_eq!(r.is_err(), routes.is_empty(), "{case}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn no_route_says_what_to_do() {
        let n = gv_number();
        let a = Avail {
            imessage: Err("not on a Mac"),
            gv_number: Some(&n),
            email: true,
        };
        let why = plan(&contact(false, true, None), &a, &GvStore::default()).unwrap_err();
        assert!(
            why.contains("have MOM text the node's number +15550001111"),
            "{why}"
        );
        assert!(
            describe(&contact(false, true, None), &a, &GvStore::default())
                .starts_with("NO ROUTE: ")
        );
        let why = plan(&contact(true, false, None), &a, &GvStore::default()).unwrap_err();
        assert!(why.contains("not on a Mac"), "{why}");
    }

    #[test]
    fn a_stale_address_is_not_used() {
        // The node's Google Voice number changed since the address was learned.
        let other = Phone::parse("+15550002222").unwrap();
        let a = Avail {
            imessage: Ok(()),
            gv_number: Some(&other),
            email: true,
        };
        assert!(plan(&contact(false, true, None), &a, &learned()).is_err());
    }

    #[test]
    fn one_kind_only() {
        let n = gv_number();
        let a = Avail {
            imessage: Ok(()),
            gv_number: Some(&n),
            email: true,
        };
        let mom = contact(true, true, Some("mom@example.com"));
        let only = |k| plan_only(&mom, a, &learned(), Some(k));
        assert_eq!(
            only(RouteKind::IMessage).unwrap(),
            [Route::IMessage(mom.imessage[0].clone())]
        );
        assert_eq!(
            only(RouteKind::GoogleVoice).unwrap(),
            [Route::GoogleVoice(ADDR.into())]
        );
        // Not part of TX's plan once Google Voice is learned, but there when asked for.
        assert_eq!(
            only(RouteKind::Email).unwrap(),
            [Route::Email("mom@example.com".into())]
        );
        assert_eq!(plan_only(&mom, a, &learned(), None).unwrap().len(), 2);
        let bob = contact(false, false, Some("bob@example.com"));
        let why = plan_only(&bob, a, &learned(), Some(RouteKind::IMessage)).unwrap_err();
        assert!(why.contains("cannot be reached that way"), "{why}");
    }

    #[test]
    fn description() {
        let n = gv_number();
        let a = Avail {
            imessage: Ok(()),
            gv_number: Some(&n),
            email: true,
        };
        assert_eq!(
            describe(&contact(true, true, None), &a, &learned()),
            "iMessage +15551234567, then Google Voice (learned 2026-09-21)"
        );
        assert_eq!(
            describe(
                &contact(false, false, Some("bob@example.com")),
                &a,
                &learned()
            ),
            "email bob@example.com"
        );
    }
}
