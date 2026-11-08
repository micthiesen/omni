//! The read view of a vCard: identity, single fields, labeled values with
//! their line ids, group membership, and the names of every other property
//! (kept on edits, never interpreted).

use std::collections::BTreeMap;

use crate::labels::{self, Field};
use crate::vcard::{Card, Prop};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Contact,
    /// An Apple group (a contact list): `X-ADDRESSBOOKSERVER-KIND:group`, or
    /// vCard 4 `KIND:group`.
    Group,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NameParts {
    pub family: String,
    pub given: String,
    pub middle: String,
    pub prefix: String,
    pub suffix: String,
}

/// ADR components (RFC 6350 6.3.1) plus Apple's country code.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AddressParts {
    pub po_box: String,
    pub extended: String,
    pub street: String,
    pub city: String,
    pub region: String,
    pub postal_code: String,
    pub country: String,
    pub country_code: Option<String>,
}

impl AddressParts {
    pub fn from_components(components: &[String]) -> Self {
        let at = |i: usize| components.get(i).cloned().unwrap_or_default();
        Self {
            po_box: at(0),
            extended: at(1),
            street: at(2),
            city: at(3),
            region: at(4),
            postal_code: at(5),
            country: at(6),
            country_code: None,
        }
    }

    /// One line for display and search.
    pub fn display(&self) -> String {
        [
            &self.po_box,
            &self.extended,
            &self.street,
            &self.city,
            &self.region,
            &self.postal_code,
            &self.country,
        ]
        .iter()
        .map(|s| s.replace('\n', ", "))
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join(", ")
    }
}

/// One labeled value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// The line id (see [`Card::line_id`]).
    pub id: String,
    pub field: Field,
    /// The decoded value (an address's one-line form).
    pub value: String,
    pub label: Option<String>,
    /// The Apple token as stored, for a built-in `X-ABLabel`.
    pub label_token: Option<String>,
    pub custom_label: bool,
    pub preferred: bool,
    pub address: Option<AddressParts>,
    /// A social profile's or IM address's `x-user` parameter.
    pub user: Option<String>,
    /// The service an IM address or social profile belongs to
    /// (`X-SERVICE-TYPE`, or a social profile's non-label TYPE).
    pub service: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Projection {
    pub kind: Kind,
    pub version: Option<String>,
    pub uid: Option<String>,
    pub full_name: Option<String>,
    pub name: Option<NameParts>,
    pub nickname: Option<String>,
    pub organization: Option<String>,
    pub department: Option<String>,
    pub job_title: Option<String>,
    /// `YYYY-MM-DD`, or `--MM-DD` when Apple's X-APPLE-OMIT-YEAR hides the year.
    pub birthday: Option<String>,
    pub birthday_year_omitted: bool,
    pub note: Option<String>,
    pub items: Vec<Item>,
    /// A group's member UIDs, in card order.
    pub members: Vec<String>,
    /// Every other property name (uppercased) and how often it appears.
    pub other: BTreeMap<String, u32>,
    pub has_photo: bool,
}

/// Properties the projection interprets.
const INTERPRETED: &[&str] = &[
    "BEGIN",
    "END",
    "VERSION",
    "UID",
    "FN",
    "N",
    "NICKNAME",
    "ORG",
    "TITLE",
    "BDAY",
    "NOTE",
    "KIND",
    "MEMBER",
    "X-ADDRESSBOOKSERVER-KIND",
    "X-ADDRESSBOOKSERVER-MEMBER",
];

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// The UID a group member line names (`urn:uuid:` prefix removed).
pub fn member_uid(prop: &Prop) -> Option<String> {
    let text = prop.text();
    let trimmed = text.trim();
    let uid = match trimmed.get(..9) {
        Some(prefix) if prefix.eq_ignore_ascii_case("urn:uuid:") => &trimmed[9..],
        _ => trimmed,
    };
    non_empty(uid.to_owned())
}

/// The birthday as shown and whether its year is hidden: Apple stores a
/// birthday without a year as `BDAY;X-APPLE-OMIT-YEAR=1604:1604-03-15`.
pub fn birthday(prop: &Prop) -> (Option<String>, bool) {
    let value = prop.value.trim();
    if value.is_empty() {
        return (None, false);
    }
    if let Some(year) = prop.param("X-APPLE-OMIT-YEAR")
        && let Some(rest) = value.strip_prefix(year.trim())
        && rest.starts_with('-')
    {
        return (Some(format!("-{rest}")), true);
    }
    (Some(value.to_owned()), false)
}

/// The service of an IM address or social profile.
fn service(field: Field, prop: &Prop) -> Option<String> {
    if let Some(service) = prop.param("X-SERVICE-TYPE") {
        return non_empty(service.to_owned());
    }
    if field == Field::SocialProfile {
        return prop
            .types()
            .into_iter()
            .find(|t| t != "PREF" && !labels::is_label_type(t))
            .map(|t| t.to_ascii_lowercase());
    }
    None
}

pub fn is_member_prop(prop: &Prop) -> bool {
    prop.is("X-ADDRESSBOOKSERVER-MEMBER") || prop.is("MEMBER")
}

pub fn kind_of(card: &Card) -> Kind {
    let group = card.props().any(|(_, p)| {
        (p.is("X-ADDRESSBOOKSERVER-KIND") || p.is("KIND"))
            && p.text().trim().eq_ignore_ascii_case("group")
    });
    if group { Kind::Group } else { Kind::Contact }
}

pub fn project(card: &Card) -> Projection {
    let kind = kind_of(card);
    let single = |name: &str| card.first(name).and_then(|p| non_empty(p.text()));
    let name = card.first("N").map(|p| {
        let c = p.components();
        let at = |i: usize| c.get(i).cloned().unwrap_or_default();
        NameParts {
            family: at(0),
            given: at(1),
            middle: at(2),
            prefix: at(3),
            suffix: at(4),
        }
    });
    let org = card.first("ORG").map(Prop::components).unwrap_or_default();
    let mut items = Vec::new();
    let mut label_lines = Vec::new();
    for (index, prop) in card.props() {
        let Some(field) = Field::of(prop) else {
            continue;
        };
        let label = labels::read(card, field, prop);
        if let Some(sibling) = labels::label_sibling(card, prop) {
            label_lines.push(sibling);
        }
        let (value, address) = if field == Field::Address {
            let mut parts = AddressParts::from_components(&prop.components());
            if let Some(group) = prop.group.as_deref() {
                parts.country_code = card
                    .group_members(group)
                    .find(|(_, p)| p.is("X-ABADR"))
                    .and_then(|(_, p)| non_empty(p.text()));
            }
            (parts.display(), Some(parts))
        } else {
            (prop.text(), None)
        };
        items.push(Item {
            id: card.line_id(index).unwrap_or_default(),
            field,
            value,
            label: label.label,
            label_token: label.token,
            custom_label: label.custom,
            preferred: label.preferred,
            address,
            user: prop.param("X-USER").map(str::to_owned),
            service: service(field, prop),
        });
    }
    let mut other: BTreeMap<String, u32> = BTreeMap::new();
    for (index, prop) in card.props() {
        let upper = prop.name_upper();
        if INTERPRETED.contains(&upper.as_str())
            || Field::of(prop).is_some()
            || label_lines.contains(&index)
        {
            continue;
        }
        *other.entry(upper).or_default() += 1;
    }
    let (birthday, birthday_year_omitted) = card.first("BDAY").map_or((None, false), birthday);
    Projection {
        kind,
        version: card.version(),
        uid: card.uid(),
        full_name: single("FN"),
        name,
        nickname: single("NICKNAME"),
        organization: org.first().cloned().and_then(non_empty),
        department: org.get(1).cloned().and_then(non_empty),
        job_title: single("TITLE"),
        birthday,
        birthday_year_omitted,
        note: single("NOTE"),
        items,
        members: card
            .props()
            .filter(|(_, p)| is_member_prop(p))
            .filter_map(|(_, p)| member_uid(p))
            .collect(),
        has_photo: other.contains_key("PHOTO"),
        other,
    }
}

impl Projection {
    /// The name to show: FN, else the N parts, else the organization.
    pub fn display_name(&self) -> String {
        if let Some(full) = &self.full_name {
            return full.clone();
        }
        if let Some(name) = &self.name {
            let joined = [
                &name.prefix,
                &name.given,
                &name.middle,
                &name.family,
                &name.suffix,
            ]
            .iter()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim())
            .collect::<Vec<_>>()
            .join(" ");
            if !joined.is_empty() {
                return joined;
            }
        }
        self.organization.clone().unwrap_or_default()
    }

    pub fn values(&self, field: Field) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(move |i| i.field == field)
    }

    /// Case-insensitive substring match over names, organization, values and
    /// notes; a query with three or more digits also matches phone digits.
    pub fn matches(&self, query: &str) -> bool {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return true;
        }
        let mut haystack = vec![self.display_name()];
        if let Some(name) = &self.name {
            haystack.extend([
                name.given.clone(),
                name.family.clone(),
                name.middle.clone(),
                format!("{} {}", name.given, name.family),
            ]);
        }
        haystack.extend(
            [
                &self.nickname,
                &self.organization,
                &self.department,
                &self.job_title,
                &self.note,
            ]
            .into_iter()
            .flatten()
            .cloned(),
        );
        haystack.extend(self.items.iter().map(|i| i.value.clone()));
        if haystack.iter().any(|h| h.to_lowercase().contains(&needle)) {
            return true;
        }
        let digits: String = needle.chars().filter(char::is_ascii_digit).collect();
        digits.len() >= 3
            && self.values(Field::Phone).any(|p| {
                p.value
                    .chars()
                    .filter(char::is_ascii_digit)
                    .collect::<String>()
                    .contains(&digits)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_contacts_and_groups() {
        let contact = Card::parse("BEGIN:VCARD\r\nVERSION:3.0\r\nN:Doe;Jane;Q;Dr.;\r\nFN:Jane Doe\r\nORG:Acme\\, Inc.;Research\r\nitem1.ADR;type=HOME:;;1 Main St;Springfield;IL;62701;USA\r\nitem1.X-ABADR:us\r\nTEL;type=CELL:(555) 010-0100\r\nPHOTO;ENCODING=b;TYPE=JPEG:AAAA\r\nX-APPLE-FUTURE:1\r\nEND:VCARD\r\n").unwrap();
        let p = project(&contact);
        assert_eq!(p.kind, Kind::Contact);
        assert_eq!(p.organization.as_deref(), Some("Acme, Inc."));
        assert_eq!(p.department.as_deref(), Some("Research"));
        let address = p.values(Field::Address).next().unwrap();
        assert_eq!(address.value, "1 Main St, Springfield, IL, 62701, USA");
        assert_eq!(
            address.address.as_ref().unwrap().country_code.as_deref(),
            Some("us")
        );
        assert!(p.has_photo);
        assert_eq!(p.other.get("X-APPLE-FUTURE"), Some(&1));
        assert_eq!(p.other.get("X-ABADR"), Some(&1));
        assert!(p.matches("5550100"));
        assert!(p.matches("jane doe"));

        let group = Card::parse("BEGIN:VCARD\r\nVERSION:3.0\r\nN:Friends\r\nFN:Friends\r\nX-ADDRESSBOOKSERVER-KIND:group\r\nX-ADDRESSBOOKSERVER-MEMBER:urn:uuid:AAA\r\nX-ADDRESSBOOKSERVER-MEMBER:urn:uuid:BBB\r\nEND:VCARD\r\n").unwrap();
        let g = project(&group);
        assert_eq!(g.kind, Kind::Group);
        assert_eq!(g.members, vec!["AAA", "BBB"]);
    }

    #[test]
    fn non_ascii_member_values_do_not_panic() {
        let group = Card::parse("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:G\r\nX-ADDRESSBOOKSERVER-KIND:group\r\nX-ADDRESSBOOKSERVER-MEMBER:ééééé\r\nEND:VCARD\r\n").unwrap();
        assert_eq!(project(&group).members, vec!["ééééé"]);
    }

    #[test]
    fn exposes_omitted_birthday_years_and_im_services() {
        let card = Card::parse("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:x\r\nBDAY;X-APPLE-OMIT-YEAR=1604:1604-03-15\r\nIMPP;X-SERVICE-TYPE=Skype;type=HOME:skype:someone\r\nX-SOCIALPROFILE;type=twitter;x-user=someone:https://twitter.com/someone\r\nEND:VCARD\r\n").unwrap();
        let p = project(&card);
        assert_eq!(p.birthday.as_deref(), Some("--03-15"));
        assert!(p.birthday_year_omitted);
        let im = p.values(Field::InstantMessage).next().unwrap();
        assert_eq!(im.service.as_deref(), Some("Skype"));
        assert_eq!(im.label.as_deref(), Some("home"));
        let social = p.values(Field::SocialProfile).next().unwrap();
        assert_eq!(social.service.as_deref(), Some("twitter"));
        assert_eq!(social.user.as_deref(), Some("someone"));
    }
}
