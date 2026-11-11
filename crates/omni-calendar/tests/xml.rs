//! CalDAV multistatus parsing.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use omni_calendar::caldav::xml::{
    CalendarCollection, extract_calendar_collections, extract_property_href,
    extract_response_hrefs_containing, pick_calendar_collection,
};

const ICLOUD_PRINCIPAL_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:">
  <response>
    <href>/</href>
    <propstat>
      <prop>
        <current-user-principal>
          <href>/123456789/principal/</href>
        </current-user-principal>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
</multistatus>"#;

const PREFIXED_HOME_SET_XML: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/123456789/principal/</d:href>
    <d:propstat>
      <d:prop>
        <c:calendar-home-set>
          <d:href>https://p42-caldav.icloud.com/123456789/calendars/</d:href>
        </c:calendar-home-set>
      </d:prop>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

const COLLECTIONS_XML: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/">
  <d:response>
    <d:href>/123456789/calendars/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/123456789/calendars/home/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Home</d:displayname>
      <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      <c:supported-calendar-component-set>
        <c:comp name="VEVENT"/>
      </c:supported-calendar-component-set>
    </d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/123456789/calendars/personal-cal/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Personal</d:displayname>
      <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      <c:supported-calendar-component-set>
        <c:comp name="VEVENT"/>
      </c:supported-calendar-component-set>
    </d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/123456789/calendars/tasks/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Reminders</d:displayname>
      <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      <c:supported-calendar-component-set>
        <c:comp name="VTODO"/>
      </c:supported-calendar-component-set>
    </d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;

fn coll(href: &str, name: &str, components: &[&str]) -> CalendarCollection {
    CalendarCollection {
        href: href.to_owned(),
        name: name.to_owned(),
        components: Some(components.iter().map(|c| (*c).to_owned()).collect()),
    }
}

// describe("extractPropertyHref")

#[test]
fn finds_current_user_principal_in_unprefixed_icloud_xml() {
    assert_eq!(
        extract_property_href(ICLOUD_PRINCIPAL_XML, "current-user-principal").as_deref(),
        Some("/123456789/principal/")
    );
}

#[test]
fn finds_calendar_home_set_in_prefixed_xml() {
    assert_eq!(
        extract_property_href(PREFIXED_HOME_SET_XML, "calendar-home-set").as_deref(),
        Some("https://p42-caldav.icloud.com/123456789/calendars/")
    );
}

#[test]
fn returns_undefined_when_the_property_is_absent() {
    assert_eq!(
        extract_property_href(ICLOUD_PRINCIPAL_XML, "calendar-home-set"),
        None
    );
}

// describe("extractCalendarCollections")

#[test]
fn extracts_only_calendar_collections_with_names_and_components() {
    assert_eq!(
        extract_calendar_collections(COLLECTIONS_XML),
        vec![
            coll("/123456789/calendars/home/", "Home", &["VEVENT"]),
            coll(
                "/123456789/calendars/personal-cal/",
                "Personal",
                &["VEVENT"]
            ),
            coll("/123456789/calendars/tasks/", "Reminders", &["VTODO"]),
        ]
    );
}

#[test]
fn handles_iclouds_unprefixed_xmlns_attribute_style_with_single_quoted_comps() {
    let xml = r#"<multistatus xmlns="DAV:">
      <response xmlns="DAV:">
        <href>/285128981/calendars/ABCD-1234/</href>
        <propstat><prop>
          <displayname xmlns="DAV:">Personal</displayname>
          <resourcetype xmlns="DAV:"><collection/><calendar xmlns="urn:ietf:params:xml:ns:caldav"/></resourcetype>
          <supported-calendar-component-set xmlns="urn:ietf:params:xml:ns:caldav"><comp name='VEVENT' xmlns='urn:ietf:params:xml:ns:caldav'/></supported-calendar-component-set>
        </prop><status>HTTP/1.1 200 OK</status></propstat>
      </response>
      <response xmlns="DAV:">
        <href>/285128981/calendars/tasks/</href>
        <propstat><prop>
          <displayname xmlns="DAV:">Shopping / Home</displayname>
          <resourcetype xmlns="DAV:"><collection/><calendar xmlns="urn:ietf:params:xml:ns:caldav"/></resourcetype>
          <supported-calendar-component-set xmlns="urn:ietf:params:xml:ns:caldav"><comp name='VTODO' xmlns='urn:ietf:params:xml:ns:caldav'/></supported-calendar-component-set>
        </prop><status>HTTP/1.1 200 OK</status></propstat>
      </response>
    </multistatus>"#;
    assert_eq!(
        extract_calendar_collections(xml),
        vec![
            coll("/285128981/calendars/ABCD-1234/", "Personal", &["VEVENT"]),
            coll("/285128981/calendars/tasks/", "Shopping / Home", &["VTODO"]),
        ]
    );
}

#[test]
fn ignores_calendar_proxy_resource_types() {
    let xml = r#"<multistatus><response><href>/p/proxy/</href><propstat><prop>
      <resourcetype><collection/><calendar-proxy-read/></resourcetype>
    </prop></propstat></response></multistatus>"#;
    assert!(extract_calendar_collections(xml).is_empty());
}

// describe("pickCalendarCollection")

#[test]
fn prefers_the_configured_name_case_insensitively() {
    let collections = extract_calendar_collections(COLLECTIONS_XML);
    assert_eq!(
        pick_calendar_collection(&collections, Some("personal")).map(|c| c.name.as_str()),
        Some("Personal")
    );
}

#[test]
fn falls_back_to_a_default_sounding_vevent_calendar_in_preference_order() {
    let collections = extract_calendar_collections(COLLECTIONS_XML);
    assert_eq!(
        pick_calendar_collection(&collections, None).map(|c| c.name.as_str()),
        Some("Personal")
    );
}

#[test]
fn prefers_the_account_default_calendar_named_icloud_when_present() {
    let mut collections = extract_calendar_collections(COLLECTIONS_XML);
    collections.push(coll("/x/calendars/work/", "iCloud", &["VEVENT"]));
    assert_eq!(
        pick_calendar_collection(&collections, None).map(|c| c.name.as_str()),
        Some("iCloud")
    );
}

#[test]
fn never_picks_a_vtodo_only_collection() {
    let collections = extract_calendar_collections(COLLECTIONS_XML);
    assert_ne!(
        pick_calendar_collection(&collections, Some("Reminders")).map(|c| c.name.as_str()),
        Some("Reminders")
    );
}

#[test]
fn returns_undefined_when_nothing_is_event_capable() {
    let collections: Vec<CalendarCollection> = extract_calendar_collections(COLLECTIONS_XML)
        .into_iter()
        .filter(|c| c.name == "Reminders")
        .collect();
    assert!(pick_calendar_collection(&collections, None).is_none());
}

#[test]
fn lists_the_href_of_each_report_match_whose_data_has_the_needle() {
    let xml = r#"<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <response><href>/1/calendars/work/x.ics</href><propstat><prop><c:calendar-data>BEGIN:VEVENT
UID:one
END:VEVENT</c:calendar-data></prop></propstat></response>
  <D:response xmlns:D="DAV:"><D:href> /1/calendars/work/y.ics </D:href><c:calendar-data><![CDATA[UID:two]]></c:calendar-data></D:response>
</multistatus>"#;
    assert_eq!(
        extract_response_hrefs_containing(xml, "UID:two"),
        ["/1/calendars/work/y.ics"]
    );
    assert_eq!(
        extract_response_hrefs_containing(xml, "UID:"),
        ["/1/calendars/work/x.ics", "/1/calendars/work/y.ics"]
    );
    assert!(extract_response_hrefs_containing(xml, "UID:three").is_empty());
}
