//! Element and attribute tag tables of the EPG binary encoding (ETSI TS 102 371).
//!
//! Source: Dream `util-QT/epgdec.cpp` (`element_tables`, `attribute_tags*`, `enums*`),
//! which was written from the specification by one of its authors. Deviations, made
//! because Dream's tables are internally inconsistent (TS 102 371 is not available
//! locally to settle them), are marked `DEVIATION`:
//!
//! * `ensemble`, `frequency`, `service` and `serviceID` use the attribute sets their
//!   XML counterparts have in TS 102 818 (Dream's assignment is shifted by one element:
//!   it gives `frequency` the attributes of `service`, and so on).
//! * `programmeGroup` starts with `id` like `programme` does (Dream omits it).
//! * `multimedia` `type` also lists the two rectangle logo types.
//!
//! Attribute tags are 0x80 + index into the element's attribute list.

/// How an attribute value is coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttrKind {
    /// One byte; value n (1-based) selects the n-th name (empty names are unused codes).
    Enum(&'static [&'static str]),
    /// Text (UTF-8, may contain string tokens).
    String,
    /// 16-bit unsigned.
    U16,
    /// 24-bit unsigned.
    U24,
    /// Time point (see [`crate::time`]).
    Time,
    /// Duration in seconds, 16 bits.
    Duration,
    /// 3-byte identifier, shown as dotted hex bytes.
    Sid,
    /// TV-Anytime genre reference: scheme byte + up to 3 term bytes.
    Genre,
    /// Bitrate in units of 0.1 kbit/s, 16 bits (Dream multiplies instead of dividing).
    Bitrate,
    /// Reserved slot ("Not used" in Dream).
    Unused,
}

/// One attribute definition.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AttrDef {
    pub name: &'static str,
    pub kind: AttrKind,
}

/// One element definition.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ElementDef {
    pub tag: u8,
    pub name: &'static str,
    pub attrs: &'static [AttrDef],
}

impl ElementDef {
    /// Attribute for tag `tag` (0x80..).
    pub(crate) fn attr(&self, tag: u8) -> Option<&'static AttrDef> {
        let def = self.attrs.get(usize::from(tag.checked_sub(0x80)?))?;
        if def.kind == AttrKind::Unused {
            None
        } else {
            Some(def)
        }
    }

    /// (tag, definition) of the attribute called `name`.
    pub(crate) fn attr_by_name(&self, name: &str) -> Option<(u8, &'static AttrDef)> {
        self.attrs
            .iter()
            .position(|a| a.name == name && a.kind != AttrKind::Unused)
            .map(|i| (0x80 + i as u8, &self.attrs[i]))
    }
}

const fn a(name: &'static str, kind: AttrKind) -> AttrDef {
    AttrDef { name, kind }
}

const SYSTEM: &[&str] = &["DAB", "DRM"];
const PROGRAMME_GROUP_TYPE: &[&str] = &[
    "",
    "series",
    "show",
    "programConcept",
    "magazine",
    "programCompilation",
    "otherCollection",
    "otherChoice",
    "topic",
];
const PROTOCOL: &[&str] = &["URL", "DAB", "DRM"];
const ALTERNATE_SOURCE_TYPE: &[&str] = &["identical", "more", "less", "similar"];
const FREQUENCY_TYPE: &[&str] = &["primary", "alternative"];
const SERVICE_FORMAT: &[&str] = &[
    "audio",
    "DLS",
    "MOTSlideshow",
    "MOTBWS",
    "TPEG",
    "DGPS",
    "proprietary",
];
const SERVICE_ID_TYPE: &[&str] = &["primary", "secondary"];
const CA_TYPE: &[&str] = &["none", "unspecified"];
const BROADCAST: &[&str] = &["on-air", "off-air"];
const RECOMMENDATION: &[&str] = &["no", "yes"];
const MULTIMEDIA_TYPE: &[&str] = &[
    "",
    "logo_unrestricted",
    "logo_mono_square",
    "logo_colour_square",
    "logo_mono_rectangle",
    "logo_colour_rectangle",
];
const GENRE_TYPE: &[&str] = &["main", "secondary", "other"];

const LANG: &[AttrDef] = &[a("xml:lang", AttrKind::String)];
const EPG_ATTRS: &[AttrDef] = &[
    a("system", AttrKind::Enum(SYSTEM)),
    a("id", AttrKind::String),
];
const SI_ATTRS: &[AttrDef] = &[
    a("version", AttrKind::U16),
    a("creationTime", AttrKind::Time),
    a("originator", AttrKind::String),
    a("serviceProvider", AttrKind::String),
    a("system", AttrKind::Enum(SYSTEM)),
];
const SCHEDULE_ATTRS: &[AttrDef] = &[
    a("version", AttrKind::U16),
    a("creationTime", AttrKind::Time),
    a("originator", AttrKind::String),
];
const PROGRAMME_ATTRS: &[AttrDef] = &[
    a("id", AttrKind::String),
    a("shortId", AttrKind::U24),
    a("version", AttrKind::U16),
    a("recommendation", AttrKind::Enum(RECOMMENDATION)),
    a("broadcast", AttrKind::Enum(BROADCAST)),
    a("", AttrKind::Unused),
    a("xml:lang", AttrKind::String),
    a("bitrate", AttrKind::String),
];
// DEVIATION: Dream starts at shortId.
const PROGRAMME_GROUP_ATTRS: &[AttrDef] = &[
    a("id", AttrKind::String),
    a("shortId", AttrKind::U24),
    a("version", AttrKind::U16),
    a("type", AttrKind::Enum(PROGRAMME_GROUP_TYPE)),
    a("numOfItems", AttrKind::U16),
];
const TIME_ATTRS: &[AttrDef] = &[
    a("time", AttrKind::Time),
    a("duration", AttrKind::Duration),
    a("actualTime", AttrKind::Time),
    a("actualDuration", AttrKind::Duration),
];
const BEARER_ATTRS: &[AttrDef] = &[a("id", AttrKind::Sid), a("trigger", AttrKind::U16)];

/// All elements, by tag.
pub(crate) const ELEMENTS: &[ElementDef] = &[
    ElementDef {
        tag: 0x02,
        name: "epg",
        attrs: EPG_ATTRS,
    },
    ElementDef {
        tag: 0x03,
        name: "serviceInformation",
        attrs: SI_ATTRS,
    },
    ElementDef {
        tag: 0x10,
        name: "shortName",
        attrs: LANG,
    },
    ElementDef {
        tag: 0x11,
        name: "mediumName",
        attrs: LANG,
    },
    ElementDef {
        tag: 0x12,
        name: "longName",
        attrs: LANG,
    },
    ElementDef {
        tag: 0x13,
        name: "mediaDescription",
        attrs: &[],
    },
    ElementDef {
        tag: 0x14,
        name: "genre",
        attrs: &[
            a("href", AttrKind::Genre),
            a("type", AttrKind::Enum(GENRE_TYPE)),
        ],
    },
    ElementDef {
        tag: 0x15,
        name: "CA",
        attrs: &[a("type", AttrKind::Enum(CA_TYPE))],
    },
    ElementDef {
        tag: 0x16,
        name: "keywords",
        attrs: LANG,
    },
    ElementDef {
        tag: 0x17,
        name: "memberOf",
        attrs: &[
            a("id", AttrKind::String),
            a("shortId", AttrKind::U24),
            a("index", AttrKind::U16),
        ],
    },
    ElementDef {
        tag: 0x18,
        name: "link",
        attrs: &[
            a("url", AttrKind::String),
            a("mimeValue", AttrKind::String),
            a("xml:lang", AttrKind::String),
            a("description", AttrKind::String),
            a("expiryTime", AttrKind::Time),
        ],
    },
    ElementDef {
        tag: 0x19,
        name: "location",
        attrs: &[],
    },
    ElementDef {
        tag: 0x1A,
        name: "shortDescription",
        attrs: LANG,
    },
    ElementDef {
        tag: 0x1B,
        name: "longDescription",
        attrs: LANG,
    },
    ElementDef {
        tag: 0x1C,
        name: "programme",
        attrs: PROGRAMME_ATTRS,
    },
    ElementDef {
        tag: 0x20,
        name: "programmeGroups",
        attrs: SCHEDULE_ATTRS,
    },
    ElementDef {
        tag: 0x21,
        name: "schedule",
        attrs: SCHEDULE_ATTRS,
    },
    ElementDef {
        tag: 0x22,
        name: "alternateSource",
        attrs: &[
            a("protocol", AttrKind::Enum(PROTOCOL)),
            a("type", AttrKind::Enum(ALTERNATE_SOURCE_TYPE)),
            a("url", AttrKind::String),
        ],
    },
    ElementDef {
        tag: 0x23,
        name: "programmeGroup",
        attrs: PROGRAMME_GROUP_ATTRS,
    },
    ElementDef {
        tag: 0x24,
        name: "scope",
        attrs: &[
            a("startTime", AttrKind::Time),
            a("stopTime", AttrKind::Time),
        ],
    },
    ElementDef {
        tag: 0x25,
        name: "serviceScope",
        attrs: BEARER_ATTRS,
    },
    // DEVIATION (x4): attribute sets per TS 102 818 semantics, see module docs.
    ElementDef {
        tag: 0x26,
        name: "ensemble",
        attrs: &[a("id", AttrKind::String), a("version", AttrKind::U16)],
    },
    ElementDef {
        tag: 0x27,
        name: "frequency",
        attrs: &[
            a("type", AttrKind::Enum(FREQUENCY_TYPE)),
            a("kHz", AttrKind::U24),
        ],
    },
    ElementDef {
        tag: 0x28,
        name: "service",
        attrs: &[
            a("version", AttrKind::U16),
            a("format", AttrKind::Enum(SERVICE_FORMAT)),
            a("", AttrKind::Unused),
            a("bitrate", AttrKind::Bitrate),
        ],
    },
    ElementDef {
        tag: 0x29,
        name: "serviceID",
        attrs: &[
            a("id", AttrKind::String),
            a("type", AttrKind::Enum(SERVICE_ID_TYPE)),
        ],
    },
    ElementDef {
        tag: 0x2A,
        name: "epgLanguage",
        attrs: LANG,
    },
    ElementDef {
        tag: 0x2B,
        name: "multimedia",
        attrs: &[
            a("mimeValue", AttrKind::String),
            a("xml:lang", AttrKind::String),
            a("url", AttrKind::String),
            a("type", AttrKind::Enum(MULTIMEDIA_TYPE)),
            a("width", AttrKind::U16),
            a("height", AttrKind::U16),
        ],
    },
    ElementDef {
        tag: 0x2C,
        name: "time",
        attrs: TIME_ATTRS,
    },
    ElementDef {
        tag: 0x2D,
        name: "bearer",
        attrs: BEARER_ATTRS,
    },
    ElementDef {
        tag: 0x2E,
        name: "programmeEvent",
        attrs: PROGRAMME_ATTRS,
    },
    ElementDef {
        tag: 0x2F,
        name: "relativeTime",
        attrs: TIME_ATTRS,
    },
    ElementDef {
        tag: 0x30,
        name: "simulcast",
        attrs: EPG_ATTRS,
    },
];

/// Special (non-element) tags.
pub(crate) const TAG_CDATA: u8 = 0x01;
pub(crate) const TAG_TOKEN_TABLE: u8 = 0x04;
pub(crate) const TAG_DEFAULT_CONTENT_ID: u8 = 0x05;
pub(crate) const TAG_DEFAULT_LANGUAGE: u8 = 0x06;

/// TV-Anytime classification scheme names by scheme number (1..=8).
pub(crate) const GENRE_SCHEMES: [&str; 8] = [
    "IntentionCS",
    "FormatCS",
    "ContentCS",
    "IntendedAudienceCS",
    "OriginationCS",
    "ContentAlertCS",
    "MediaTypeCS",
    "AtmosphereCS",
];

pub(crate) fn element_by_tag(tag: u8) -> Option<&'static ElementDef> {
    ELEMENTS.iter().find(|e| e.tag == tag)
}

pub(crate) fn element_by_name(name: &str) -> Option<&'static ElementDef> {
    ELEMENTS.iter().find(|e| e.name == name)
}
