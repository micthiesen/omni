//! Field-targeted vCard edits and the checks that verify them.
//!
//! Every edit goes through [`Edits`], so a line the patch does not name keeps
//! its exact bytes. Values are addressed by line id (read with the card's
//! etag), single fields by name; a single field that appears more than once,
//! two edits of one line, or a label shared by two values is refused rather
//! than guessed at. Structured values (N, ORG, ADR) change only the named
//! components; the others keep their raw text. Each plan is checked against
//! the request before anything can be sent ([`self_check`]).

use std::collections::{BTreeMap, BTreeSet};

use crate::labels::{self, Field};
use crate::model::{self, AddressParts, Kind, Projection};
use crate::vcard::{
    self, Card, Edits, Param, Prop, VcardError, escape_text, render, split_unescaped, unescape,
    with_value,
};

/// Why an edit was refused. Each maps to a tool error code.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Ambiguous(String),
    #[error("{0}")]
    UnknownValue(String),
    #[error("{0}")]
    Unsupported(String),
    /// The planned card does not say what the request asked for: a bug,
    /// caught before anything is sent.
    #[error("the planned card does not match the request ({0}); nothing was sent")]
    PlanCheck(String),
}

impl EditError {
    pub fn code(&self) -> &'static str {
        match self {
            EditError::Invalid(_) => "invalid_input",
            EditError::Ambiguous(_) => "ambiguous_field",
            EditError::UnknownValue(_) => "unknown_value_id",
            EditError::Unsupported(_) => "unsupported",
            EditError::PlanCheck(_) => "plan_check_failed",
        }
    }
}

impl From<VcardError> for EditError {
    fn from(error: VcardError) -> Self {
        EditError::Invalid(error.to_string())
    }
}

/// The single-valued fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SingleField {
    FullName,
    Name,
    Nickname,
    Organization,
    Department,
    JobTitle,
    Birthday,
    Note,
}

impl SingleField {
    pub fn as_str(self) -> &'static str {
        match self {
            SingleField::FullName => "fullName",
            SingleField::Name => "name",
            SingleField::Nickname => "nickname",
            SingleField::Organization => "organization",
            SingleField::Department => "department",
            SingleField::JobTitle => "jobTitle",
            SingleField::Birthday => "birthday",
            SingleField::Note => "note",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NamePatch {
    pub family: Option<String>,
    pub given: Option<String>,
    pub middle: Option<String>,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
}

impl NamePatch {
    fn parts(&self) -> [Option<&String>; 5] {
        [
            self.family.as_ref(),
            self.given.as_ref(),
            self.middle.as_ref(),
            self.prefix.as_ref(),
            self.suffix.as_ref(),
        ]
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AddressPatch {
    pub po_box: Option<String>,
    pub extended: Option<String>,
    pub street: Option<String>,
    pub city: Option<String>,
    pub region: Option<String>,
    pub postal_code: Option<String>,
    pub country: Option<String>,
}

impl AddressPatch {
    fn parts(&self) -> [Option<&String>; 7] {
        [
            self.po_box.as_ref(),
            self.extended.as_ref(),
            self.street.as_ref(),
            self.city.as_ref(),
            self.region.as_ref(),
            self.postal_code.as_ref(),
            self.country.as_ref(),
        ]
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewValue {
    pub field: Field,
    pub value: Option<String>,
    pub address: Option<AddressPatch>,
    pub label: Option<String>,
    pub preferred: bool,
    /// Required for IM (`X-SERVICE-TYPE`) and social profile (its TYPE).
    pub service: Option<String>,
    /// A social profile's `x-user`.
    pub user: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValueUpdate {
    pub id: String,
    pub value: Option<String>,
    pub address: Option<AddressPatch>,
    pub label: Option<String>,
    pub preferred: Option<bool>,
}

/// A partial contact edit. Absent fields are not touched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContactPatch {
    pub full_name: Option<String>,
    pub name: Option<NamePatch>,
    pub nickname: Option<String>,
    pub organization: Option<String>,
    pub department: Option<String>,
    pub job_title: Option<String>,
    /// `YYYY-MM-DD`, or `--MM-DD` for a birthday without a year.
    pub birthday: Option<String>,
    pub note: Option<String>,
    pub clear: Vec<SingleField>,
    pub add: Vec<NewValue>,
    pub update: Vec<ValueUpdate>,
    pub remove: Vec<String>,
}

/// An edited card and what changed (property names and actions, no values).
#[derive(Clone, Debug)]
pub struct Edited {
    pub card: Card,
    pub changes: Vec<String>,
}

impl Edited {
    pub fn unchanged(&self) -> bool {
        self.changes.is_empty()
    }
}

fn check_text(what: &str, value: &str, multiline: bool) -> Result<(), EditError> {
    let bad = value
        .chars()
        .any(|c| c.is_control() && !(multiline && (c == '\n' || c == '\t')));
    if bad {
        return Err(EditError::Invalid(format!(
            "{what} contains control characters"
        )));
    }
    Ok(())
}

/// Only vCard 3.0 (what iCloud stores) is edited.
pub fn ensure_editable(card: &Card) -> Result<(), EditError> {
    match card.version().as_deref() {
        Some("3.0") => Ok(()),
        other => Err(EditError::Unsupported(format!(
            "only vCard 3.0 cards are edited (this card is {})",
            other.unwrap_or("unversioned")
        ))),
    }
}

/// Encodes a value of `field` for the line (escaping only TEXT values).
fn encode_value(field: Field, value: &str) -> Result<String, EditError> {
    check_text(field.as_str(), value, false)?;
    if value.trim().is_empty() {
        return Err(EditError::Invalid(format!(
            "{} value must not be empty",
            field.as_str()
        )));
    }
    Ok(match field {
        Field::Email | Field::RelatedName => escape_text(value.trim()),
        _ => value.trim().to_owned(),
    })
}

/// Sets components of a structured value by index. Components not named
/// keep their raw text (an unescaped comma in an address street stays a
/// list separator); a named component is rewritten only when its decoded
/// text differs. Returns the new raw value and whether it changed.
fn set_components(raw: &str, sets: &[(usize, &str)]) -> (String, bool) {
    let mut parts = split_unescaped(raw, ';');
    let mut changed = false;
    for (index, value) in sets {
        let current = parts.get(*index).map(|p| unescape(p)).unwrap_or_default();
        if current == *value {
            continue;
        }
        if parts.len() <= *index {
            parts.resize(index + 1, String::new());
        }
        parts[*index] = escape_text(value);
        changed = true;
    }
    (parts.join(";"), changed)
}

/// A new structured value of `width` components from the named ones.
fn new_components(width: usize, sets: &[(usize, &str)]) -> String {
    let mut parts = vec![String::new(); width];
    for (index, value) in sets {
        if *index >= parts.len() {
            parts.resize(index + 1, String::new());
        }
        parts[*index] = escape_text(value);
    }
    parts.join(";")
}

fn trimmed_sets<'v>(values: &[Option<&'v String>]) -> Vec<(usize, &'v str)> {
    values
        .iter()
        .enumerate()
        .filter_map(|(i, v)| v.map(|v| (i, v.trim())))
        .collect()
}

/// A birthday request as (value to write, year omitted).
fn birthday_value(input: &str) -> Result<(String, bool), EditError> {
    let invalid = || {
        EditError::Invalid(
            "birthday must be a real date: YYYY-MM-DD, or --MM-DD without a year".to_owned(),
        )
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !input.is_ascii() {
        return Err(invalid());
    }
    // A year-less date is checked in a leap year, so --02-29 is accepted;
    // Apple stores it with year 1604, also a leap year.
    let (year, month_day, omit) = match input.strip_prefix("--") {
        Some(rest) => ("2000", rest, true),
        None if input.len() == 10 && input.as_bytes()[4] == b'-' => {
            (&input[..4], &input[5..], false)
        }
        None => return Err(invalid()),
    };
    let (month, day) = month_day.split_once('-').ok_or_else(invalid)?;
    if !digits(year) || month.len() != 2 || day.len() != 2 || !digits(month) || !digits(day) {
        return Err(invalid());
    }
    let parse = |s: &str| s.parse::<i16>().map_err(|_| invalid());
    let (year, month, day) = (parse(year)?, parse(month)?, parse(day)?);
    let month = i8::try_from(month).map_err(|_| invalid())?;
    let day = i8::try_from(day).map_err(|_| invalid())?;
    jiff::civil::Date::new(year, month, day).map_err(|_| invalid())?;
    Ok(if omit {
        (format!("1604-{month_day}"), true)
    } else {
        (input.to_owned(), false)
    })
}

/// A parameter value given by the caller (service or user name).
fn check_param_text(what: &str, value: &str) -> Result<String, EditError> {
    check_text(what, value, false)?;
    let value = value.trim();
    if value.is_empty() || value.contains('"') {
        return Err(EditError::Invalid(format!(
            "{what} must be non-empty text without double quotes"
        )));
    }
    Ok(value.to_owned())
}

struct Editor<'a> {
    card: &'a Card,
    edits: Edits,
    changes: Vec<String>,
    next_item: Option<u64>,
    /// Names of group siblings removed together with a value.
    removed_siblings: Vec<String>,
}

impl<'a> Editor<'a> {
    fn new(card: &'a Card) -> Self {
        Self {
            card,
            edits: Edits::default(),
            changes: Vec::new(),
            next_item: card.next_item_number(),
            removed_siblings: Vec::new(),
        }
    }

    fn new_group(&mut self) -> Result<String, EditError> {
        let number = self.next_item.ok_or_else(|| {
            EditError::Unsupported("the card has no free itemN group number left".to_owned())
        })?;
        self.next_item = number.checked_add(1);
        Ok(format!("item{number}"))
    }

    fn touch_guard(&self, index: usize) -> Result<(), EditError> {
        if self.edits.is_touched(index) {
            return Err(EditError::Invalid(
                "two edits in this request change the same line; send them separately".to_owned(),
            ));
        }
        Ok(())
    }

    fn replace(&mut self, index: usize, text: String) -> Result<(), EditError> {
        self.touch_guard(index)?;
        self.edits.replace(index, text);
        Ok(())
    }

    fn remove_line(&mut self, index: usize) -> Result<(), EditError> {
        self.touch_guard(index)?;
        self.edits.remove(index);
        Ok(())
    }

    fn only(&self, name: &str) -> Result<Option<(usize, &'a Prop)>, EditError> {
        let found: Vec<(usize, &Prop)> = self.card.named(name).collect();
        match found.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(*one)),
            many => Err(EditError::Ambiguous(format!(
                "the card has {} {name} lines; edit them by value id instead",
                many.len()
            ))),
        }
    }

    fn text_of(&self, index: usize) -> String {
        self.card.lines()[index].text().to_owned()
    }

    /// Sets a TEXT property, inserting it when absent.
    fn set_text(&mut self, name: &str, value: &str, multiline: bool) -> Result<(), EditError> {
        check_text(name, value, multiline)?;
        let value = value.trim();
        match self.only(name)? {
            Some((_, prop)) if prop.text() == value => {}
            Some((index, prop)) => {
                let text = with_value(prop, &self.text_of(index), &escape_text(value));
                self.replace(index, text)?;
                self.changes.push(format!("{name} changed"));
            }
            None if value.is_empty() => {}
            None => {
                self.edits
                    .append(render(None, name, &[], &escape_text(value)));
                self.changes.push(format!("{name} added"));
            }
        }
        Ok(())
    }

    /// Sets named components of a structured property in one edit. With
    /// `drop_when_empty`, a value left with no text removes the line.
    fn set_structured(
        &mut self,
        name: &str,
        width: usize,
        sets: &[(usize, &str)],
        drop_when_empty: bool,
    ) -> Result<(), EditError> {
        for (_, value) in sets {
            check_text(name, value, false)?;
        }
        match self.only(name)? {
            Some((index, prop)) => {
                let (mut value, changed) = set_components(&prop.value, sets);
                if !changed {
                    return Ok(());
                }
                let empty = split_unescaped(&value, ';')
                    .iter()
                    .all(|p| unescape(p).trim().is_empty());
                if empty && drop_when_empty {
                    self.remove_line(index)?;
                    self.changes.push(format!("{name} removed"));
                    return Ok(());
                }
                // Drop trailing components this edit emptied (ORG:Acme;Research
                // with the department cleared becomes ORG:Acme).
                let mut parts = split_unescaped(&value, ';');
                while parts.len() > width.max(1)
                    && parts.last().is_some_and(String::is_empty)
                    && sets.iter().any(|(i, _)| *i == parts.len() - 1)
                {
                    parts.pop();
                }
                value = parts.join(";");
                let text = with_value(prop, &self.text_of(index), &value);
                self.replace(index, text)?;
                self.changes.push(format!("{name} changed"));
            }
            None => {
                if sets.iter().all(|(_, v)| v.is_empty()) {
                    return Ok(());
                }
                let value = new_components(width, sets);
                self.edits.append(render(None, name, &[], &value));
                self.changes.push(format!("{name} added"));
            }
        }
        Ok(())
    }

    fn remove_all(&mut self, name: &str) -> Result<(), EditError> {
        let indices: Vec<usize> = self.card.named(name).map(|(i, _)| i).collect();
        if indices.is_empty() {
            return Ok(());
        }
        for index in indices {
            self.remove_line(index)?;
        }
        self.changes.push(format!("{name} removed"));
        Ok(())
    }

    fn set_birthday(&mut self, input: &str) -> Result<(), EditError> {
        let (value, omit) = birthday_value(input.trim())?;
        let omit_param = || Param::new("X-APPLE-OMIT-YEAR", "1604");
        match self.only("BDAY")? {
            Some((_, prop)) if model::birthday(prop).0.as_deref() == Some(input.trim()) => {}
            Some((index, prop)) => {
                let has_omit = prop.param("X-APPLE-OMIT-YEAR").is_some();
                let text = if omit || has_omit {
                    let mut params: Vec<Param> = prop
                        .params
                        .iter()
                        .filter(|p| !p.is("X-APPLE-OMIT-YEAR"))
                        .cloned()
                        .collect();
                    if omit {
                        params.push(omit_param());
                    }
                    render(prop.group.as_deref(), &prop.name, &params, &value)
                } else {
                    with_value(prop, &self.text_of(index), &value)
                };
                self.replace(index, text)?;
                self.changes.push("BDAY changed".to_owned());
            }
            None => {
                let mut params = vec![Param::new("value", "date")];
                if omit {
                    params.push(omit_param());
                }
                self.edits.append(render(None, "BDAY", &params, &value));
                self.changes.push("BDAY added".to_owned());
            }
        }
        Ok(())
    }

    fn value_line(&self, id: &str) -> Result<(usize, &'a Prop, Field), EditError> {
        let index = self.card.find_id(id).ok_or_else(|| {
            EditError::UnknownValue(format!(
                "no value with id {id} in this version of the card; read it again"
            ))
        })?;
        let prop = self
            .card
            .prop(index)
            .ok_or_else(|| EditError::UnknownValue(format!("{id} is not a property line")))?;
        let field = Field::of(prop)
            .ok_or_else(|| EditError::Invalid(format!("{id} does not name an editable value")))?;
        Ok((index, prop, field))
    }

    /// Whether another value shares the line's item group (and so its label).
    fn shares_group(&self, index: usize, prop: &Prop) -> bool {
        prop.group.as_deref().is_some_and(|group| {
            self.card
                .group_members(group)
                .any(|(i, p)| i != index && Field::of(p).is_some())
        })
    }

    /// Params with TYPE values replaced; every other parameter keeps its
    /// original text.
    fn type_params(prop_params: &[Param], types: &[String]) -> Vec<Param> {
        let mut params: Vec<Param> = prop_params
            .iter()
            .filter(|p| !(p.name.is_none() || p.is("TYPE")))
            .cloned()
            .collect();
        let type_name = prop_params
            .iter()
            .find(|p| p.is("TYPE"))
            .and_then(|p| p.name.clone())
            .unwrap_or_else(|| "type".to_owned());
        for value in types {
            let value = if value == "PREF" { "pref" } else { value };
            params.push(Param::new(&type_name, value));
        }
        params
    }

    fn add(&mut self, new: &NewValue) -> Result<(), EditError> {
        let field = new.field;
        let value = if field == Field::Address {
            let address = new.address.as_ref().ok_or_else(|| {
                EditError::Invalid("an address value needs `address`, not `value`".to_owned())
            })?;
            let sets = trimmed_sets(&address.parts());
            for (_, part) in &sets {
                check_text("address", part, false)?;
            }
            if sets.iter().all(|(_, v)| v.is_empty()) {
                return Err(EditError::Invalid("the address is empty".to_owned()));
            }
            new_components(7, &sets)
        } else {
            if new.address.is_some() {
                return Err(EditError::Invalid(format!(
                    "`address` applies only to address values, not {}",
                    field.as_str()
                )));
            }
            let value = new.value.as_deref().ok_or_else(|| {
                EditError::Invalid(format!("a new {} needs `value`", field.as_str()))
            })?;
            encode_value(field, value)?
        };
        let encoding = match &new.label {
            Some(label) => {
                check_text("label", label, false)?;
                if label.trim().is_empty() {
                    return Err(EditError::Invalid("label must not be empty".to_owned()));
                }
                Some(labels::encode(field, label))
            }
            None => None,
        };
        let is_service_field = matches!(field, Field::InstantMessage | Field::SocialProfile);
        let service = match (&new.service, is_service_field) {
            (Some(service), true) => Some(check_param_text("service", service)?),
            (None, true) => {
                return Err(EditError::Invalid(format!(
                    "a new {} needs `service` (for example Skype or twitter)",
                    field.as_str()
                )));
            }
            (Some(_), false) => {
                return Err(EditError::Invalid(
                    "`service` applies only to instantMessage and socialProfile values".to_owned(),
                ));
            }
            (None, false) => None,
        };
        let user = match (&new.user, field) {
            (Some(user), Field::SocialProfile) => Some(check_param_text("user", user)?),
            (Some(_), _) => {
                return Err(EditError::Invalid(
                    "`user` applies only to socialProfile values".to_owned(),
                ));
            }
            (None, _) => None,
        };
        let mut base = Vec::new();
        let mut types: Vec<String> = Vec::new();
        match (field, &service) {
            (Field::Email, _) => types.push("INTERNET".to_owned()),
            (Field::InstantMessage, Some(service)) => {
                base.push(Param::new("X-SERVICE-TYPE", service));
            }
            (Field::SocialProfile, Some(service)) => types.push(service.to_ascii_lowercase()),
            _ => {}
        }
        if let Some(user) = &user {
            base.push(Param::new("x-user", user));
        }
        if let Some(encoding) = &encoding {
            types.extend(encoding.types.iter().map(|t| (*t).to_owned()));
        }
        if new.preferred {
            types.push("pref".to_owned());
        }
        let params = Self::type_params(&base, &types);
        let property = field.property();
        match encoding.and_then(|e| e.ab_label) {
            Some(ab_label) => {
                let group = self.new_group()?;
                self.edits
                    .append(render(Some(&group), property, &params, &value));
                self.edits
                    .append(render(Some(&group), "X-ABLabel", &[], &ab_label));
            }
            None => self.edits.append(render(None, property, &params, &value)),
        }
        self.changes.push(format!("{property} added"));
        Ok(())
    }

    fn update(&mut self, update: &ValueUpdate) -> Result<(), EditError> {
        let (index, prop, field) = self.value_line(&update.id)?;
        let original = self.text_of(index);
        let mut value = prop.value.clone();
        if let Some(new) = &update.value {
            if field == Field::Address {
                return Err(EditError::Invalid(
                    "change an address with `address`, not `value`".to_owned(),
                ));
            }
            if prop.text() != new.trim() {
                if matches!(field, Field::SocialProfile | Field::InstantMessage) {
                    return Err(EditError::Unsupported(format!(
                        "changing a {} value would leave its service and user name stale; remove it and add a new one",
                        field.as_str()
                    )));
                }
                value = encode_value(field, new)?;
            }
        }
        if let Some(address) = &update.address {
            if field != Field::Address {
                return Err(EditError::Invalid(format!(
                    "`address` applies only to address values, not {}",
                    field.as_str()
                )));
            }
            let sets = trimmed_sets(&address.parts());
            for (_, part) in &sets {
                check_text("address", part, false)?;
            }
            let (next, changed) = set_components(&prop.value, &sets);
            if changed {
                value = next;
            }
        }
        // TYPE values keep their written case (a social profile's service).
        let mut types = prop.raw_types();
        let mut group = prop.group.clone();
        let mut label_changed = false;
        if let Some(preferred) = update.preferred {
            types.retain(|t| !t.eq_ignore_ascii_case("PREF"));
            if preferred {
                types.push("pref".to_owned());
            }
        }
        if let Some(label) = &update.label {
            check_text("label", label, false)?;
            if label.trim().is_empty() {
                return Err(EditError::Invalid("label must not be empty".to_owned()));
            }
            let current = labels::read(self.card, field, prop);
            if !labels::same_label(&current, label) {
                if self.shares_group(index, prop) {
                    return Err(EditError::Unsupported(format!(
                        "value {} shares its label group with another value; its label cannot change alone",
                        update.id
                    )));
                }
                label_changed = true;
                let encoding = labels::encode(field, label);
                types.retain(|t| !labels::is_label_type(t));
                let at = types
                    .iter()
                    .position(|t| t.eq_ignore_ascii_case("PREF"))
                    .unwrap_or(types.len());
                for (offset, t) in encoding.types.iter().enumerate() {
                    types.insert(at + offset, (*t).to_owned());
                }
                let sibling = labels::label_sibling(self.card, prop);
                match (encoding.ab_label, sibling) {
                    (Some(ab_label), Some(si)) => {
                        if let Some(sib) = self.card.prop(si)
                            && sib.value != ab_label
                        {
                            let sib_text = with_value(sib, &self.text_of(si), &ab_label);
                            self.replace(si, sib_text)?;
                        }
                    }
                    (Some(ab_label), None) => {
                        let name = match group.clone() {
                            Some(name) => name,
                            None => self.new_group()?,
                        };
                        self.edits
                            .insert_after(index, render(Some(&name), "X-ABLabel", &[], &ab_label));
                        group = Some(name);
                    }
                    (None, Some(si)) => self.remove_line(si)?,
                    (None, None) => {}
                }
            }
        }
        let mut sorted_new: Vec<String> = types.iter().map(|t| t.to_ascii_uppercase()).collect();
        sorted_new.sort();
        let mut sorted_old = prop.types();
        sorted_old.sort();
        let text = if sorted_new != sorted_old || group != prop.group {
            render(
                group.as_deref(),
                &prop.name,
                &Self::type_params(&prop.params, &types),
                &value,
            )
        } else if value != prop.value {
            with_value(prop, &original, &value)
        } else {
            original.clone()
        };
        if text != original {
            self.replace(index, text)?;
            self.changes.push(format!("{} changed", field.property()));
        } else if label_changed {
            self.changes
                .push(format!("{} label changed", field.property()));
        }
        Ok(())
    }

    fn remove(&mut self, id: &str) -> Result<(), EditError> {
        let (index, prop, field) = self.value_line(id)?;
        let mut siblings = Vec::new();
        if let Some(group) = prop.group.as_deref() {
            for (i, sibling) in self.card.group_members(group) {
                if i == index {
                    continue;
                }
                if Field::of(sibling).is_some() {
                    return Err(EditError::Unsupported(format!(
                        "value {id} shares item group {group} with another value; remove both or edit them separately"
                    )));
                }
                siblings.push((i, sibling.name_upper()));
            }
        }
        self.remove_line(index)?;
        for (sibling, name) in siblings {
            self.remove_line(sibling)?;
            if name != "X-ABLABEL" {
                self.removed_siblings.push(name);
            }
        }
        self.changes.push(format!("{} removed", field.property()));
        Ok(())
    }
}

fn validate_patch(patch: &ContactPatch) -> Result<(), EditError> {
    let mut seen = BTreeSet::new();
    for id in patch
        .update
        .iter()
        .map(|u| &u.id)
        .chain(patch.remove.iter())
    {
        if !seen.insert(id) {
            return Err(EditError::Invalid(format!(
                "value {id} is named more than once"
            )));
        }
    }
    let cleared: BTreeSet<SingleField> = patch.clear.iter().copied().collect();
    let set: [(SingleField, bool); 8] = [
        (SingleField::FullName, patch.full_name.is_some()),
        (SingleField::Name, patch.name.is_some()),
        (SingleField::Nickname, patch.nickname.is_some()),
        (SingleField::Organization, patch.organization.is_some()),
        (SingleField::Department, patch.department.is_some()),
        (SingleField::JobTitle, patch.job_title.is_some()),
        (SingleField::Birthday, patch.birthday.is_some()),
        (SingleField::Note, patch.note.is_some()),
    ];
    for (field, is_set) in set {
        if is_set && cleared.contains(&field) {
            return Err(EditError::Invalid(format!(
                "{} is both set and cleared",
                field.as_str()
            )));
        }
    }
    for field in [SingleField::FullName, SingleField::Name] {
        if cleared.contains(&field) {
            return Err(EditError::Invalid(format!(
                "{} cannot be cleared (vCard 3.0 requires it); set it instead",
                field.as_str()
            )));
        }
    }
    if patch
        .full_name
        .as_deref()
        .is_some_and(|f| f.trim().is_empty())
    {
        return Err(EditError::Invalid("fullName must not be empty".to_owned()));
    }
    Ok(())
}

/// Applies a contact patch. Lines the patch does not name are untouched, and
/// the result is checked against the request before it is returned.
pub fn apply_patch(card: &Card, patch: &ContactPatch) -> Result<Edited, EditError> {
    ensure_editable(card)?;
    if model::kind_of(card) == Kind::Group {
        return Err(EditError::Invalid(
            "this is a group; use the group tools".to_owned(),
        ));
    }
    validate_patch(patch)?;
    let cleared: BTreeSet<SingleField> = patch.clear.iter().copied().collect();
    let mut editor = Editor::new(card);
    if let Some(full_name) = &patch.full_name {
        editor.set_text("FN", full_name, false)?;
    }
    if let Some(name) = &patch.name {
        editor.set_structured("N", 5, &trimmed_sets(&name.parts()), false)?;
    }
    if let Some(nickname) = &patch.nickname {
        editor.set_text("NICKNAME", nickname, false)?;
    }
    // Organization and department share the ORG line: one edit sets and
    // clears both components together.
    let mut org: Vec<(usize, &str)> = Vec::new();
    for (slot, set, field) in [
        (0, &patch.organization, SingleField::Organization),
        (1, &patch.department, SingleField::Department),
    ] {
        if let Some(value) = set {
            org.push((slot, value.trim()));
        } else if cleared.contains(&field) {
            org.push((slot, ""));
        }
    }
    if !org.is_empty() {
        editor.set_structured("ORG", 1, &org, true)?;
    }
    if let Some(title) = &patch.job_title {
        editor.set_text("TITLE", title, false)?;
    }
    if let Some(birthday) = &patch.birthday {
        editor.set_birthday(birthday)?;
    }
    if let Some(note) = &patch.note {
        editor.set_text("NOTE", note, true)?;
    }
    for field in &cleared {
        match field {
            SingleField::Nickname => editor.remove_all("NICKNAME")?,
            SingleField::JobTitle => editor.remove_all("TITLE")?,
            SingleField::Birthday => editor.remove_all("BDAY")?,
            SingleField::Note => editor.remove_all("NOTE")?,
            _ => {}
        }
    }
    for update in &patch.update {
        editor.update(update)?;
    }
    for id in &patch.remove {
        editor.remove(id)?;
    }
    for new in &patch.add {
        editor.add(new)?;
    }
    let edited = editor.edits.apply(card)?;
    self_check(card, &edited, patch, &editor.removed_siblings)?;
    let changes = if edited.to_text() == card.to_text() {
        Vec::new()
    } else {
        editor.changes
    };
    Ok(Edited {
        card: edited,
        changes,
    })
}

// ---- plan self-check ----

fn plan_err(what: impl Into<String>) -> EditError {
    EditError::PlanCheck(what.into())
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn same_text(got: Option<&String>, want: Option<String>, what: &str) -> Result<(), EditError> {
    if got.map(|g| g.trim()) == want.as_deref() {
        Ok(())
    } else {
        Err(plan_err(format!("{what} differs")))
    }
}

/// An item's identity for matching: field, decoded value, label and pref.
fn item_key(item: &model::Item) -> (Field, String, Option<String>, bool, bool) {
    (
        item.field,
        item.value.clone(),
        item.label.clone(),
        item.custom_label,
        item.preferred,
    )
}

/// Checks the planned card against the request: every requested field and
/// value holds, every other field and value is as before, and the set of
/// other properties is unchanged except group siblings removed with a value.
pub fn self_check(
    before_card: &Card,
    after_card: &Card,
    patch: &ContactPatch,
    removed_siblings: &[String],
) -> Result<(), EditError> {
    let before = model::project(before_card);
    let after = model::project(after_card);
    let cleared: BTreeSet<SingleField> = patch.clear.iter().copied().collect();
    let expect = |set: &Option<String>, field: SingleField, old: &Option<String>| match set {
        Some(value) => non_empty(value),
        None if cleared.contains(&field) => None,
        None => old.clone(),
    };
    same_text(
        after.full_name.as_ref(),
        expect(&patch.full_name, SingleField::FullName, &before.full_name),
        "fullName",
    )?;
    same_text(
        after.nickname.as_ref(),
        expect(&patch.nickname, SingleField::Nickname, &before.nickname),
        "nickname",
    )?;
    same_text(
        after.organization.as_ref(),
        expect(
            &patch.organization,
            SingleField::Organization,
            &before.organization,
        ),
        "organization",
    )?;
    same_text(
        after.department.as_ref(),
        expect(
            &patch.department,
            SingleField::Department,
            &before.department,
        ),
        "department",
    )?;
    same_text(
        after.job_title.as_ref(),
        expect(&patch.job_title, SingleField::JobTitle, &before.job_title),
        "jobTitle",
    )?;
    same_text(
        after.note.as_ref(),
        expect(&patch.note, SingleField::Note, &before.note),
        "note",
    )?;
    same_text(
        after.birthday.as_ref(),
        expect(&patch.birthday, SingleField::Birthday, &before.birthday),
        "birthday",
    )?;
    let name_parts = |p: &Projection| {
        p.name.as_ref().map(|n| {
            [&n.family, &n.given, &n.middle, &n.prefix, &n.suffix].map(|s| s.trim().to_owned())
        })
    };
    let mut want_name = name_parts(&before);
    if let Some(name) = &patch.name {
        let mut parts = want_name.unwrap_or_default();
        for (i, value) in name.parts().iter().enumerate() {
            if let Some(value) = value {
                parts[i] = value.trim().to_owned();
            }
        }
        want_name = (before.name.is_some() || parts.iter().any(|p| !p.is_empty())).then_some(parts);
    }
    if name_parts(&after) != want_name {
        return Err(plan_err("name differs"));
    }

    // Values: untouched ones as before, then each update and add.
    let touched: BTreeSet<&str> = patch
        .update
        .iter()
        .map(|u| u.id.as_str())
        .chain(patch.remove.iter().map(String::as_str))
        .collect();
    let mut pool: Vec<model::Item> = after.items.clone();
    let mut take = |pred: &dyn Fn(&model::Item) -> bool, what: &str| -> Result<(), EditError> {
        match pool.iter().position(pred) {
            Some(i) => {
                pool.remove(i);
                Ok(())
            }
            None => Err(plan_err(format!("{what} differs"))),
        }
    };
    for item in before
        .items
        .iter()
        .filter(|i| !touched.contains(i.id.as_str()))
    {
        let key = item_key(item);
        take(&|i: &model::Item| item_key(i) == key, item.field.as_str())?;
    }
    for update in &patch.update {
        let Some(old) = before.items.iter().find(|i| i.id == update.id) else {
            return Err(plan_err("an updated value is missing"));
        };
        let value = match (&update.value, &update.address, &old.address) {
            (Some(value), _, _) => value.trim().to_owned(),
            (None, Some(address), Some(parts)) => {
                let mut next = parts.clone();
                let set = |slot: &mut String, v: Option<&String>| {
                    if let Some(v) = v {
                        *slot = v.trim().to_owned();
                    }
                };
                set(&mut next.po_box, address.po_box.as_ref());
                set(&mut next.extended, address.extended.as_ref());
                set(&mut next.street, address.street.as_ref());
                set(&mut next.city, address.city.as_ref());
                set(&mut next.region, address.region.as_ref());
                set(&mut next.postal_code, address.postal_code.as_ref());
                set(&mut next.country, address.country.as_ref());
                next.display()
            }
            _ => old.value.clone(),
        };
        let preferred = update.preferred.unwrap_or(old.preferred);
        let field = old.field;
        let label = update.label.clone();
        let old_label = (old.label.clone(), old.custom_label);
        take(
            &|i: &model::Item| {
                i.field == field
                    && i.value == value
                    && i.preferred == preferred
                    && match &label {
                        Some(label) => labels::same_label(
                            &labels::Label {
                                label: i.label.clone(),
                                token: i.label_token.clone(),
                                custom: i.custom_label,
                                preferred: i.preferred,
                            },
                            label,
                        ),
                        None => (i.label.clone(), i.custom_label) == old_label,
                    }
            },
            field.as_str(),
        )?;
    }
    for new in &patch.add {
        let value = match &new.address {
            Some(address) => {
                let components: Vec<String> = address
                    .parts()
                    .iter()
                    .map(|p| p.map(|v| v.trim().to_owned()).unwrap_or_default())
                    .collect();
                AddressParts::from_components(&components).display()
            }
            None => new.value.as_deref().unwrap_or_default().trim().to_owned(),
        };
        let field = new.field;
        let preferred = new.preferred;
        let label = new.label.clone();
        let service = new
            .service
            .as_deref()
            .map(|s| s.trim().to_ascii_lowercase());
        let user = new.user.as_deref().map(|u| u.trim().to_owned());
        take(
            &|i: &model::Item| {
                i.field == field
                    && i.value == value
                    && i.preferred == preferred
                    && (service.is_none()
                        || i.service.as_deref().map(str::to_ascii_lowercase) == service)
                    && i.user == user
                    && match &label {
                        Some(label) => labels::same_label(
                            &labels::Label {
                                label: i.label.clone(),
                                token: i.label_token.clone(),
                                custom: i.custom_label,
                                preferred: i.preferred,
                            },
                            label,
                        ),
                        None => i.label.is_none(),
                    }
            },
            field.as_str(),
        )?;
    }
    if !pool.is_empty() {
        return Err(plan_err("an unrequested value appeared"));
    }
    let mut want_other = before.other.clone();
    for name in removed_siblings {
        if let Some(count) = want_other.get_mut(name) {
            *count = count.saturating_sub(1);
        }
    }
    want_other.retain(|_, n| *n > 0);
    if after.other != want_other || after.uid != before.uid || after.kind != before.kind {
        return Err(plan_err("other properties changed"));
    }
    Ok(())
}

// ---- groups ----

/// A member reference must survive as one `urn:uuid:` value.
fn check_member_uid(uid: &str) -> Result<(), EditError> {
    let ok = !uid.is_empty()
        && uid.len() <= 200
        && uid
            .chars()
            .all(|c| !c.is_control() && !c.is_whitespace() && c != ',' && c != ';');
    if ok {
        Ok(())
    } else {
        Err(EditError::Unsupported(
            "a contact's UID cannot be written as a group member reference".to_owned(),
        ))
    }
}

fn member_name(card: &Card) -> &'static str {
    let apple = card.props().any(|(_, p)| p.is("X-ADDRESSBOOKSERVER-KIND"));
    let v4 = card.props().any(|(_, p)| p.is("KIND"));
    if !apple && v4 {
        "MEMBER"
    } else {
        "X-ADDRESSBOOKSERVER-MEMBER"
    }
}

fn ensure_group(card: &Card) -> Result<(), EditError> {
    ensure_editable(card)?;
    if model::kind_of(card) != Kind::Group {
        return Err(EditError::Invalid("this card is not a group".to_owned()));
    }
    Ok(())
}

fn member_counts(members: &[String]) -> BTreeMap<String, u32> {
    let mut counts = BTreeMap::new();
    for uid in members {
        *counts.entry(uid.to_ascii_lowercase()).or_insert(0) += 1;
    }
    counts
}

/// Adds and removes member UIDs on a group card. Lines of other members and
/// every other property are untouched; adding a present member or removing
/// an absent one changes nothing.
pub fn change_members(card: &Card, add: &[String], remove: &[String]) -> Result<Edited, EditError> {
    ensure_group(card)?;
    let removing: BTreeSet<String> = remove.iter().map(|u| u.to_ascii_lowercase()).collect();
    for uid in add {
        check_member_uid(uid)?;
        if removing.contains(&uid.to_ascii_lowercase()) {
            return Err(EditError::Invalid(format!(
                "member {uid} is both added to and removed from the same group"
            )));
        }
    }
    let mut edits = Edits::default();
    let mut present: BTreeSet<String> = BTreeSet::new();
    let mut last_member = None;
    let mut removed = 0usize;
    for (index, prop) in card.props().filter(|(_, p)| model::is_member_prop(p)) {
        last_member = Some(index);
        let Some(uid) = model::member_uid(prop) else {
            continue;
        };
        if removing.contains(&uid.to_ascii_lowercase()) {
            edits.remove(index);
            removed += 1;
        } else {
            present.insert(uid.to_ascii_lowercase());
        }
    }
    let name = member_name(card);
    let mut added = Vec::new();
    for uid in add {
        if present.insert(uid.to_ascii_lowercase()) {
            added.push(uid.clone());
        }
    }
    for uid in &added {
        let line = render(None, name, &[], &format!("urn:uuid:{uid}"));
        match last_member {
            Some(index) => edits.insert_after(index, line),
            None => edits.append(line),
        }
    }
    let edited = edits.apply(card)?;
    // Check the plan: exactly these members changed, nothing else did.
    let before = model::project(card);
    let after = model::project(&edited);
    let mut want = member_counts(&before.members);
    want.retain(|uid, _| !removing.contains(uid));
    for uid in &added {
        *want.entry(uid.to_ascii_lowercase()).or_insert(0) += 1;
    }
    if member_counts(&after.members) != want
        || after.other != before.other
        || after.full_name != before.full_name
        || after.uid != before.uid
    {
        return Err(plan_err("group membership differs"));
    }
    let mut changes = Vec::new();
    if !added.is_empty() {
        changes.push(format!("{} member(s) added", added.len()));
    }
    if removed > 0 {
        changes.push(format!("{removed} member line(s) removed"));
    }
    Ok(Edited {
        card: edited,
        changes,
    })
}

/// Renames a group: FN, and the first N component when N is present.
pub fn rename_group(card: &Card, name: &str) -> Result<Edited, EditError> {
    ensure_group(card)?;
    check_group_name(name)?;
    let mut editor = Editor::new(card);
    editor.set_text("FN", name, false)?;
    if card.first("N").is_some() {
        editor.set_structured("N", 1, &[(0, name.trim())], false)?;
    }
    let edited = editor.edits.apply(card)?;
    let before = model::project(card);
    let after = model::project(&edited);
    if after.full_name.as_deref() != Some(name.trim())
        || after.members != before.members
        || after.other != before.other
    {
        return Err(plan_err("group name differs"));
    }
    Ok(Edited {
        changes: if edited.to_text() == card.to_text() {
            Vec::new()
        } else {
            editor.changes
        },
        card: edited,
    })
}

pub fn check_group_name(name: &str) -> Result<(), EditError> {
    check_text("name", name, false)?;
    if name.trim().is_empty() {
        return Err(EditError::Invalid(
            "a group name must not be empty".to_owned(),
        ));
    }
    Ok(())
}

fn rev(now: jiff::Timestamp) -> String {
    now.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// A new iCloud-style group card.
pub fn new_group(
    uid: &str,
    name: &str,
    members: &[String],
    now: jiff::Timestamp,
) -> Result<Card, EditError> {
    check_group_name(name)?;
    let name = escape_text(name.trim());
    let mut text = format!(
        "BEGIN:VCARD\r\nVERSION:3.0\r\nPRODID:-//Omni//Contacts//EN\r\nN:{name}\r\nFN:{name}\r\nX-ADDRESSBOOKSERVER-KIND:group\r\n"
    );
    let mut seen = BTreeSet::new();
    for uid in members {
        check_member_uid(uid)?;
        if seen.insert(uid.to_ascii_lowercase()) {
            text.push_str(&vcard::fold(
                &format!("X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:{uid}"),
                "\r\n",
            ));
        }
    }
    text.push_str(&format!("REV:{}\r\nUID:{uid}\r\nEND:VCARD\r\n", rev(now)));
    Card::parse(&text).map_err(|e| EditError::Invalid(e.to_string()))
}

/// A new contact card built from a patch (fullName defaults to the name
/// parts, else the organization, which then also shows as a company).
pub fn new_contact(
    uid: &str,
    patch: &ContactPatch,
    now: jiff::Timestamp,
) -> Result<Edited, EditError> {
    if !patch.update.is_empty() || !patch.remove.is_empty() || !patch.clear.is_empty() {
        return Err(EditError::Invalid(
            "a new contact takes only fields and added values".to_owned(),
        ));
    }
    let name = patch.name.clone().unwrap_or_default();
    let joined = [
        &name.prefix,
        &name.given,
        &name.middle,
        &name.family,
        &name.suffix,
    ]
    .iter()
    .filter_map(|p| p.as_deref())
    .map(str::trim)
    .filter(|p| !p.is_empty())
    .collect::<Vec<_>>()
    .join(" ");
    let organization = patch.organization.clone().unwrap_or_default();
    let full_name = patch
        .full_name
        .clone()
        .filter(|f| !f.trim().is_empty())
        .or_else(|| (!joined.is_empty()).then(|| joined.clone()))
        .or_else(|| (!organization.trim().is_empty()).then(|| organization.clone()))
        .ok_or_else(|| {
            EditError::Invalid("a new contact needs fullName, name or organization".to_owned())
        })?;
    check_text("fullName", &full_name, false)?;
    let company = joined.is_empty() && patch.full_name.is_none() && !organization.trim().is_empty();
    let mut base = format!(
        "BEGIN:VCARD\r\nVERSION:3.0\r\nPRODID:-//Omni//Contacts//EN\r\nN:;;;;\r\nFN:{}\r\n",
        escape_text(full_name.trim())
    );
    if company {
        base.push_str("X-ABShowAs:COMPANY\r\n");
    }
    base.push_str(&format!("REV:{}\r\nUID:{uid}\r\nEND:VCARD\r\n", rev(now)));
    let card = Card::parse(&base).map_err(|e| EditError::Invalid(e.to_string()))?;
    let mut patch = patch.clone();
    patch.full_name = None;
    let edited = apply_patch(&card, &patch)?;
    let mut changes = vec!["contact created".to_owned()];
    changes.extend(edited.changes);
    Ok(Edited {
        card: edited.card,
        changes,
    })
}

/// A deterministic UUID-shaped identifier from a seed (the idempotency key),
/// so a repeated create names the same resource.
pub fn uid_from_seed(seed: &str) -> String {
    let hex = omni_core::digest::sha256_hex(seed.as_bytes()).to_ascii_uppercase();
    let variant = match &hex[16..17] {
        "0" | "4" | "8" | "C" => "8",
        "1" | "5" | "9" | "D" => "9",
        "2" | "6" | "A" | "E" => "A",
        _ => "B",
    };
    format!(
        "{}-{}-4{}-{}{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        variant,
        &hex[17..20],
        &hex[20..32]
    )
}

// ---- verification ----

/// What a written card must look like, as property digests with names and
/// multiplicity (no values).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Expectation {
    /// Lines the write adds or changes to.
    pub present: Vec<(String, String)>,
    /// Lines the write removes or changes from.
    pub absent: Vec<(String, String)>,
    /// Lines the write leaves alone.
    pub preserved: Vec<(String, String)>,
}

fn expand(counts: BTreeMap<String, (String, u32)>) -> Vec<(String, String)> {
    counts
        .into_iter()
        .flat_map(|(key, (name, n))| std::iter::repeat_n((key, name), n as usize))
        .collect()
}

pub fn expectation(before: Option<&Card>, after: &Card) -> Expectation {
    let before = before.map(Card::key_counts).unwrap_or_default();
    let after = after.key_counts();
    let mut present = BTreeMap::new();
    let mut absent = BTreeMap::new();
    let mut preserved = BTreeMap::new();
    for (key, (name, n)) in &after {
        let had = before.get(key).map_or(0, |(_, b)| *b);
        let kept = had.min(*n);
        if kept > 0 {
            preserved.insert(key.clone(), (name.clone(), kept));
        }
        if *n > kept {
            present.insert(key.clone(), (name.clone(), n - kept));
        }
    }
    for (key, (name, b)) in &before {
        let now = after.get(key).map_or(0, |(_, n)| *n);
        if *b > now {
            absent.insert(key.clone(), (name.clone(), b - now));
        }
    }
    Expectation {
        present: expand(present),
        absent: expand(absent),
        preserved: expand(preserved),
    }
}

/// Differences between a read-back card and its expectation (property names).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Check {
    /// Intended lines that are missing.
    pub missing: Vec<String>,
    /// Lines that should have been removed but remain.
    pub lingering: Vec<String>,
    /// Untouched lines the server did not keep.
    pub lost: Vec<String>,
}

impl Check {
    pub fn applied(&self) -> bool {
        self.missing.is_empty() && self.lingering.is_empty()
    }

    pub fn clean(&self) -> bool {
        self.applied() && self.lost.is_empty()
    }
}

fn tally(items: &[(String, String)]) -> BTreeMap<&str, (&str, u32)> {
    let mut out: BTreeMap<&str, (&str, u32)> = BTreeMap::new();
    for (key, name) in items {
        out.entry(key.as_str()).or_insert((name.as_str(), 0)).1 += 1;
    }
    out
}

pub fn check(expectation: &Expectation, fetched: &Card) -> Check {
    let have = fetched.key_counts();
    let count = |key: &str| have.get(key).map_or(0, |(_, n)| *n);
    let present = tally(&expectation.present);
    let preserved = tally(&expectation.preserved);
    let absent = tally(&expectation.absent);
    let mut result = Check::default();
    for (key, (name, n)) in &present {
        let want = n + preserved.get(key).map_or(0, |(_, p)| *p);
        if count(key) < want {
            result.missing.push((*name).to_owned());
        }
    }
    for (key, (name, n)) in &preserved {
        if count(key) < *n && !present.contains_key(key) {
            result.lost.push((*name).to_owned());
        }
    }
    for (key, (name, _)) in &absent {
        let allowed =
            present.get(key).map_or(0, |(_, p)| *p) + preserved.get(key).map_or(0, |(_, p)| *p);
        if count(key) > allowed {
            result.lingering.push((*name).to_owned());
        }
    }
    for list in [&mut result.missing, &mut result.lingering, &mut result.lost] {
        list.sort();
        list.dedup();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nN:Doe;Jane;;;\r\nFN:Jane Doe\r\nitem1.EMAIL;type=INTERNET;type=pref:jane@example.com\r\nitem1.X-ABLabel:Book club\r\nTEL;type=CELL;type=VOICE:+1 555 0100\r\nPHOTO;ENCODING=b;TYPE=JPEG:AAAA\r\nX-APPLE-POSTER:opaque\r\nEND:VCARD\r\n";

    fn card(body: &str) -> Card {
        Card::parse(&format!(
            "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Test Person\r\n{body}END:VCARD\r\n"
        ))
        .unwrap()
    }

    fn id_of(card: &Card, name: &str) -> String {
        let (index, _) = card.named(name).next().unwrap();
        card.line_id(index).unwrap()
    }

    fn org_line(card: &Card) -> Option<String> {
        card.named("ORG")
            .next()
            .map(|(i, _)| card.lines()[i].text().to_owned())
    }

    #[test]
    fn a_value_edit_changes_only_its_line() {
        let card = Card::parse(CARD).unwrap();
        let patch = ContactPatch {
            update: vec![ValueUpdate {
                id: id_of(&card, "TEL"),
                value: Some("+15550100".to_owned()),
                address: None,
                label: None,
                preferred: None,
            }],
            ..ContactPatch::default()
        };
        let edited = apply_patch(&card, &patch).unwrap();
        assert_eq!(
            edited.card.to_text(),
            CARD.replace("+1 555 0100", "+15550100")
        );
        assert_eq!(edited.changes, vec!["TEL changed"]);
    }

    #[test]
    fn setting_the_same_values_is_a_no_op() {
        let card = Card::parse(CARD).unwrap();
        let patch = ContactPatch {
            full_name: Some("Jane Doe".to_owned()),
            name: Some(NamePatch {
                given: Some("Jane".to_owned()),
                ..NamePatch::default()
            }),
            update: vec![ValueUpdate {
                id: id_of(&card, "EMAIL"),
                value: Some("jane@example.com".to_owned()),
                address: None,
                label: Some("Book club".to_owned()),
                preferred: Some(true),
            }],
            ..ContactPatch::default()
        };
        let edited = apply_patch(&card, &patch).unwrap();
        assert!(edited.unchanged(), "{:?}", edited.changes);
        assert_eq!(edited.card.to_text(), CARD);
    }

    #[test]
    fn echoing_every_value_as_read_changes_nothing() {
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:x\r\nitem1.X-ABRELATEDNAMES:Sam\r\nitem1.X-ABLabel:_$!<Son>!$_\r\nitem2.EMAIL;type=INTERNET:a@example.com\r\nitem2.X-ABLabel:Home\r\nTEL;type=WORK;type=VOICE;type=pref:1\r\nitem3.URL:https\\://example.com\r\nitem3.X-ABLabel:_$!<HomePage>!$_\r\nEND:VCARD\r\n";
        let card = Card::parse(text).unwrap();
        let projection = model::project(&card);
        let patch = ContactPatch {
            update: projection
                .items
                .iter()
                .map(|item| ValueUpdate {
                    id: item.id.clone(),
                    value: Some(item.value.clone()),
                    address: None,
                    label: item.label.clone(),
                    preferred: Some(item.preferred),
                })
                .collect(),
            ..ContactPatch::default()
        };
        let edited = apply_patch(&card, &patch).unwrap();
        assert!(edited.unchanged(), "{:?}", edited.changes);
        assert_eq!(edited.card.to_text(), text);
    }

    #[test]
    fn custom_and_builtin_labels_are_written_like_apple() {
        let card = Card::parse(CARD).unwrap();
        let patch = ContactPatch {
            update: vec![
                ValueUpdate {
                    id: id_of(&card, "EMAIL"),
                    value: None,
                    address: None,
                    label: Some("work".to_owned()),
                    preferred: None,
                },
                ValueUpdate {
                    id: id_of(&card, "TEL"),
                    value: None,
                    address: None,
                    label: Some("Grandma's landline".to_owned()),
                    preferred: None,
                },
            ],
            ..ContactPatch::default()
        };
        let text = apply_patch(&card, &patch).unwrap().card.to_text();
        assert!(
            text.contains("item1.EMAIL;type=INTERNET;type=WORK;type=pref:jane@example.com\r\n")
        );
        assert!(!text.contains("Book club"));
        assert!(text.contains("item2.TEL:+1 555 0100\r\nitem2.X-ABLabel:Grandma's landline\r\n"));
        assert!(text.contains("PHOTO;ENCODING=b;TYPE=JPEG:AAAA\r\nX-APPLE-POSTER:opaque\r\n"));
    }

    #[test]
    fn label_changes_keep_other_parameters_as_written() {
        let card = card(
            "item1.EMAIL;type=INTERNET;X-SERVICE=\"a:b;c\";X-CARET=^'x^';type=HOME:a@example.com\r\n",
        );
        let patch = ContactPatch {
            update: vec![ValueUpdate {
                id: id_of(&card, "EMAIL"),
                value: None,
                address: None,
                label: Some("work".to_owned()),
                preferred: None,
            }],
            ..ContactPatch::default()
        };
        let edited = apply_patch(&card, &patch).unwrap().card;
        let (index, _) = edited.named("EMAIL").next().unwrap();
        assert_eq!(
            edited.lines()[index].text(),
            "item1.EMAIL;X-SERVICE=\"a:b;c\";X-CARET=^'x^';type=INTERNET;type=WORK:a@example.com"
        );
    }

    #[test]
    fn a_label_shared_by_two_values_is_not_changed_alone() {
        let card = card(
            "item1.EMAIL;type=INTERNET:a@example.com\r\nitem1.TEL:1\r\nitem1.X-ABLabel:Shared\r\n",
        );
        let patch = ContactPatch {
            update: vec![ValueUpdate {
                id: id_of(&card, "EMAIL"),
                value: None,
                address: None,
                label: Some("Other text".to_owned()),
                preferred: None,
            }],
            ..ContactPatch::default()
        };
        assert!(matches!(
            apply_patch(&card, &patch),
            Err(EditError::Unsupported(_))
        ));
    }

    #[test]
    fn removing_a_grouped_value_removes_its_label_and_nothing_else() {
        let card = Card::parse(CARD).unwrap();
        let patch = ContactPatch {
            remove: vec![id_of(&card, "EMAIL")],
            ..ContactPatch::default()
        };
        let text = apply_patch(&card, &patch).unwrap().card.to_text();
        assert_eq!(
            text,
            CARD.replace(
                "item1.EMAIL;type=INTERNET;type=pref:jane@example.com\r\nitem1.X-ABLabel:Book club\r\n",
                ""
            )
        );
    }

    #[test]
    fn stale_ids_and_duplicated_single_fields_are_refused() {
        let card = Card::parse(CARD).unwrap();
        let stale = ContactPatch {
            remove: vec!["000000000000".to_owned()],
            ..ContactPatch::default()
        };
        assert!(matches!(
            apply_patch(&card, &stale),
            Err(EditError::UnknownValue(_))
        ));
        let two_notes = card_with("NOTE:a\r\nNOTE:b\r\n");
        let note = ContactPatch {
            note: Some("c".to_owned()),
            ..ContactPatch::default()
        };
        assert!(matches!(
            apply_patch(&two_notes, &note),
            Err(EditError::Ambiguous(_))
        ));
    }

    fn card_with(body: &str) -> Card {
        card(body)
    }

    #[test]
    fn department_edits_extend_or_create_org() {
        let department = ContactPatch {
            department: Some("Research".to_owned()),
            ..ContactPatch::default()
        };
        let one = apply_patch(&card("ORG:Acme\r\n"), &department).unwrap();
        assert_eq!(org_line(&one.card).as_deref(), Some("ORG:Acme;Research"));
        let none = apply_patch(&card(""), &department).unwrap();
        assert_eq!(org_line(&none.card).as_deref(), Some("ORG:;Research"));
        let created = new_contact(
            "U",
            &ContactPatch {
                full_name: Some("X".to_owned()),
                department: Some("Research".to_owned()),
                ..ContactPatch::default()
            },
            jiff::Timestamp::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(org_line(&created.card).as_deref(), Some("ORG:;Research"));
    }

    #[test]
    fn organization_set_and_department_clear_apply_together() {
        let base = card("ORG:Acme;Research\r\n");
        let both = apply_patch(
            &base,
            &ContactPatch {
                organization: Some("NewCo".to_owned()),
                clear: vec![SingleField::Department],
                ..ContactPatch::default()
            },
        )
        .unwrap();
        assert_eq!(org_line(&both.card).as_deref(), Some("ORG:NewCo"));
        let cleared = apply_patch(
            &base,
            &ContactPatch {
                clear: vec![SingleField::Organization, SingleField::Department],
                ..ContactPatch::default()
            },
        )
        .unwrap();
        assert_eq!(org_line(&cleared.card), None);
        assert_eq!(cleared.changes, vec!["ORG removed"]);
    }

    #[test]
    fn untouched_structured_components_keep_their_raw_text() {
        let base = card(
            "item1.ADR;type=HOME:;;1 Main St,Apt 2;Springfield;IL;62701;USA\r\nitem1.X-ABADR:us\r\nORG:A\\,B;Dept,X\r\n",
        );
        let edited = apply_patch(
            &base,
            &ContactPatch {
                organization: Some("C".to_owned()),
                update: vec![ValueUpdate {
                    id: id_of(&base, "ADR"),
                    value: None,
                    address: Some(AddressPatch {
                        city: Some("Shelbyville".to_owned()),
                        ..AddressPatch::default()
                    }),
                    label: None,
                    preferred: None,
                }],
                ..ContactPatch::default()
            },
        )
        .unwrap();
        let text = edited.card.to_text();
        assert!(
            text.contains("item1.ADR;type=HOME:;;1 Main St,Apt 2;Shelbyville;IL;62701;USA\r\n")
        );
        assert!(text.contains("ORG:C;Dept,X\r\n"));
    }

    #[test]
    fn birthdays_without_a_year_use_apples_omit_year_form() {
        let base = card("BDAY;X-APPLE-OMIT-YEAR=1604:1604-03-15\r\n");
        let same = apply_patch(
            &base,
            &ContactPatch {
                birthday: Some("--03-15".to_owned()),
                ..ContactPatch::default()
            },
        )
        .unwrap();
        assert!(same.unchanged());
        let dated = apply_patch(
            &base,
            &ContactPatch {
                birthday: Some("1990-03-15".to_owned()),
                ..ContactPatch::default()
            },
        )
        .unwrap();
        assert_eq!(dated.card.first("BDAY").unwrap().value, "1990-03-15");
        assert!(
            dated
                .card
                .first("BDAY")
                .unwrap()
                .param("X-APPLE-OMIT-YEAR")
                .is_none()
        );
        let undated = apply_patch(
            &card("BDAY;value=date:1990-03-15\r\n"),
            &ContactPatch {
                birthday: Some("--04-01".to_owned()),
                ..ContactPatch::default()
            },
        )
        .unwrap();
        let line = undated
            .card
            .lines()
            .iter()
            .find(|l| l.text().starts_with("BDAY"))
            .unwrap();
        assert_eq!(
            line.text(),
            "BDAY;value=date;X-APPLE-OMIT-YEAR=1604:1604-04-01"
        );
    }

    #[test]
    fn birthdays_must_be_real_dates() {
        let base = card("");
        for bad in [
            "1990-13-45",
            "2001-02-29",
            "--13-01",
            "--02-30",
            "90-01-01",
            "1990-1-01",
            "+990-01-01",
        ] {
            let patch = ContactPatch {
                birthday: Some(bad.to_owned()),
                ..ContactPatch::default()
            };
            assert!(apply_patch(&base, &patch).is_err(), "{bad} accepted");
        }
        for good in ["2000-02-29", "1990-12-31", "--02-29"] {
            let patch = ContactPatch {
                birthday: Some(good.to_owned()),
                ..ContactPatch::default()
            };
            assert!(apply_patch(&base, &patch).is_ok(), "{good} refused");
        }
    }

    #[test]
    fn new_im_and_social_values_carry_their_service() {
        let base = card("");
        let add = |field: Field, service: Option<&str>, user: Option<&str>| ContactPatch {
            add: vec![NewValue {
                field,
                value: Some("skype:someone".to_owned()),
                address: None,
                label: None,
                preferred: false,
                service: service.map(str::to_owned),
                user: user.map(str::to_owned),
            }],
            ..ContactPatch::default()
        };
        assert!(apply_patch(&base, &add(Field::InstantMessage, None, None)).is_err());
        assert!(apply_patch(&base, &add(Field::Email, Some("Skype"), None)).is_err());
        assert!(apply_patch(&base, &add(Field::InstantMessage, Some("Skype"), Some("x"))).is_err());
        let im = apply_patch(&base, &add(Field::InstantMessage, Some("Skype"), None)).unwrap();
        let item = model::project(&im.card).items[0].clone();
        assert_eq!(item.service.as_deref(), Some("Skype"));
        let social = apply_patch(
            &base,
            &add(Field::SocialProfile, Some("Mastodon"), Some("someone")),
        )
        .unwrap();
        let item = model::project(&social.card).items[0].clone();
        assert_eq!(item.service.as_deref(), Some("mastodon"));
        assert_eq!(item.user.as_deref(), Some("someone"));
    }

    #[test]
    fn label_edits_keep_a_social_profiles_service_case() {
        let base =
            card("X-SOCIALPROFILE;type=twitter;x-user=someone:https://twitter.com/someone\r\n");
        let patch = ContactPatch {
            update: vec![ValueUpdate {
                id: id_of(&base, "X-SOCIALPROFILE"),
                value: None,
                address: None,
                label: Some("work".to_owned()),
                preferred: None,
            }],
            ..ContactPatch::default()
        };
        let edited = apply_patch(&base, &patch).unwrap().card;
        let (index, _) = edited.named("X-SOCIALPROFILE").next().unwrap();
        assert_eq!(
            edited.lines()[index].text(),
            "item1.X-SOCIALPROFILE;x-user=someone;type=twitter:https://twitter.com/someone"
        );
        assert_eq!(
            edited.lines()[index + 1].text(),
            "item1.X-ABLabel:_$!<Work>!$_"
        );
    }

    #[test]
    fn an_unknown_apple_label_token_can_be_copied_to_a_new_value() {
        let base = card("item1.X-ABRELATEDNAMES:Sam\r\nitem1.X-ABLabel:_$!<Son>!$_\r\n");
        let token = model::project(&base).items[0].label_token.clone().unwrap();
        assert_eq!(token, "_$!<Son>!$_");
        let patch = ContactPatch {
            add: vec![NewValue {
                field: Field::RelatedName,
                value: Some("Alex".to_owned()),
                address: None,
                label: Some(token),
                preferred: false,
                service: None,
                user: None,
            }],
            ..ContactPatch::default()
        };
        let text = apply_patch(&base, &patch).unwrap().card.to_text();
        assert!(text.contains("item2.X-ABRELATEDNAMES:Alex\r\nitem2.X-ABLabel:_$!<Son>!$_\r\n"));
    }

    #[test]
    fn exhausted_item_numbers_refuse_instead_of_panicking() {
        let base = card("item18446744073709551615.EMAIL;type=INTERNET:a@example.com\r\n");
        let patch = ContactPatch {
            add: vec![NewValue {
                field: Field::Email,
                value: Some("b@example.com".to_owned()),
                address: None,
                label: Some("Custom".to_owned()),
                preferred: false,
                service: None,
                user: None,
            }],
            ..ContactPatch::default()
        };
        assert!(matches!(
            apply_patch(&base, &patch),
            Err(EditError::Unsupported(_))
        ));
        let large = card("item4294967295.EMAIL;type=INTERNET:a@example.com\r\n");
        let text = apply_patch(&large, &patch).unwrap().card.to_text();
        assert!(text.contains("item4294967296.X-ABLabel:Custom\r\n"));
    }

    #[test]
    fn social_and_im_values_are_not_rewritten_under_stale_metadata() {
        let base = card("X-SOCIALPROFILE;type=twitter;x-user=old:https://twitter.com/old\r\n");
        let patch = ContactPatch {
            update: vec![ValueUpdate {
                id: id_of(&base, "X-SOCIALPROFILE"),
                value: Some("https://twitter.com/new".to_owned()),
                address: None,
                label: None,
                preferred: None,
            }],
            ..ContactPatch::default()
        };
        assert!(matches!(
            apply_patch(&base, &patch),
            Err(EditError::Unsupported(_))
        ));
    }

    #[test]
    fn injected_line_breaks_are_refused() {
        let base = card("NOTE:x\r\n");
        for patch in [
            ContactPatch {
                note: Some("a\r\nX-INJECT:1".to_owned()),
                ..ContactPatch::default()
            },
            ContactPatch {
                full_name: Some("a\nX-INJECT:1".to_owned()),
                ..ContactPatch::default()
            },
            ContactPatch {
                add: vec![NewValue {
                    field: Field::Url,
                    value: Some("https://x\nX-INJECT:1".to_owned()),
                    address: None,
                    label: Some("x\nX".to_owned()),
                    preferred: false,
                    service: None,
                    user: None,
                }],
                ..ContactPatch::default()
            },
        ] {
            assert!(apply_patch(&base, &patch).is_err());
        }
        let multiline = apply_patch(
            &base,
            &ContactPatch {
                note: Some("line1\nline2".to_owned()),
                ..ContactPatch::default()
            },
        )
        .unwrap();
        assert_eq!(multiline.card.first("NOTE").unwrap().value, "line1\\nline2");
        let mut raw = Edits::default();
        raw.append("NOTE:a\r\nX-INJECT:1".to_owned());
        assert_eq!(raw.apply(&base), Err(VcardError::LineBreak));
    }

    #[test]
    fn membership_changes_touch_only_member_lines() {
        let group = Card::parse("BEGIN:VCARD\r\nVERSION:3.0\r\nN:Friends\r\nFN:Friends\r\nX-ADDRESSBOOKSERVER-KIND:group\r\nX-ADDRESSBOOKSERVER-MEMBER:urn:uuid:AAA\r\nX-ADDRESSBOOKSERVER-MEMBER:urn:uuid:BBB\r\nX-APPLE-GROUP-FUTURE:keep\r\nEND:VCARD\r\n").unwrap();
        let edited = change_members(
            &group,
            &["CCC".to_owned(), "aaa".to_owned()],
            &["BBB".to_owned()],
        )
        .unwrap();
        assert_eq!(
            edited.card.to_text(),
            "BEGIN:VCARD\r\nVERSION:3.0\r\nN:Friends\r\nFN:Friends\r\nX-ADDRESSBOOKSERVER-KIND:group\r\nX-ADDRESSBOOKSERVER-MEMBER:urn:uuid:AAA\r\nX-ADDRESSBOOKSERVER-MEMBER:urn:uuid:CCC\r\nX-APPLE-GROUP-FUTURE:keep\r\nEND:VCARD\r\n"
        );
        let noop = change_members(&group, &["AAA".to_owned()], &["ZZZ".to_owned()]).unwrap();
        assert!(noop.changes.is_empty());
        assert_eq!(noop.card.to_text(), group.to_text());
    }

    #[test]
    fn non_ascii_member_values_are_kept_on_membership_changes() {
        let group = Card::parse("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:G\r\nX-ADDRESSBOOKSERVER-KIND:group\r\nX-ADDRESSBOOKSERVER-MEMBER:\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\r\nEND:VCARD\r\n").unwrap();
        let edited = change_members(&group, &["AAA".to_owned()], &[]).unwrap();
        assert_eq!(
            model::project(&edited.card).members,
            vec!["\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}", "AAA"]
        );
    }

    #[test]
    fn verification_reports_lost_untouched_properties() {
        let before = Card::parse(CARD).unwrap();
        let patch = ContactPatch {
            note: Some("hello".to_owned()),
            ..ContactPatch::default()
        };
        let after = apply_patch(&before, &patch).unwrap().card;
        let expect = expectation(Some(&before), &after);
        assert!(check(&expect, &after).clean());
        let dropped =
            Card::parse(&after.to_text().replace("X-APPLE-POSTER:opaque\r\n", "")).unwrap();
        let result = check(&expect, &dropped);
        assert!(result.applied());
        assert_eq!(result.lost, vec!["X-APPLE-POSTER"]);
        assert!(!check(&expect, &before).applied());
    }

    #[test]
    fn verification_sees_labels_moved_between_values() {
        let before = card(
            "item1.EMAIL;type=INTERNET:a@example.com\r\nitem1.X-ABLabel:Alpha\r\nitem2.EMAIL;type=INTERNET:b@example.com\r\nitem2.X-ABLabel:Beta\r\n",
        );
        let expect = expectation(Some(&before), &before);
        let swapped = card(
            "item1.EMAIL;type=INTERNET:a@example.com\r\nitem1.X-ABLabel:Beta\r\nitem2.EMAIL;type=INTERNET:b@example.com\r\nitem2.X-ABLabel:Alpha\r\n",
        );
        assert!(!check(&expect, &swapped).clean());
        let ungrouped = card(
            "EMAIL;type=INTERNET:a@example.com\r\nX-ABLabel:Alpha\r\nEMAIL;type=INTERNET:b@example.com\r\nX-ABLabel:Beta\r\n",
        );
        assert!(!check(&expect, &ungrouped).clean());
        let renumbered = card(
            "item7.EMAIL;type=INTERNET:a@example.com\r\nitem7.X-ABLabel:Alpha\r\nitem9.EMAIL;type=INTERNET:b@example.com\r\nitem9.X-ABLabel:Beta\r\n",
        );
        assert!(check(&expect, &renumbered).clean());
    }

    #[test]
    fn seeded_uids_are_stable_v4_shapes() {
        let uid = uid_from_seed("key-1234567890abcdef");
        assert_eq!(uid, uid_from_seed("key-1234567890abcdef"));
        assert_eq!(uid.len(), 36);
        assert_eq!(&uid[14..15], "4");
    }
}
