//! search_contacts: macOS Contacts and the messaging apps' own address books (WhatsApp's,
//! Telegram's), merged into people.
use crate::common::{ContactRecord, address_contains, address_key, display_address};
use crate::types::{Contact, ContactName, NameSource};
use std::collections::{BTreeSet, HashMap};

/// One person in a messaging app's own address book.
pub struct AddressBookEntry {
    pub source: NameSource,
    pub name: String,
    /// E.164 when the country is known.
    pub phone: Option<String>,
    /// WhatsApp: normalized JIDs, the phone JID and the `@lid` identity.
    pub jids: Vec<String>,
    /// Telegram: the user ID that Telegram users see.
    pub telegram_id: Option<String>,
    /// Telegram: the public username, without the `@`.
    pub telegram_username: Option<String>,
}

/// One entry from any source, before merging.
struct Entry {
    name: ContactName,
    addresses: Vec<String>,
    jids: Vec<String>,
    telegram_ids: Vec<String>,
    telegram_usernames: Vec<String>,
}

/// Merge entries that share an address, JID or Telegram ID into people and return those matching
/// `query`, sorted by first name, at most `cap` of them, and whether that was all of them.
pub fn search(
    records: Vec<ContactRecord>,
    books: Vec<AddressBookEntry>,
    query: &str,
    cap: usize,
) -> (Vec<Contact>, bool) {
    let entries: Vec<Entry> = records
        .into_iter()
        .map(|r| Entry {
            name: ContactName {
                name: r.name,
                source: NameSource::Contacts,
            },
            addresses: r.addresses,
            jids: Vec::new(),
            telegram_ids: Vec::new(),
            telegram_usernames: Vec::new(),
        })
        .chain(books.into_iter().map(|b| Entry {
            name: ContactName {
                name: b.name,
                source: b.source,
            },
            addresses: b.phone.into_iter().collect(),
            jids: b.jids,
            telegram_ids: b.telegram_id.into_iter().collect(),
            telegram_usernames: b.telegram_username.into_iter().collect(),
        }))
        .collect();

    // Union-find over entries, joined through every address, JID and Telegram ID they share.
    let mut parent: Vec<usize> = (0..entries.len()).collect();
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let mut owner = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        let keys = entry
            .addresses
            .iter()
            .map(|a| address_key(a))
            .chain(
                entry
                    .jids
                    .iter()
                    .map(|j| format!("jid:{}", j.to_lowercase())),
            )
            .chain(entry.telegram_ids.iter().map(|id| format!("telegram:{id}")));
        for key in keys {
            let first = *owner.entry(key).or_insert(i);
            let (a, b) = (root(&mut parent, first), root(&mut parent, i));
            parent[b] = a;
        }
    }
    let mut groups: HashMap<usize, Vec<Entry>> = HashMap::new();
    for (i, entry) in entries.into_iter().enumerate() {
        groups.entry(root(&mut parent, i)).or_default().push(entry);
    }

    let query = query.trim().to_lowercase();
    // A Telegram username matches with or without its `@`.
    let username = query.strip_prefix('@').unwrap_or(&query);
    let mut people: Vec<Contact> = groups
        .into_values()
        .map(person)
        .filter(|c| {
            c.names
                .iter()
                .any(|n| n.name.to_lowercase().contains(&query))
                || c.phone_numbers
                    .iter()
                    .chain(&c.email_addresses)
                    .any(|a| address_contains(a, &query))
                || c.whatsapp_jids.iter().any(|j| j.contains(&query))
                || c.telegram_ids.contains(&query)
                || (!username.is_empty()
                    && c.telegram_usernames
                        .iter()
                        .any(|u| u.to_lowercase().contains(username)))
        })
        .collect();
    people.sort_by_cached_key(|c| {
        (
            c.names[0].name.to_lowercase(),
            c.phone_numbers
                .first()
                .or(c.email_addresses.first())
                .cloned(),
        )
    });
    let complete = people.len() <= cap;
    people.truncate(cap);
    (people, complete)
}

fn person(entries: Vec<Entry>) -> Contact {
    // Sorted and deduplicated: Contacts names, then WhatsApp's, then Telegram's, each
    // alphabetically.
    let names: BTreeSet<ContactName> = entries
        .iter()
        .map(|e| ContactName {
            name: e.name.name.clone(),
            source: e.name.source,
        })
        .collect();
    let mut names: Vec<ContactName> = names.into_iter().collect();
    names.sort_by_key(|n| (n.source, n.name.to_lowercase()));
    let (mut phones, mut emails, mut jids) = (Vec::new(), Vec::new(), Vec::new());
    let (mut telegram_ids, mut telegram_usernames) = (Vec::new(), Vec::new());
    let mut seen = BTreeSet::new();
    for entry in &entries {
        for address in &entry.addresses {
            if seen.insert(address_key(address)) {
                if address.contains('@') {
                    emails.push(address.trim().to_owned());
                } else {
                    phones.push(display_address(address.trim()));
                }
            }
        }
        for (from, to) in [
            (&entry.jids, &mut jids),
            (&entry.telegram_ids, &mut telegram_ids),
            (&entry.telegram_usernames, &mut telegram_usernames),
        ] {
            for value in from {
                if !to.contains(value) {
                    to.push(value.clone());
                }
            }
        }
    }
    Contact {
        in_contacts: names.iter().any(|n| n.source == NameSource::Contacts),
        names,
        phone_numbers: phones,
        email_addresses: emails,
        whatsapp_jids: jids,
        telegram_ids,
        telegram_usernames,
    }
}
