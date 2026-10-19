//! search_contacts: macOS Contacts and WhatsApp's address book, merged into people.
use crate::common::{ContactRecord, address_contains, address_key, display_address};
use crate::types::{Contact, ContactName, NameSource};
use crate::whatsapp::AddressBookEntry;
use std::collections::{BTreeSet, HashMap};

/// One entry from either source, before merging.
struct Entry {
    name: ContactName,
    addresses: Vec<String>,
    jids: Vec<String>,
}

/// Merge entries that share an address or JID into people and return those matching
/// `query`, sorted by first name, at most `cap` of them, and whether that was all of them.
pub fn search(
    records: Vec<ContactRecord>,
    whatsapp: Vec<AddressBookEntry>,
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
        })
        .chain(whatsapp.into_iter().map(|w| Entry {
            name: ContactName {
                name: w.name,
                source: NameSource::Whatsapp,
            },
            addresses: w.phone.into_iter().collect(),
            jids: w.jids,
        }))
        .collect();

    // Union-find over entries, joined through every address and JID they share.
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
        let keys = entry.addresses.iter().map(|a| address_key(a)).chain(
            entry
                .jids
                .iter()
                .map(|j| format!("jid:{}", j.to_lowercase())),
        );
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
    // Sorted and deduplicated: Contacts names before WhatsApp ones, each alphabetically.
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
        for jid in &entry.jids {
            if !jids.contains(jid) {
                jids.push(jid.clone());
            }
        }
    }
    Contact {
        in_contacts: names.iter().any(|n| n.source == NameSource::Contacts),
        names,
        phone_numbers: phones,
        email_addresses: emails,
        whatsapp_jids: jids,
    }
}
