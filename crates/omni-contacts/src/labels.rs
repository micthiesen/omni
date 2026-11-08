//! Value labels as Apple writes them.
//!
//! A label is either TYPE parameters (`TEL;type=CELL;type=VOICE`) or an
//! `X-ABLabel` line in the same `itemN` group, holding a custom text or one
//! of Apple's built-in tokens (`_$!<Other>!$_`). Reading reports one label
//! string; writing mirrors Apple's choice of encoding for each label.

use crate::vcard::{Card, Prop};

/// The multi-valued fields the tools edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Field {
    Email,
    Phone,
    Address,
    Url,
    InstantMessage,
    SocialProfile,
    RelatedName,
    Date,
}

impl Field {
    pub const ALL: [Field; 8] = [
        Field::Email,
        Field::Phone,
        Field::Address,
        Field::Url,
        Field::InstantMessage,
        Field::SocialProfile,
        Field::RelatedName,
        Field::Date,
    ];

    /// The vCard property name.
    pub fn property(self) -> &'static str {
        match self {
            Field::Email => "EMAIL",
            Field::Phone => "TEL",
            Field::Address => "ADR",
            Field::Url => "URL",
            Field::InstantMessage => "IMPP",
            Field::SocialProfile => "X-SOCIALPROFILE",
            Field::RelatedName => "X-ABRELATEDNAMES",
            Field::Date => "X-ABDATE",
        }
    }

    pub fn of(prop: &Prop) -> Option<Field> {
        Field::ALL.into_iter().find(|f| prop.is(f.property()))
    }

    /// The tool-facing name.
    pub fn as_str(self) -> &'static str {
        match self {
            Field::Email => "email",
            Field::Phone => "phone",
            Field::Address => "address",
            Field::Url => "url",
            Field::InstantMessage => "instantMessage",
            Field::SocialProfile => "socialProfile",
            Field::RelatedName => "relatedName",
            Field::Date => "date",
        }
    }
}

/// TYPE values that carry a label (everything else, such as INTERNET, PREF
/// or X- values, is kept when a label changes).
const LABEL_TYPES: &[&str] = &[
    "HOME", "WORK", "OTHER", "CELL", "IPHONE", "MAIN", "FAX", "PAGER", "VOICE", "SCHOOL", "MOBILE",
];

/// Apple's built-in label tokens and how the tools name them.
const BUILTIN: &[(&str, &str)] = &[
    ("Mobile", "mobile"),
    ("iPhone", "iphone"),
    ("Home", "home"),
    ("Work", "work"),
    ("Main", "main"),
    ("HomeFAX", "home fax"),
    ("WorkFAX", "work fax"),
    ("OtherFAX", "other fax"),
    ("Pager", "pager"),
    ("Other", "other"),
    ("HomePage", "homepage"),
    ("School", "school"),
    ("Anniversary", "anniversary"),
    ("Father", "father"),
    ("Mother", "mother"),
    ("Parent", "parent"),
    ("Brother", "brother"),
    ("Sister", "sister"),
    ("Child", "child"),
    ("Friend", "friend"),
    ("Spouse", "spouse"),
    ("Partner", "partner"),
    ("Assistant", "assistant"),
    ("Manager", "manager"),
];

/// A value's label as read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Label {
    pub label: Option<String>,
    /// The Apple token as stored (`_$!<Son>!$_`) for a built-in label.
    pub token: Option<String>,
    /// A free-text label (not a built-in or TYPE label).
    pub custom: bool,
    pub preferred: bool,
}

/// The tool name of an Apple built-in token (`_$!<HomePage>!$_` → `homepage`).
pub fn builtin_name(raw: &str) -> Option<String> {
    let token = raw.strip_prefix("_$!<")?.strip_suffix(">!$_")?;
    Some(
        BUILTIN
            .iter()
            .find(|(t, _)| t.eq_ignore_ascii_case(token))
            .map_or_else(
                || token.to_ascii_lowercase(),
                |(_, name)| (*name).to_owned(),
            ),
    )
}

/// The `X-ABLabel` sibling of a grouped value, with its index.
pub fn label_sibling(card: &Card, prop: &Prop) -> Option<usize> {
    let group = prop.group.as_deref()?;
    card.group_members(group)
        .find(|(_, p)| p.is("X-ABLABEL"))
        .map(|(i, _)| i)
}

fn type_label(field: Field, types: &[String]) -> Option<String> {
    let has = |t: &str| types.iter().any(|v| v == t);
    if field == Field::Phone {
        if has("FAX") {
            return Some(
                if has("HOME") {
                    "home fax"
                } else if has("WORK") {
                    "work fax"
                } else {
                    "other fax"
                }
                .to_owned(),
            );
        }
        for (ty, name) in [
            ("IPHONE", "iphone"),
            ("CELL", "mobile"),
            ("MOBILE", "mobile"),
            ("MAIN", "main"),
            ("PAGER", "pager"),
        ] {
            if has(ty) {
                return Some(name.to_owned());
            }
        }
    }
    for (ty, name) in [
        ("HOME", "home"),
        ("WORK", "work"),
        ("SCHOOL", "school"),
        ("OTHER", "other"),
    ] {
        if has(ty) {
            return Some(name.to_owned());
        }
    }
    None
}

/// Reads the label of the value on `prop`.
pub fn read(card: &Card, field: Field, prop: &Prop) -> Label {
    let types = prop.types();
    let preferred = types.iter().any(|t| t == "PREF") || prop.param("PREF").is_some();
    if let Some(index) = label_sibling(card, prop)
        && let Some(sibling) = card.prop(index)
    {
        let raw = sibling.text();
        return match builtin_name(&raw) {
            Some(name) => Label {
                label: Some(name),
                token: Some(raw.trim().to_owned()),
                custom: false,
                preferred,
            },
            None => Label {
                label: Some(raw),
                token: None,
                custom: true,
                preferred,
            },
        };
    }
    Label {
        label: type_label(field, &types),
        token: None,
        custom: false,
        preferred,
    }
}

/// How a requested label is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Encoding {
    /// TYPE values to add (after removing the old label TYPEs).
    pub types: Vec<&'static str>,
    /// The `X-ABLabel` value to write, already escaped.
    pub ab_label: Option<String>,
}

/// Whether `requested` names the label a value already has: a custom label
/// by exact text, a built-in or TYPE label by name (any case), or an Apple
/// token by the name it reads as. Echoing a label as read is never a change.
pub fn same_label(current: &Label, requested: &str) -> bool {
    let requested = requested.trim();
    let Some(current_label) = current.label.as_deref() else {
        return false;
    };
    if current.custom {
        return current_label == requested;
    }
    let name = builtin_name(requested).unwrap_or_else(|| requested.to_owned());
    current_label.eq_ignore_ascii_case(&name)
}

/// The encoding Apple uses for `label` on `field`. An Apple token
/// (`_$!<Son>!$_`) is written as given.
pub fn encode(field: Field, label: &str) -> Encoding {
    let trimmed = label.trim();
    if builtin_name(trimmed).is_some() {
        return Encoding {
            types: Vec::new(),
            ab_label: Some(trimmed.to_owned()),
        };
    }
    let lower = trimmed.to_lowercase();
    let types: Option<&[&'static str]> = match (field, lower.as_str()) {
        (Field::Phone, "mobile" | "cell") => Some(&["CELL", "VOICE"]),
        (Field::Phone, "iphone") => Some(&["IPHONE", "CELL", "VOICE"]),
        (Field::Phone, "home") => Some(&["HOME", "VOICE"]),
        (Field::Phone, "work") => Some(&["WORK", "VOICE"]),
        (Field::Phone, "main") => Some(&["MAIN"]),
        (Field::Phone, "home fax") => Some(&["HOME", "FAX"]),
        (Field::Phone, "work fax") => Some(&["WORK", "FAX"]),
        (Field::Phone, "other fax") => Some(&["OTHER", "FAX"]),
        (Field::Phone, "pager") => Some(&["PAGER"]),
        (Field::Phone, "other") => Some(&["OTHER", "VOICE"]),
        (Field::Email | Field::Address | Field::Url, "home") => Some(&["HOME"]),
        (Field::Email | Field::Address | Field::Url, "work") => Some(&["WORK"]),
        _ => None,
    };
    if let Some(types) = types {
        return Encoding {
            types: types.to_vec(),
            ab_label: None,
        };
    }
    let builtin = BUILTIN
        .iter()
        .find(|(_, name)| *name == lower)
        .map(|(token, _)| format!("_$!<{token}>!$_"));
    Encoding {
        types: Vec::new(),
        ab_label: Some(builtin.unwrap_or_else(|| crate::vcard::escape_text(trimmed))),
    }
}

/// Whether a TYPE value is label-bearing.
pub fn is_label_type(value: &str) -> bool {
    LABEL_TYPES.iter().any(|t| t.eq_ignore_ascii_case(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(text: &str) -> Card {
        Card::parse(&format!(
            "BEGIN:VCARD\r\nVERSION:3.0\r\n{text}END:VCARD\r\n"
        ))
        .unwrap()
    }

    #[test]
    fn reads_type_builtin_and_custom_labels() {
        let c = card(
            "TEL;type=IPHONE;type=CELL;type=VOICE;type=pref:1\r\nitem1.TEL:2\r\nitem1.X-ABLabel:_$!<Mobile>!$_\r\nitem2.EMAIL;type=INTERNET:a@example.com\r\nitem2.X-ABLabel:Book club\\, Tuesdays\r\nTEL;type=HOME;type=FAX:3\r\n",
        );
        let labels: Vec<Label> = c
            .named("TEL")
            .chain(c.named("EMAIL"))
            .map(|(_, p)| read(&c, Field::of(p).unwrap(), p))
            .collect();
        assert_eq!(labels[0].label.as_deref(), Some("iphone"));
        assert!(labels[0].preferred);
        assert_eq!(labels[1].label.as_deref(), Some("mobile"));
        assert!(!labels[1].custom);
        assert_eq!(labels[2].label.as_deref(), Some("home fax"));
        assert_eq!(labels[3].label.as_deref(), Some("Book club, Tuesdays"));
        assert!(labels[3].custom);
    }

    #[test]
    fn echoing_a_read_label_is_the_same_label() {
        let c = card(
            "item1.X-ABRELATEDNAMES:Sam\r\nitem1.X-ABLabel:_$!<Son>!$_\r\nitem2.EMAIL:a@example.com\r\nitem2.X-ABLabel:Home\r\n",
        );
        let son = c
            .named("X-ABRELATEDNAMES")
            .map(|(_, p)| read(&c, Field::RelatedName, p))
            .next()
            .unwrap();
        assert_eq!(son.label.as_deref(), Some("son"));
        assert!(same_label(&son, "son"));
        assert!(same_label(&son, "_$!<Son>!$_"));
        let home = c
            .named("EMAIL")
            .map(|(_, p)| read(&c, Field::Email, p))
            .next()
            .unwrap();
        assert!(home.custom);
        assert!(same_label(&home, "Home"));
        assert!(!same_label(&home, "home"));
        assert_eq!(
            encode(Field::RelatedName, "_$!<Son>!$_")
                .ab_label
                .as_deref(),
            Some("_$!<Son>!$_")
        );
    }

    #[test]
    fn encodes_like_apple() {
        assert_eq!(encode(Field::Phone, "Mobile").types, vec!["CELL", "VOICE"]);
        assert_eq!(
            encode(Field::Email, "other").ab_label.as_deref(),
            Some("_$!<Other>!$_")
        );
        assert_eq!(
            encode(Field::Email, "Book club; Tue").ab_label.as_deref(),
            Some("Book club\\; Tue")
        );
    }
}
