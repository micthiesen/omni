//! Lossless edits beside lines the parser cannot read, and malformed cards
//! that must be refused or kept verbatim. Synthetic cards only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use omni_contacts::edit::{ContactPatch, ValueUpdate, apply_patch, ensure_editable};
use omni_contacts::model;
use omni_contacts::vcard::{Card, VcardError};

fn folded_photo() -> String {
    let photo: String = (0..600)
        .map(|i| char::from(b'A' + (i % 26) as u8))
        .collect();
    let mut line =
        String::from("PHOTO;ENCODING=b;TYPE=JPEG;X-ABCROP-RECTANGLE=ABClipRect_1&0&0&10&10&abc=:");
    line.push_str(&photo[..40]);
    for chunk in photo.as_bytes()[40..].chunks(74) {
        line.push_str("\r\n ");
        line.push_str(std::str::from_utf8(chunk).unwrap());
    }
    line.push_str("\r\n");
    line
}

#[test]
fn edits_beside_unparsed_and_folded_lines_keep_them_byte_for_byte() {
    let photo = folded_photo();
    let kept = [
        photo.as_str(),
        "X-APPLE-SYNTH-POSTER;X-P=\"a:b;c\";type=X-ONE,X-TWO:opaque\\,data\r\n\tfolded-with-tab\r\n",
        "X-BROKEN;X-Q=\"unterminated:value\r\n",
        "not a content line\r\n",
        "X-ABShowAs:PERSON\r\n",
    ];
    let text = format!(
        "BEGIN:VCARD\r\nVERSION:3.0\r\nPRODID:-//Apple Inc.//iPhone OS 18.1//EN\r\nN:Doe;Jane;;;\r\nFN:Jane Doe\r\n{}{}{}{}\r\nitem1.TEL;type=CELL;type=VOICE:+1 555 0100\r\nitem1.X-ABLabel:_$!<Mobile>!$_\r\n{}END:VCARD\r\n",
        kept[0], kept[1], kept[2], kept[3], kept[4]
    );
    let card = Card::parse(&text).unwrap();
    assert_eq!(card.to_text(), text);
    assert!(
        card.first("X-BROKEN").is_none(),
        "an unterminated quote is not a property"
    );
    let (index, _) = card.named("TEL").next().unwrap();
    let edited = apply_patch(
        &card,
        &ContactPatch {
            update: vec![ValueUpdate {
                id: card.line_id(index).unwrap(),
                value: Some("+1 555 0199".to_owned()),
                address: None,
                label: Some("Kid's phone".to_owned()),
                preferred: Some(true),
            }],
            note: Some("hello".to_owned()),
            ..ContactPatch::default()
        },
    )
    .unwrap();
    let out = edited.card.to_text();
    for line in kept {
        assert!(out.contains(line), "lost {line:?}");
    }
    let tel = edited
        .card
        .lines()
        .iter()
        .find(|l| l.text().contains(".TEL"))
        .unwrap();
    assert_eq!(tel.text(), "item1.TEL;type=pref:+1 555 0199");
    assert!(out.contains("item1.X-ABLabel:Kid's phone\r\n"));
}

#[test]
fn content_outside_the_card_is_refused_not_interpreted() {
    let trailing = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:x\r\nX-ADDRESSBOOKSERVER-KIND:group\r\nEND:VCARD\r\nX-ADDRESSBOOKSERVER-MEMBER:urn:uuid:ZZZ\r\n";
    assert_eq!(Card::parse(trailing), Err(VcardError::OutsideContent));
    let leading = "X-FOO:1\r\nBEGIN:VCARD\r\nVERSION:3.0\r\nFN:x\r\nEND:VCARD\r\n";
    assert_eq!(Card::parse(leading), Err(VcardError::OutsideContent));
    let blank_tail = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:x\r\nEND:VCARD\r\n\r\n";
    assert_eq!(Card::parse(blank_tail).unwrap().to_text(), blank_tail);
}

#[test]
fn a_byte_order_mark_is_read_and_kept() {
    let text = "\u{feff}BEGIN:VCARD\r\nVERSION:3.0\r\nFN:x\r\nEND:VCARD\r\n";
    let card = Card::parse(text).unwrap();
    assert_eq!(card.to_text(), text);
    assert_eq!(model::project(&card).full_name.as_deref(), Some("x"));
}

#[test]
fn bare_carriage_returns_are_not_a_card() {
    assert!(Card::parse("BEGIN:VCARD\rVERSION:3.0\rFN:x\rEND:VCARD\r").is_err());
}

#[test]
fn vcard_2_1_cards_are_read_but_never_edited() {
    let text = "BEGIN:VCARD\r\nVERSION:2.1\r\nFN:x\r\nNOTE;ENCODING=QUOTED-PRINTABLE:a=\r\nb\r\nEND:VCARD\r\n";
    let card = Card::parse(text).unwrap();
    assert_eq!(card.to_text(), text);
    assert!(ensure_editable(&card).is_err());
    let patch = ContactPatch {
        note: Some("c".to_owned()),
        ..ContactPatch::default()
    };
    assert_eq!(
        apply_patch(&card, &patch).unwrap_err().code(),
        "unsupported"
    );
}
