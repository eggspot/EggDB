//! JMAP push (RFC 8620 §7): the state of each data type as a `StateChange`, an EventSource
//! stream that sends one whenever the account's state moves, and PushSubscriptions whose URLs
//! are verified and then sent each change.

use crate::json::{Json, parse, string};
use crate::store;
use eggdb_keyspace::{KeysWrite, Keyspace};
use std::collections::BTreeMap;
use std::time::Duration;

pub(crate) const PUSH_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const MOST_SUBSCRIPTION_SECONDS: u64 = 7 * 24 * 3600;
pub(crate) const MOST_SUBSCRIPTIONS: usize = 100;

/// The states a `StateChange` reports for the account, by type.
pub(crate) type States = Vec<(&'static str, String)>;

/// The `StateChange` object (RFC 8620 §7.1) for `account`'s `states`, keeping only `types`
/// (every type when `None`).
pub(crate) fn state_change(account: &str, states: &States, types: Option<&[String]>) -> String {
    let changed: Vec<String> = states
        .iter()
        .filter(|(kind, _)| types.is_none_or(|wanted| wanted.iter().any(|wanted| wanted == "*" || wanted == kind)))
        .map(|(kind, state)| format!("{}:{}", string(kind), string(state)))
        .collect();
    format!("{{\"@type\":\"StateChange\",\"changed\":{{{}:{{{}}}}}}}", string(account), changed.join(","))
}

/// Sends `body` with `POST` to `url`; whether the push service took it (any 2xx).
pub(crate) fn post(url: &str, body: &str) -> bool {
    eggdb_http::parse_http_url(url)
        .ok()
        .and_then(|target| eggdb_http::post(&target, "application/json", body.as_bytes(), PUSH_TIMEOUT).ok())
        .is_some_and(|status| (200..300).contains(&status))
}

/// A verification code for a new PushSubscription: 128 random bits as hex.
pub(crate) fn verification_code() -> Option<String> {
    let mut bytes = [0u8; 16];
    eggdb_platform::secure_random::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// One kept PushSubscription.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Subscription {
    pub(crate) id: String,
    pub(crate) url: String,
    pub(crate) types: Option<Vec<String>>,
    pub(crate) expires: u64,
    pub(crate) verified: bool,
    pub(crate) json: Json,
}

pub(crate) fn subscriptions(keyspace: &(impl Keyspace + ?Sized)) -> Vec<Subscription> {
    let mut found = Vec::new();
    let _ = keyspace.read(0, &mut |keys| {
        found = store::pushes(keys).map_err(|error| error.0)?;
        Ok(())
    });
    found
        .into_iter()
        .filter_map(|(id, text)| {
            let json = parse(&text)?;
            Some(Subscription {
                id,
                url: json.get("url")?.as_str()?.to_owned(),
                types: json.get("types").and_then(Json::as_array).map(|types| types.iter().filter_map(Json::as_str).map(str::to_owned).collect()),
                expires: json.get("expiresAt").and_then(Json::as_u64).unwrap_or(0),
                verified: json.get("verified") == Some(&Json::Bool(true)),
                json,
            })
        })
        .collect()
}

/// Sends a `StateChange` to every verified, unexpired subscription whose last sent state differs
/// from `states`, removing expired ones; `sent` remembers what each was last sent.
pub(crate) fn deliver(keyspace: &(impl Keyspace + ?Sized), account: &str, states: &States, now: u64, sent: &mut BTreeMap<String, String>) {
    let fingerprint: String = states.iter().map(|(kind, state)| format!("{kind}={state};")).collect();
    for subscription in subscriptions(keyspace) {
        if subscription.expires <= now {
            let _ = keyspace.write(0, &mut |keys: &mut dyn KeysWrite| store::remove_push(keys, &subscription.id).map(drop).map_err(|error| error.0));
            sent.remove(&subscription.id);
            continue;
        }
        if !subscription.verified {
            continue;
        }
        let previous = sent.entry(subscription.id.clone()).or_insert_with(|| fingerprint.clone());
        if *previous == fingerprint {
            continue;
        }
        if post(&subscription.url, &state_change(account, states, subscription.types.as_deref())) {
            *previous = fingerprint.clone();
        }
    }
}
