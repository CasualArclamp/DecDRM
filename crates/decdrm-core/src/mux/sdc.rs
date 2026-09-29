//! Service Description Channel data entities (ES 201 980 §6.4).
//!
//! The SDC data field ([`crate::rx::SdcBlock::data`]) is a sequence of *data entities*.
//! Every entity has a 12-bit header — 7-bit body length, version flag, 4-bit entity type
//! — followed by a body of `4 + 8·length` bits (§6.4.3.0), so each entity occupies
//! exactly `2 + length` bytes and entities stay byte aligned. Unused space is padded
//! with `0x00` bytes, i.e. an all-zero header ends the list.
//!
//! [`parse_sdc`] splits a data field into [`SdcEntity`]s and decodes every DRM30 entity
//! type (0–14 and the type 15 extension 0); anything it cannot decode is preserved as
//! raw bytes, and it never panics on garbage. [`encode_sdc_data`] is the transmitter
//! side inverse and [`sdc_block_bits`] adds the AFS index and CRC of a whole SDC block.
//!
//! Bit layouts follow ES 201 980 V4.2.1 §6.4.3 and Dream's `CSDCReceive` /
//! `CSDCTransmit` (`SDC/SDCReceive.cpp`, `SDC/SDCTransmit.cpp`, `SDC/audioparam.cpp`).
//! Where the two differ the spec wins and the difference is noted at the field.
//!
//! Rust notes: the entity *kinds* are an `enum` whose variants carry a struct each
//! ([`EntityBody`]); `match` on it is exhaustive, so adding a variant makes the compiler
//! point at every place that must handle it. Parsing helpers return
//! `Result<T, &'static str>` — `Err` holds a short reason and the `?` operator returns
//! it early from the enclosing function.

use crate::bits::{BitReader, BitWriter, pack, unpack};
use crate::fec::crc::Crc;
use crate::fec::mlc::MscProtection;

// ---------------------------------------------------------------------------------
// Entity container
// ---------------------------------------------------------------------------------

/// One SDC data entity: the header's version flag plus the decoded body. The other
/// header fields are derived from the body: [`SdcEntity::entity_type`] and
/// [`SdcEntity::length`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdcEntity {
    /// Version flag; its meaning depends on the entity type, see [`version_mechanism`].
    pub version: bool,
    pub body: EntityBody,
}

/// How an entity type uses the version flag (§6.4.3.0, table 23).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VersionMechanism {
    /// 0 = data for the current configuration, 1 = data for the next configuration
    /// (sent while the FAC reconfiguration index is non-zero).
    Reconfiguration,
    /// The flag is inverted whenever the content of the list changes; receivers then
    /// discard all data stored for that entity type.
    List,
    /// The flag has no meaning (set to 0).
    Unique,
}

/// Version flag mechanism of an entity type (table 23).
pub fn version_mechanism(entity_type: u8) -> VersionMechanism {
    match entity_type {
        0 | 2 | 5 | 9 | 10 | 14 => VersionMechanism::Reconfiguration,
        1 | 8 | 12 => VersionMechanism::Unique,
        _ => VersionMechanism::List,
    }
}

/// Body bits of an entity that could not be (or is not) decoded: the first four body
/// bits and the `length` bytes that follow them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RawBody {
    /// The first 4 body bits (low nibble).
    pub first_nibble: u8,
    pub bytes: Vec<u8>,
}

/// Decoded body of an SDC data entity, one variant per entity type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityBody {
    /// Type 0 (§6.4.3.1).
    Multiplex(MultiplexDescription),
    /// Type 1 (§6.4.3.2).
    Label(Label),
    /// Type 2 (§6.4.3.3).
    ConditionalAccess(ConditionalAccess),
    /// Type 3 (§6.4.3.4).
    AfsMultiplex(AfsMultiplex),
    /// Type 4 (§6.4.3.5).
    AfsSchedule(AfsSchedule),
    /// Type 5 (§6.4.3.6).
    Application(ApplicationInfo),
    /// Type 6 (§6.4.3.7).
    Announcement(Announcement),
    /// Type 7 (§6.4.3.8).
    AfsRegion(AfsRegion),
    /// Type 8 (§6.4.3.9).
    TimeDate(TimeAndDate),
    /// Type 9 (§6.4.3.10).
    Audio(AudioInfo),
    /// Type 10 (§6.4.3.11). `None` is the special "transmission discontinued at the
    /// reconfiguration" entity (length 0, body `0000`).
    FacChannel(Option<FacChannelParameters>),
    /// Type 11 (§6.4.3.12).
    AfsOtherService(AfsOtherService),
    /// Type 12 (§6.4.3.13).
    LanguageCountry(LanguageCountry),
    /// Type 13 (§6.4.3.14).
    AfsDetailedRegion(AfsDetailedRegion),
    /// Type 14 (§6.4.3.15).
    PacketFec(PacketStreamFec),
    /// Type 15, extension 0 (§6.4.3.16.1).
    ServiceLinking(ServiceLinking),
    /// A valid entity this implementation does not decode (type 15 with a reserved
    /// extension type — the extension type is `raw.first_nibble`).
    Unknown { entity_type: u8, raw: RawBody },
    /// An entity whose body does not fit its type's syntax (wrong length, reserved
    /// bits set, values out of range, or truncated by the end of the data field).
    Invalid { entity_type: u8, raw: RawBody, reason: &'static str },
}

impl EntityBody {
    /// The 4-bit data entity type.
    pub fn entity_type(&self) -> u8 {
        match self {
            Self::Multiplex(_) => 0,
            Self::Label(_) => 1,
            Self::ConditionalAccess(_) => 2,
            Self::AfsMultiplex(_) => 3,
            Self::AfsSchedule(_) => 4,
            Self::Application(_) => 5,
            Self::Announcement(_) => 6,
            Self::AfsRegion(_) => 7,
            Self::TimeDate(_) => 8,
            Self::Audio(_) => 9,
            Self::FacChannel(_) => 10,
            Self::AfsOtherService(_) => 11,
            Self::LanguageCountry(_) => 12,
            Self::AfsDetailedRegion(_) => 13,
            Self::PacketFec(_) => 14,
            Self::ServiceLinking(_) => 15,
            Self::Unknown { entity_type, .. } | Self::Invalid { entity_type, .. } => *entity_type & 0x0F,
        }
    }
}

/// Errors of the SDC encoder.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SdcError {
    #[error("entity body of {0} bytes exceeds the 127-byte limit")]
    BodyTooLong(usize),
    #[error("invalid entity content: {0}")]
    InvalidContent(&'static str),
}

impl SdcEntity {
    pub fn new(version: bool, body: EntityBody) -> Self {
        Self { version, body }
    }

    /// The 4-bit data entity type.
    pub fn entity_type(&self) -> u8 {
        self.body.entity_type()
    }

    /// The header's "length of body" field: body bytes after the first 4 body bits.
    /// For an entity that cannot be encoded this returns the raw length or 0.
    pub fn length(&self) -> usize {
        match &self.body {
            EntityBody::Unknown { raw, .. } | EntityBody::Invalid { raw, .. } => raw.bytes.len(),
            body => {
                let mut w = BitWriter::new();
                match encode_body(body, &mut w) {
                    Ok(()) => w.len().saturating_sub(4) / 8,
                    Err(_) => 0,
                }
            }
        }
    }

    /// Encode header and body; the result is `2 + length` bytes.
    pub fn encode(&self) -> Result<Vec<u8>, SdcError> {
        let mut body = BitWriter::new();
        encode_body(&self.body, &mut body)?;
        let bits = body.into_bits();
        // Every body is 4 + 8n bits by construction of the encoders below.
        debug_assert!(bits.len() >= 4 && (bits.len() - 4).is_multiple_of(8));
        let len = bits.len().saturating_sub(4) / 8;
        if len > 127 {
            return Err(SdcError::BodyTooLong(len));
        }
        let mut w = BitWriter::new();
        w.write(len as u32, 7);
        w.write(u32::from(self.version), 1);
        w.write(u32::from(self.entity_type()), 4);
        let mut all = w.into_bits();
        all.extend_from_slice(&bits);
        Ok(pack(&all))
    }
}

// ---------------------------------------------------------------------------------
// Parsing and encoding of whole data fields
// ---------------------------------------------------------------------------------

/// Parse an SDC data field (the bytes between the AFS index and the CRC) into data
/// entities. Stops at the zero padding or at the end of the data. Entities that do not
/// match their type's syntax are returned as [`EntityBody::Invalid`]; an entity cut
/// off by the end of the data is returned as `Invalid` too and ends the list.
///
/// Unlike Dream (which aborts the whole block at the first bad entity) the remaining
/// entities are still decoded, since the length field tells where the next one starts.
pub fn parse_sdc(data: &[u8]) -> Vec<SdcEntity> {
    let mut out = Vec::new();
    let mut p = 0;
    while p + 2 <= data.len() {
        let length = usize::from(data[p] >> 1);
        let version = data[p] & 1 == 1;
        let entity_type = data[p + 1] >> 4;
        let first_nibble = data[p + 1] & 0x0F;
        if length == 0 && !version && entity_type == 0 {
            // Padding (0x00 bytes). A genuine type 0 entity always has a body.
            break;
        }
        let end = p + 2 + length;
        if end > data.len() {
            let raw = RawBody { first_nibble, bytes: data[p + 2..].to_vec() };
            out.push(SdcEntity::new(version, EntityBody::Invalid { entity_type, raw, reason: "truncated" }));
            break;
        }
        let raw = RawBody { first_nibble, bytes: data[p + 2..end].to_vec() };
        out.push(SdcEntity::new(version, parse_body(entity_type, raw)));
        p = end;
    }
    out
}

/// Result of [`encode_sdc_data`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedSdcData {
    /// The data field, zero padded to the requested capacity.
    pub data: Vec<u8>,
    /// Indices (into the input slice) of the entities that were written.
    pub written: Vec<usize>,
    /// Indices of the entities that did not fit (they can go into a later block).
    pub skipped: Vec<usize>,
}

/// Build an SDC data field of `capacity` bytes (table 21; see [`sdc_data_capacity`])
/// from `entities`, in order. An entity that does not fit in the remaining space is
/// skipped (and reported) and the following ones are still tried — the same policy as
/// Dream's `CSDCTransmit::CommitFlush`, except that an exact fit is allowed. The rest
/// of the field is padded with `0x00`.
pub fn encode_sdc_data(entities: &[SdcEntity], capacity: usize) -> Result<EncodedSdcData, SdcError> {
    let mut data = Vec::with_capacity(capacity);
    let mut written = Vec::new();
    let mut skipped = Vec::new();
    for (i, e) in entities.iter().enumerate() {
        let bytes = e.encode()?;
        if data.len() + bytes.len() <= capacity {
            data.extend_from_slice(&bytes);
            written.push(i);
        } else {
            skipped.push(i);
        }
    }
    data.resize(capacity, 0);
    Ok(EncodedSdcData { data, written, skipped })
}

/// Size in bytes of the SDC data field for an SDC block of `sdc_bits` information bits
/// per super frame (`MlcParams::sdc(..).total_bits()`): the block carries a 4-bit AFS
/// index and a 16-bit CRC, the rest is filled with whole bytes (§6.4.2, table 21).
pub fn sdc_data_capacity(sdc_bits: usize) -> usize {
    sdc_bits.saturating_sub(20) / 8
}

/// Assemble a complete SDC block as bits (one per byte, the input of the SDC MLC
/// encoder): AFS index, data field, CRC-16 over the AFS index (as a byte with four
/// leading zeros) and the data, then zero padding up to `total_bits` (§6.4.2).
pub fn sdc_block_bits(afs_index: u8, data: &[u8], total_bits: usize) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write(u32::from(afs_index & 0x0F), 4);
    w.write_bytes(data);
    let mut crc = Crc::crc16();
    crc.add_byte(afs_index & 0x0F);
    crc.add_bytes(data);
    w.write(crc.value(), 16);
    let mut bits = w.into_bits();
    if bits.len() < total_bits {
        bits.resize(total_bits, 0);
    }
    bits
}

// ---------------------------------------------------------------------------------
// Entity structs
// ---------------------------------------------------------------------------------

/// Raw 24-bit stream description of the multiplex description entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamDescription {
    /// "Data length for part A" field (12 bits). With hierarchical modulation, stream
    /// 0's field instead carries the hierarchical protection level (2 msbs) and 10 rfu
    /// bits; use [`MultiplexDescription::streams`] for the interpreted lengths.
    pub len_a: u16,
    /// "Data length for part B" field (12 bits).
    pub len_b: u16,
}

/// Lengths of one stream's logical frame, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamLengths {
    /// Bytes carried in the higher protected part A of the multiplex frame.
    pub part_a: usize,
    /// Bytes carried in the lower protected part B (or, for the hierarchical stream,
    /// in the very strongly protected hierarchical frame).
    pub part_b: usize,
}

impl StreamLengths {
    pub fn total(&self) -> usize {
        self.part_a + self.part_b
    }
}

/// Multiplex description data entity — type 0 (§6.4.3.1): protection levels and the
/// stream lengths of the MSC.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MultiplexDescription {
    /// Protection level of part A (0..=3).
    pub protection_a: u8,
    /// Protection level of part B (0..=3).
    pub protection_b: u8,
    /// Stream descriptions for streams 0.. (1 to 4), as transmitted.
    pub streams: Vec<StreamDescription>,
}

impl MultiplexDescription {
    /// Description of a non-hierarchical multiplex.
    pub fn new(protection_a: u8, protection_b: u8, streams: &[StreamLengths]) -> Self {
        Self {
            protection_a,
            protection_b,
            streams: streams
                .iter()
                .map(|s| StreamDescription { len_a: s.part_a as u16, len_b: s.part_b as u16 })
                .collect(),
        }
    }

    /// Description of a hierarchical multiplex (64-QAM HMsym/HMmix): stream 0 is the
    /// hierarchical stream of `hierarchical_len` bytes at `protection_hier`, followed
    /// by the regular `streams` (streams 1..).
    pub fn new_hierarchical(
        protection_a: u8,
        protection_b: u8,
        protection_hier: u8,
        hierarchical_len: usize,
        streams: &[StreamLengths],
    ) -> Self {
        let mut d = Self::new(protection_a, protection_b, streams);
        d.streams.insert(
            0,
            StreamDescription { len_a: u16::from(protection_hier & 3) << 10, len_b: hierarchical_len as u16 },
        );
        d
    }

    /// Number of streams.
    pub fn num_streams(&self) -> usize {
        self.streams.len()
    }

    /// Interpreted stream lengths. With `hierarchical` modulation, stream 0 is the
    /// hierarchical stream: its part A length is 0 and its part B length is the length
    /// of the hierarchical frame (Dream `CSDCReceive::DataEntityType0`).
    pub fn streams(&self, hierarchical: bool) -> Vec<StreamLengths> {
        self.streams
            .iter()
            .enumerate()
            .map(|(i, s)| {
                if hierarchical && i == 0 {
                    StreamLengths { part_a: 0, part_b: usize::from(s.len_b) }
                } else {
                    StreamLengths { part_a: usize::from(s.len_a), part_b: usize::from(s.len_b) }
                }
            })
            .collect()
    }

    /// Protection level of the hierarchical frame (stream 0's two msbs); only meaningful
    /// with hierarchical modulation.
    pub fn hierarchical_protection(&self) -> u8 {
        self.streams.first().map_or(0, |s| (s.len_a >> 10) as u8 & 3)
    }

    /// With hierarchical modulation, stream 0's 10 rfu bits must be zero (Dream rejects
    /// the entity otherwise).
    pub fn hierarchical_rfu_ok(&self) -> bool {
        self.streams.first().is_none_or(|s| s.len_a & 0x3FF == 0)
    }

    /// MLC protection parameters.
    pub fn protection(&self, hierarchical: bool) -> MscProtection {
        MscProtection {
            part_a: usize::from(self.protection_a),
            part_b: usize::from(self.protection_b),
            hierarchical: if hierarchical { usize::from(self.hierarchical_protection()) } else { 0 },
        }
    }

    /// Total length of part A over all streams (bytes) — the `X` of the MLC's N₁
    /// formula (§7.2.1.1).
    pub fn part_a_bytes(&self, hierarchical: bool) -> usize {
        self.streams(hierarchical).iter().map(|s| s.part_a).sum()
    }
}

/// Label data entity — type 1 (§6.4.3.2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Label {
    pub short_id: u8,
    /// Label bytes (UTF-8, up to 64 bytes / 16 characters), possibly preceded by a
    /// text control byte (0x01..=0x0F, §6.7.3.1).
    pub bytes: Vec<u8>,
}

impl Label {
    pub fn new(short_id: u8, text: &str) -> Self {
        Self { short_id, bytes: text.as_bytes().to_vec() }
    }

    /// The text control field (§6.7.2) if the label carries one.
    pub fn text_control(&self) -> Option<u8> {
        match self.bytes.first() {
            Some(&b) if (1..=0x0F).contains(&b) => Some(b),
            _ => None,
        }
    }

    /// The label text (lossy UTF-8, without the text control byte and trailing
    /// NUL/space padding some encoders add).
    pub fn text(&self) -> String {
        let start = usize::from(self.text_control().is_some());
        let s = String::from_utf8_lossy(&self.bytes[start..]);
        s.trim_end_matches(['\0', ' ']).to_string()
    }
}

/// Conditional access parameters data entity — type 2 (§6.4.3.3).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConditionalAccess {
    pub short_id: u8,
    /// The parameters refer to the audio stream.
    pub audio_ca: bool,
    /// The parameters refer to the data stream(s).
    pub data_ca: bool,
    /// CA system specific information.
    pub system_data: Vec<u8>,
}

/// Region/Schedule field of the AFS entities (types 3 and 11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegionSchedule {
    /// Region Id (0 = unspecified, 1..=15 defined by entities 7/13).
    pub region_id: u8,
    /// Schedule Id (0 = unspecified, 1..=15 defined by entity 4).
    pub schedule_id: u8,
}

/// A frequency field of type 3 (and of type 11 for DRM services): multiplier flag plus
/// 15-bit value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DrmFrequency {
    /// Multiplier 10 (robustness mode E) instead of 1. Dream treats this bit as rfu and
    /// rejects the whole entity when it is set; ES 201 980 V4 defines it.
    pub times_ten: bool,
    /// Frequency value (15 bits).
    pub value: u16,
}

impl DrmFrequency {
    pub fn from_khz(khz: u32) -> Self {
        if khz <= 0x7FFF {
            Self { times_ten: false, value: khz as u16 }
        } else {
            Self { times_ten: true, value: (khz / 10).min(0x7FFF) as u16 }
        }
    }

    pub fn khz(&self) -> u32 {
        u32::from(self.value) * if self.times_ten { 10 } else { 1 }
    }
}

/// AFS: multiple frequency network information data entity — type 3 (§6.4.3.4).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AfsMultiplex {
    /// The multiplex is broadcast synchronously (identical content and timing).
    pub synchronous: bool,
    /// The frequencies apply to the enhancement layer (Dream ignores these).
    pub enhancement_layer: bool,
    /// Service restriction: bit *n* set = Short Id *n* is available on the frequencies
    /// (msb = Short Id 3). `None` = all services.
    pub short_id_flags: Option<u8>,
    pub region_schedule: Option<RegionSchedule>,
    pub frequencies: Vec<DrmFrequency>,
}

/// AFS: schedule definition data entity — type 4 (§6.4.3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AfsSchedule {
    /// 1..=15.
    pub schedule_id: u8,
    /// Days the schedule applies to: msb (bit 6) = Monday … lsb = Sunday.
    pub day_code: u8,
    /// Start time in minutes since midnight UTC (0..=1439).
    pub start_minute: u16,
    /// Duration in minutes (1..=16383).
    pub duration_minutes: u16,
}

/// Application information data entity — type 5 (§6.4.3.6). The fields map 1:1 onto
/// `decdrm_data::DataServiceConfig` (whose total packet length is
/// `packet_length + 3`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApplicationInfo {
    pub short_id: u8,
    /// Stream carrying the data service / application.
    pub stream_id: u8,
    /// Packet mode (`true`) or synchronous stream mode.
    pub packet_mode: bool,
    /// Packet mode: data units span several packets (`true`) or single packets.
    pub data_unit_indicator: bool,
    /// Packet mode: packet Id 0..=3.
    pub packet_id: u8,
    /// Enhancement data available in another channel.
    pub enhancement: bool,
    /// Application domain (3 bits): 0 = DRM, 1 = DAB (TS 101 968).
    pub app_domain: u8,
    /// Packet mode: length of each packet's *data field* in bytes (1..=255).
    pub packet_length: u8,
    /// The "application data" field, raw.
    pub application_data: Vec<u8>,
}

impl ApplicationInfo {
    /// User application identifier from the first two application-data bytes: for the
    /// DAB domain `rfa(5) + user application type(11)`; for the DRM domain the 16-bit
    /// value Dream reads (identical when the top bits are zero — TS 101 968 is not
    /// available here to confirm the exact DRM-domain layout).
    pub fn user_app_id(&self) -> Option<u16> {
        let v = u16::from_be_bytes([*self.application_data.first()?, *self.application_data.get(1)?]);
        Some(if self.app_domain == 1 { v & 0x07FF } else { v })
    }

    /// Application data after the 2-byte user application identifier.
    pub fn user_app_data(&self) -> &[u8] {
        self.application_data.get(2..).unwrap_or(&[])
    }
}

/// Announcement support and switching data entity — type 6 (§6.4.3.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Announcement {
    /// Services of the tuned multiplex the definition applies to (bit n = Short Id n).
    pub short_id_flags: u8,
    /// Announcements are carried elsewhere (then `id` is an Announcement Id linking to
    /// type 11 entities) rather than in the tuned multiplex (then `id` is a Short Id).
    pub other_service: bool,
    /// Short Id or Announcement Id.
    pub id: u8,
    /// Announcement types provided (10 bits; b0 travel, b1 news flash, b2 weather
    /// flash, b3 warning/alarm, b4 warning/alarm test).
    pub support_flags: u16,
    /// Announcement types currently active (same bit assignment).
    pub switching_flags: u16,
}

/// AFS: region definition data entity — type 7 (§6.4.3.8).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AfsRegion {
    /// 1..=15.
    pub region_id: u8,
    /// Southerly latitude in degrees (-90..=90).
    pub latitude: i16,
    /// Westerly longitude in degrees (-180..=179).
    pub longitude: i16,
    /// Extent to the north in degrees.
    pub latitude_extent: u8,
    /// Extent to the east in degrees (may wrap past +179).
    pub longitude_extent: u8,
    /// CIRAF zones 1..=85.
    pub ciraf_zones: Vec<u8>,
}

/// Local time offset of the time and date entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LocalTimeOffset {
    /// rfu bits (2); non-zero means the offset fields are to be ignored.
    pub rfu: u8,
    /// Local time is behind UTC.
    pub negative: bool,
    /// Offset in half hours (0..=31).
    pub half_hours: u8,
}

impl LocalTimeOffset {
    /// Offset in minutes, `None` when the rfu bits are set.
    pub fn minutes(&self) -> Option<i32> {
        if self.rfu != 0 {
            return None;
        }
        let m = i32::from(self.half_hours) * 30;
        Some(if self.negative { -m } else { m })
    }

    pub fn from_minutes(minutes: i32) -> Self {
        Self { rfu: 0, negative: minutes < 0, half_hours: ((minutes.abs() + 15) / 30).min(31) as u8 }
    }
}

/// Time and date information data entity — type 8 (§6.4.3.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimeAndDate {
    /// Modified Julian Date (17 bits).
    pub mjd: u32,
    /// UTC hours (5 bits) and minutes (6 bits).
    pub hour: u8,
    pub minute: u8,
    /// Present when the entity has the optional fourth byte.
    pub local_offset: Option<LocalTimeOffset>,
}

impl TimeAndDate {
    /// Build from a UTC civil date and time.
    pub fn from_utc(year: i32, month: u8, day: u8, hour: u8, minute: u8) -> Self {
        Self { mjd: mjd_from_civil(year, month, day), hour, minute, local_offset: None }
    }

    /// Civil UTC date `(year, month, day)`.
    pub fn date(&self) -> (i32, u8, u8) {
        civil_from_mjd(self.mjd)
    }

    /// Local time offset in minutes, if signalled and valid.
    pub fn local_offset_minutes(&self) -> Option<i32> {
        self.local_offset.and_then(|o| o.minutes())
    }
}

/// Modified Julian Date of a proleptic Gregorian date (MJD 0 = 1858-11-17).
pub fn mjd_from_civil(year: i32, month: u8, day: u8) -> u32 {
    // Howard Hinnant's days_from_civil; 1970-01-01 is MJD 40587.
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (days + 40_587).max(0) as u32
}

/// Proleptic Gregorian date `(year, month, day)` of a Modified Julian Date.
pub fn civil_from_mjd(mjd: u32) -> (i32, u8, u8) {
    let z = i64::from(mjd) - 40_587 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
    (year, month, day)
}

/// Audio information data entity — type 9 (§6.4.3.10), fields as transmitted. See
/// [`crate::mux::service::AudioParams`] for the interpretation (and Dream's quirks).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AudioInfo {
    pub short_id: u8,
    pub stream_id: u8,
    /// Audio coding (2 bits): 0 AAC, 1 reserved (Opus in Dream), 2 reserved, 3 xHE-AAC.
    pub coding: u8,
    /// SBR flag (AAC; rfa otherwise).
    pub sbr: bool,
    /// Audio mode (2 bits): 0 mono, 1 parametric stereo (AAC), 2 stereo.
    pub mode: u8,
    /// Audio sampling rate code (3 bits; meaning depends on `coding`).
    pub sample_rate: u8,
    /// A text message is carried in the last 4 bytes of the stream.
    pub text: bool,
    pub enhancement: bool,
    /// Coder field (5 bits): for AAC/xHE-AAC the MPEG Surround mode in the 3 msbs.
    pub coder_field: u8,
    /// The final rfa bit.
    pub rfa: bool,
    /// Codec specific config (xHE-AAC static config; empty for AAC).
    pub codec_config: Vec<u8>,
}

/// FAC channel parameters data entity — type 10 (§6.4.3.11): the channel parameters
/// of the next configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FacChannelParameters {
    /// Base/Enhancement flag.
    pub enhancement: bool,
    /// Robustness mode (2 bits): 0..=3 = A..D (RM flag 0), 0 = E (RM flag 1).
    pub robustness_mode: u8,
    pub rm_flag: bool,
    /// Spectrum occupancy (3 bits).
    pub spectrum_occupancy: u8,
    /// Interleaver depth flag: `true` = short (400 ms).
    pub short_interleaving: bool,
    /// MSC mode (2 bits, as in the FAC).
    pub msc_mode: u8,
    /// SDC mode (1 bit, as in the FAC).
    pub sdc_mode: u8,
    /// Number of services code (4 bits, as in the FAC).
    pub num_services: u8,
    /// rfa (4 bits).
    pub rfa: u8,
}

/// AFS: other services data entity — type 11 (§6.4.3.12).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AfsOtherService {
    /// `id` is an Announcement Id (else a Short Id of the tuned multiplex).
    pub announcement: bool,
    /// Short Id or Announcement Id.
    pub id: u8,
    /// The other service is the "same service" (else an alternative one).
    pub same_service: bool,
    /// System Id (5 bits), see [`other_service_id_bits`].
    pub system_id: u8,
    pub region_schedule: Option<RegionSchedule>,
    /// Other Service Id, present for the system ids that define one.
    pub other_service_id: Option<u32>,
    /// Frequency fields, 16 bits for DRM/AM (systems 0–2) and 8 bits (channel codes)
    /// for FM and DAB. See [`AfsOtherService::frequency`].
    pub frequencies: Vec<u16>,
}

/// Length in bits of the Other Service Id of a type 11 System Id (table 22a/§6.4.3.12).
/// Reserved system ids carry no id (Dream's behaviour).
pub fn other_service_id_bits(system_id: u8) -> u32 {
    match system_id {
        0 | 1 | 3 | 6 | 9 => 24,
        4 | 7 | 10 => 16,
        11 => 32,
        _ => 0,
    }
}

/// Length in bytes of a type 11 frequency field.
pub fn other_service_frequency_bytes(system_id: u8) -> usize {
    if system_id <= 2 { 2 } else { 1 }
}

/// Decoded frequency of a type 11 entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtherFrequency {
    /// DRM or AM frequency in kHz.
    Khz(u32),
    /// FM frequency in kHz (100 kHz raster).
    FmKhz(u32),
    /// DAB channel code (64..=101 = 5A..13F).
    DabChannel(u8),
    /// Anything else (reserved system ids or out-of-range codes).
    Raw(u16),
}

impl AfsOtherService {
    /// Decoded value of frequency field `i`.
    pub fn frequency(&self, i: usize) -> Option<OtherFrequency> {
        let v = *self.frequencies.get(i)?;
        Some(match self.system_id {
            0 => OtherFrequency::Khz(DrmFrequency { times_ten: v & 0x8000 != 0, value: v & 0x7FFF }.khz()),
            1 | 2 => OtherFrequency::Khz(u32::from(v & 0x7FFF)),
            3..=5 if v <= 204 => OtherFrequency::FmKhz(87_500 + 100 * u32::from(v)),
            6..=8 if v <= 140 => OtherFrequency::FmKhz(76_000 + 100 * u32::from(v)),
            9..=11 => OtherFrequency::DabChannel(v as u8),
            _ => OtherFrequency::Raw(v),
        })
    }
}

/// Language and country data entity — type 12 (§6.4.3.13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LanguageCountry {
    pub short_id: u8,
    /// ISO 639-2 language code (three lower case ISO 8859-1 characters, "---" if
    /// unspecified).
    pub language: [u8; 3],
    /// ISO 3166 country code (two lower case characters, "--" if unspecified).
    pub country: [u8; 2],
}

impl LanguageCountry {
    pub fn language_str(&self) -> String {
        latin1(&self.language)
    }

    pub fn country_str(&self) -> String {
        latin1(&self.country)
    }
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// One square of the detailed region definition (units of 1/16 degree).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegionSquare {
    /// Southerly latitude, 1/16 degrees (12-bit two's complement).
    pub latitude: i16,
    /// Westerly longitude, 1/16 degrees (13-bit two's complement).
    pub longitude: i16,
    /// Extent to the north, 1/16 degrees (11 bits).
    pub latitude_extent: u16,
    /// Extent to the east, 1/16 degrees (11 bits).
    pub longitude_extent: u16,
}

/// AFS: detailed region definition data entity — type 13 (§6.4.3.14).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AfsDetailedRegion {
    /// 1..=15, shared with type 7.
    pub region_id: u8,
    /// 1..=16 squares.
    pub squares: Vec<RegionSquare>,
}

/// Packet stream FEC parameters data entity — type 14 (§6.4.3.15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PacketStreamFec {
    pub stream_id: u8,
    /// R parameter (1..=180).
    pub r: u8,
    /// C parameter (1..=239).
    pub c: u8,
    /// Packet data field length (1..=255).
    pub packet_length: u8,
}

/// Id list of the service linking entity.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LinkIdList {
    /// Identifier List Qualifier (2 bits): 00 DAB, 01 RDS, 11 DRM/AMSS.
    pub qualifier: u8,
    /// Each Id is a DAB data service.
    pub data: bool,
    /// Identifiers (16, 24 or 32 bits each, see [`link_id_bits`]).
    pub ids: Vec<u32>,
}

/// Service linking information data entity — type 15 extension 0 (§6.4.3.16.1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServiceLinking {
    /// Linkage Actuator: the link is active.
    pub active: bool,
    /// Hard link (same content) rather than soft link.
    pub hard: bool,
    /// International linkage set (ILS indicator).
    pub international: bool,
    /// Linkage Set Number (12 bits).
    pub lsn: u16,
    pub id_list: Option<LinkIdList>,
}

/// Bits per Id of the service linking Id list (table 22b).
pub fn link_id_bits(international: bool, data: bool) -> u32 {
    if data {
        32
    } else if international {
        24
    } else {
        16
    }
}

// ---------------------------------------------------------------------------------
// Body parsing
// ---------------------------------------------------------------------------------

/// Reader over the body bits of one entity (4 + 8·len bits).
struct Body<'a> {
    r: BitReader<'a>,
    len: usize,
}

impl Body<'_> {
    fn u(&mut self, n: u32) -> u32 {
        self.r.read(n)
    }

    fn u8(&mut self, n: u32) -> u8 {
        self.r.read(n) as u8
    }

    fn flag(&mut self) -> bool {
        self.r.read_bool()
    }

    /// `n`-bit two's complement number.
    fn signed(&mut self, n: u32) -> i16 {
        let v = self.r.read(n) as i32;
        (if v & (1 << (n - 1)) != 0 { v - (1 << n) } else { v }) as i16
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.r.read_byte()).collect()
    }

    /// Whole bytes left in the body.
    fn remaining_bytes(&self) -> usize {
        self.r.remaining() / 8
    }

    fn rest(&mut self) -> Vec<u8> {
        let n = self.remaining_bytes();
        self.bytes(n)
    }

    fn done(&self) -> Result<(), &'static str> {
        if self.r.overrun {
            Err("body too short")
        } else if self.r.remaining() != 0 {
            Err("unexpected trailing body bytes")
        } else {
            Ok(())
        }
    }
}

fn parse_body(entity_type: u8, raw: RawBody) -> EntityBody {
    let mut bits = Vec::with_capacity(4 + 8 * raw.bytes.len());
    for i in (0..4).rev() {
        bits.push((raw.first_nibble >> i) & 1);
    }
    bits.extend(unpack(&raw.bytes));
    let mut b = Body { r: BitReader::from_bits(&bits), len: raw.bytes.len() };
    let parsed = match entity_type {
        0 => parse_multiplex(&mut b).map(EntityBody::Multiplex),
        1 => parse_label(&mut b).map(EntityBody::Label),
        2 => parse_ca(&mut b).map(EntityBody::ConditionalAccess),
        3 => parse_afs_multiplex(&mut b).map(EntityBody::AfsMultiplex),
        4 => parse_schedule(&mut b).map(EntityBody::AfsSchedule),
        5 => parse_application(&mut b).map(EntityBody::Application),
        6 => parse_announcement(&mut b).map(EntityBody::Announcement),
        7 => parse_region(&mut b).map(EntityBody::AfsRegion),
        8 => parse_time(&mut b).map(EntityBody::TimeDate),
        9 => parse_audio(&mut b).map(EntityBody::Audio),
        10 => parse_fac_channel(&mut b).map(EntityBody::FacChannel),
        11 => parse_other_service(&mut b).map(EntityBody::AfsOtherService),
        12 => parse_language(&mut b).map(EntityBody::LanguageCountry),
        13 => parse_detailed_region(&mut b).map(EntityBody::AfsDetailedRegion),
        14 => parse_packet_fec(&mut b).map(EntityBody::PacketFec),
        _ => {
            if raw.first_nibble == 0 {
                parse_service_linking(&mut b).map(EntityBody::ServiceLinking)
            } else {
                return EntityBody::Unknown { entity_type, raw };
            }
        }
    };
    match parsed {
        Ok(body) => body,
        Err(reason) => EntityBody::Invalid { entity_type, raw, reason },
    }
}

fn parse_multiplex(b: &mut Body) -> Result<MultiplexDescription, &'static str> {
    if b.len == 0 || !b.len.is_multiple_of(3) || b.len / 3 > 4 {
        return Err("multiplex description needs 1 to 4 stream descriptions");
    }
    let protection_a = b.u8(2);
    let protection_b = b.u8(2);
    let streams = (0..b.len / 3)
        .map(|_| StreamDescription { len_a: b.u(12) as u16, len_b: b.u(12) as u16 })
        .collect();
    b.done()?;
    Ok(MultiplexDescription { protection_a, protection_b, streams })
}

fn parse_label(b: &mut Body) -> Result<Label, &'static str> {
    if b.len == 0 || b.len > 64 {
        return Err("label must have 1 to 64 bytes");
    }
    let short_id = b.u8(2);
    if b.u(2) != 0 {
        return Err("label rfu bits set");
    }
    let bytes = b.rest();
    b.done()?;
    Ok(Label { short_id, bytes })
}

fn parse_ca(b: &mut Body) -> Result<ConditionalAccess, &'static str> {
    let short_id = b.u8(2);
    let audio_ca = b.flag();
    let data_ca = b.flag();
    let system_data = b.rest();
    b.done()?;
    Ok(ConditionalAccess { short_id, audio_ca, data_ca, system_data })
}

fn parse_afs_multiplex(b: &mut Body) -> Result<AfsMultiplex, &'static str> {
    let synchronous = b.flag();
    let enhancement_layer = b.flag();
    let restricted = b.flag();
    let region_flag = b.flag();
    let mut n = b.len;
    let short_id_flags = if restricted {
        let flags = b.u8(4);
        let _rfa = b.u(4);
        n = n.checked_sub(1).ok_or("missing service restriction field")?;
        Some(flags)
    } else {
        None
    };
    let region_schedule = if region_flag {
        n = n.checked_sub(1).ok_or("missing region/schedule field")?;
        Some(RegionSchedule { region_id: b.u8(4), schedule_id: b.u8(4) })
    } else {
        None
    };
    if !n.is_multiple_of(2) {
        return Err("odd number of frequency bytes");
    }
    let frequencies = (0..n / 2).map(|_| DrmFrequency { times_ten: b.flag(), value: b.u(15) as u16 }).collect();
    b.done()?;
    Ok(AfsMultiplex { synchronous, enhancement_layer, short_id_flags, region_schedule, frequencies })
}

fn parse_schedule(b: &mut Body) -> Result<AfsSchedule, &'static str> {
    if b.len != 4 {
        return Err("schedule definition must be 4 bytes");
    }
    let s = AfsSchedule {
        schedule_id: b.u8(4),
        day_code: b.u8(7),
        start_minute: b.u(11) as u16,
        duration_minutes: b.u(14) as u16,
    };
    b.done()?;
    // Same checks as Dream's DataEntityType4.
    if s.schedule_id == 0 || s.day_code == 0 || s.start_minute > 1439 || s.duration_minutes == 0 {
        return Err("schedule field out of range");
    }
    Ok(s)
}

fn parse_application(b: &mut Body) -> Result<ApplicationInfo, &'static str> {
    let short_id = b.u8(2);
    let stream_id = b.u8(2);
    let packet_mode = b.flag();
    let mut a = ApplicationInfo { short_id, stream_id, packet_mode, ..Default::default() };
    if packet_mode {
        if b.len < 2 {
            return Err("packet mode application information too short");
        }
        a.data_unit_indicator = b.flag();
        a.packet_id = b.u8(2);
        a.enhancement = b.flag();
        a.app_domain = b.u8(3);
        a.packet_length = b.u8(8);
    } else {
        if b.len < 1 {
            return Err("application information too short");
        }
        let _rfa = b.u(3);
        a.enhancement = b.flag();
        a.app_domain = b.u8(3);
    }
    a.application_data = b.rest();
    b.done()?;
    Ok(a)
}

fn parse_announcement(b: &mut Body) -> Result<Announcement, &'static str> {
    if b.len != 3 {
        return Err("announcement entity must be 3 bytes");
    }
    let short_id_flags = b.u8(4);
    let other_service = b.flag();
    let id = b.u8(2);
    let _rfa = b.u(1);
    let support_flags = b.u(10) as u16;
    let switching_flags = b.u(10) as u16;
    b.done()?;
    Ok(Announcement { short_id_flags, other_service, id, support_flags, switching_flags })
}

fn parse_region(b: &mut Body) -> Result<AfsRegion, &'static str> {
    if b.len < 4 {
        return Err("region definition shorter than 4 bytes");
    }
    let region_id = b.u8(4);
    let latitude = b.signed(8);
    let longitude = b.signed(9);
    let latitude_extent = b.u8(7);
    let longitude_extent = b.u8(8);
    let ciraf_zones = b.rest();
    b.done()?;
    // Same checks as Dream's DataEntityType7.
    if region_id == 0
        || ciraf_zones.iter().any(|&z| z == 0 || z > 85)
        || !(-90..=90).contains(&latitude)
        || latitude + i16::from(latitude_extent) > 90
        || !(-180..=179).contains(&longitude)
    {
        return Err("region definition field out of range");
    }
    Ok(AfsRegion { region_id, latitude, longitude, latitude_extent, longitude_extent, ciraf_zones })
}

fn parse_time(b: &mut Body) -> Result<TimeAndDate, &'static str> {
    if b.len != 3 && b.len != 4 {
        return Err("time and date entity must be 3 or 4 bytes");
    }
    let mjd = b.u(17);
    let hour = b.u8(5);
    let minute = b.u8(6);
    let local_offset = if b.len == 4 {
        Some(LocalTimeOffset { rfu: b.u8(2), negative: b.flag(), half_hours: b.u8(5) })
    } else {
        None
    };
    b.done()?;
    Ok(TimeAndDate { mjd, hour, minute, local_offset })
}

fn parse_audio(b: &mut Body) -> Result<AudioInfo, &'static str> {
    if b.len < 2 {
        return Err("audio information shorter than 2 bytes");
    }
    let a = AudioInfo {
        short_id: b.u8(2),
        stream_id: b.u8(2),
        coding: b.u8(2),
        sbr: b.flag(),
        mode: b.u8(2),
        sample_rate: b.u8(3),
        text: b.flag(),
        enhancement: b.flag(),
        coder_field: b.u8(5),
        rfa: b.flag(),
        codec_config: b.rest(),
    };
    b.done()?;
    Ok(a)
}

fn parse_fac_channel(b: &mut Body) -> Result<Option<FacChannelParameters>, &'static str> {
    if b.len == 0 {
        // "Transmission discontinued": length 0 and the first four body bits 0.
        return if b.u(4) == 0 { Ok(None) } else { Err("FAC channel parameters too short") };
    }
    if b.len != 2 {
        return Err("FAC channel parameters must be 2 bytes");
    }
    let p = FacChannelParameters {
        enhancement: b.flag(),
        robustness_mode: b.u8(2),
        rm_flag: b.flag(),
        spectrum_occupancy: b.u8(3),
        short_interleaving: b.flag(),
        msc_mode: b.u8(2),
        sdc_mode: b.u8(1),
        num_services: b.u8(4),
        rfa: b.u8(4),
    };
    if b.u(1) != 0 {
        return Err("FAC channel parameters rfu bit set");
    }
    b.done()?;
    Ok(Some(p))
}

fn parse_other_service(b: &mut Body) -> Result<AfsOtherService, &'static str> {
    let announcement = b.flag();
    let id = b.u8(2);
    let region_flag = b.flag();
    let same_service = b.flag();
    let _rfa = b.u(2);
    let system_id = b.u8(5);
    let mut n = b.len.checked_sub(1).ok_or("other services entity too short")?;
    let region_schedule = if region_flag {
        n = n.checked_sub(1).ok_or("missing region/schedule field")?;
        Some(RegionSchedule { region_id: b.u8(4), schedule_id: b.u8(4) })
    } else {
        None
    };
    let id_bits = other_service_id_bits(system_id);
    let other_service_id = if id_bits > 0 {
        n = n.checked_sub(id_bits as usize / 8).ok_or("missing other service id")?;
        Some(b.u(id_bits))
    } else {
        None
    };
    let fb = other_service_frequency_bytes(system_id);
    if n % fb != 0 {
        return Err("frequency list length mismatch");
    }
    let frequencies = (0..n / fb).map(|_| b.u(8 * fb as u32) as u16).collect();
    b.done()?;
    Ok(AfsOtherService { announcement, id, same_service, system_id, region_schedule, other_service_id, frequencies })
}

fn parse_language(b: &mut Body) -> Result<LanguageCountry, &'static str> {
    if b.len != 5 {
        return Err("language and country entity must be 5 bytes");
    }
    let short_id = b.u8(2);
    if b.u(2) != 0 {
        return Err("language and country rfu bits set");
    }
    let language = [b.u8(8), b.u8(8), b.u8(8)];
    let country = [b.u8(8), b.u8(8)];
    b.done()?;
    Ok(LanguageCountry { short_id, language, country })
}

fn parse_detailed_region(b: &mut Body) -> Result<AfsDetailedRegion, &'static str> {
    if b.len == 0 || !b.len.is_multiple_of(6) {
        return Err("detailed region definition needs 6 bytes per square");
    }
    let region_id = b.u8(4);
    let mut squares = Vec::with_capacity(b.len / 6);
    for _ in 0..b.len / 6 {
        if b.u(1) != 0 {
            return Err("detailed region rfu bit set");
        }
        squares.push(RegionSquare {
            latitude: b.signed(12),
            longitude: b.signed(13),
            latitude_extent: b.u(11) as u16,
            longitude_extent: b.u(11) as u16,
        });
    }
    b.done()?;
    if region_id == 0 {
        return Err("region id 0");
    }
    Ok(AfsDetailedRegion { region_id, squares })
}

fn parse_packet_fec(b: &mut Body) -> Result<PacketStreamFec, &'static str> {
    if b.len != 3 {
        return Err("packet stream FEC entity must be 3 bytes");
    }
    let stream_id = b.u8(2);
    if b.u(2) != 0 {
        return Err("packet stream FEC rfu bits set");
    }
    let p = PacketStreamFec { stream_id, r: b.u8(8), c: b.u8(8), packet_length: b.u8(8) };
    b.done()?;
    Ok(p)
}

fn parse_service_linking(b: &mut Body) -> Result<ServiceLinking, &'static str> {
    let _extension = b.u(4);
    let has_list = b.flag();
    let active = b.flag();
    let hard = b.flag();
    let international = b.flag();
    let lsn = b.u(12) as u16;
    let id_list = if has_list {
        if b.u(1) != 0 {
            return Err("service linking rfu bit set");
        }
        let qualifier = b.u8(2);
        let data = b.flag();
        let count = b.u(4) as usize;
        let bits = link_id_bits(international, data);
        if b.r.remaining() != count * bits as usize {
            return Err("service linking id list length mismatch");
        }
        let ids = (0..count).map(|_| b.u(bits)).collect();
        Some(LinkIdList { qualifier, data, ids })
    } else {
        None
    };
    b.done()?;
    Ok(ServiceLinking { active, hard, international, lsn, id_list })
}

// ---------------------------------------------------------------------------------
// Body encoding
// ---------------------------------------------------------------------------------

fn bits(w: &mut BitWriter, v: impl Into<u32>, n: u32) {
    let v: u32 = v.into();
    w.write(v & if n >= 32 { u32::MAX } else { (1u32 << n) - 1 }, n);
}

fn signed(w: &mut BitWriter, v: i16, n: u32) {
    w.write((i32::from(v) as u32) & ((1u32 << n) - 1), n);
}

fn encode_body(body: &EntityBody, w: &mut BitWriter) -> Result<(), SdcError> {
    match body {
        EntityBody::Multiplex(m) => {
            if m.streams.is_empty() || m.streams.len() > 4 {
                return Err(SdcError::InvalidContent("multiplex description needs 1 to 4 streams"));
            }
            bits(w, m.protection_a, 2);
            bits(w, m.protection_b, 2);
            for s in &m.streams {
                bits(w, s.len_a, 12);
                bits(w, s.len_b, 12);
            }
        }
        EntityBody::Label(l) => {
            if l.bytes.is_empty() || l.bytes.len() > 64 {
                return Err(SdcError::InvalidContent("label must have 1 to 64 bytes"));
            }
            bits(w, l.short_id, 2);
            bits(w, 0u8, 2);
            w.write_bytes(&l.bytes);
        }
        EntityBody::ConditionalAccess(c) => {
            bits(w, c.short_id, 2);
            bits(w, c.audio_ca, 1);
            bits(w, c.data_ca, 1);
            w.write_bytes(&c.system_data);
        }
        EntityBody::AfsMultiplex(a) => {
            bits(w, a.synchronous, 1);
            bits(w, a.enhancement_layer, 1);
            bits(w, a.short_id_flags.is_some(), 1);
            bits(w, a.region_schedule.is_some(), 1);
            if let Some(f) = a.short_id_flags {
                bits(w, f, 4);
                bits(w, 0u8, 4);
            }
            if let Some(rs) = a.region_schedule {
                bits(w, rs.region_id, 4);
                bits(w, rs.schedule_id, 4);
            }
            for f in &a.frequencies {
                bits(w, f.times_ten, 1);
                bits(w, f.value, 15);
            }
        }
        EntityBody::AfsSchedule(s) => {
            bits(w, s.schedule_id, 4);
            bits(w, s.day_code, 7);
            bits(w, s.start_minute, 11);
            bits(w, s.duration_minutes, 14);
        }
        EntityBody::Application(a) => {
            bits(w, a.short_id, 2);
            bits(w, a.stream_id, 2);
            bits(w, a.packet_mode, 1);
            if a.packet_mode {
                bits(w, a.data_unit_indicator, 1);
                bits(w, a.packet_id, 2);
                bits(w, a.enhancement, 1);
                bits(w, a.app_domain, 3);
                bits(w, a.packet_length, 8);
            } else {
                bits(w, 0u8, 3);
                bits(w, a.enhancement, 1);
                bits(w, a.app_domain, 3);
            }
            w.write_bytes(&a.application_data);
        }
        EntityBody::Announcement(a) => {
            bits(w, a.short_id_flags, 4);
            bits(w, a.other_service, 1);
            bits(w, a.id, 2);
            bits(w, 0u8, 1);
            bits(w, a.support_flags, 10);
            bits(w, a.switching_flags, 10);
        }
        EntityBody::AfsRegion(r) => {
            bits(w, r.region_id, 4);
            signed(w, r.latitude, 8);
            signed(w, r.longitude, 9);
            bits(w, r.latitude_extent, 7);
            bits(w, r.longitude_extent, 8);
            w.write_bytes(&r.ciraf_zones);
        }
        EntityBody::TimeDate(t) => {
            bits(w, t.mjd, 17);
            bits(w, t.hour, 5);
            bits(w, t.minute, 6);
            if let Some(o) = t.local_offset {
                bits(w, o.rfu, 2);
                bits(w, o.negative, 1);
                bits(w, o.half_hours, 5);
            }
        }
        EntityBody::Audio(a) => {
            bits(w, a.short_id, 2);
            bits(w, a.stream_id, 2);
            bits(w, a.coding, 2);
            bits(w, a.sbr, 1);
            bits(w, a.mode, 2);
            bits(w, a.sample_rate, 3);
            bits(w, a.text, 1);
            bits(w, a.enhancement, 1);
            bits(w, a.coder_field, 5);
            bits(w, a.rfa, 1);
            w.write_bytes(&a.codec_config);
        }
        EntityBody::FacChannel(None) => bits(w, 0u8, 4),
        EntityBody::FacChannel(Some(p)) => {
            bits(w, p.enhancement, 1);
            bits(w, p.robustness_mode, 2);
            bits(w, p.rm_flag, 1);
            bits(w, p.spectrum_occupancy, 3);
            bits(w, p.short_interleaving, 1);
            bits(w, p.msc_mode, 2);
            bits(w, p.sdc_mode, 1);
            bits(w, p.num_services, 4);
            bits(w, p.rfa, 4);
            bits(w, 0u8, 1);
        }
        EntityBody::AfsOtherService(o) => {
            bits(w, o.announcement, 1);
            bits(w, o.id, 2);
            bits(w, o.region_schedule.is_some(), 1);
            bits(w, o.same_service, 1);
            bits(w, 0u8, 2);
            bits(w, o.system_id, 5);
            if let Some(rs) = o.region_schedule {
                bits(w, rs.region_id, 4);
                bits(w, rs.schedule_id, 4);
            }
            let id_bits = other_service_id_bits(o.system_id);
            if id_bits > 0 {
                let id = o.other_service_id.ok_or(SdcError::InvalidContent("system id requires an other service id"))?;
                bits(w, id, id_bits);
            }
            let fb = 8 * other_service_frequency_bytes(o.system_id) as u32;
            for &f in &o.frequencies {
                bits(w, f, fb);
            }
        }
        EntityBody::LanguageCountry(l) => {
            bits(w, l.short_id, 2);
            bits(w, 0u8, 2);
            w.write_bytes(&l.language);
            w.write_bytes(&l.country);
        }
        EntityBody::AfsDetailedRegion(r) => {
            if r.squares.is_empty() || r.squares.len() > 16 {
                return Err(SdcError::InvalidContent("detailed region needs 1 to 16 squares"));
            }
            bits(w, r.region_id, 4);
            for s in &r.squares {
                bits(w, 0u8, 1);
                signed(w, s.latitude, 12);
                signed(w, s.longitude, 13);
                bits(w, s.latitude_extent, 11);
                bits(w, s.longitude_extent, 11);
            }
        }
        EntityBody::PacketFec(p) => {
            bits(w, p.stream_id, 2);
            bits(w, 0u8, 2);
            bits(w, p.r, 8);
            bits(w, p.c, 8);
            bits(w, p.packet_length, 8);
        }
        EntityBody::ServiceLinking(s) => {
            bits(w, 0u8, 4); // extension type 0
            bits(w, s.id_list.is_some(), 1);
            bits(w, s.active, 1);
            bits(w, s.hard, 1);
            bits(w, s.international, 1);
            bits(w, s.lsn, 12);
            if let Some(l) = &s.id_list {
                if l.ids.len() > 15 {
                    return Err(SdcError::InvalidContent("at most 15 linked ids"));
                }
                bits(w, 0u8, 1);
                bits(w, l.qualifier, 2);
                bits(w, l.data, 1);
                bits(w, l.ids.len() as u32, 4);
                let n = link_id_bits(s.international, l.data);
                for &id in &l.ids {
                    bits(w, id, n);
                }
            }
        }
        EntityBody::Unknown { raw, .. } | EntityBody::Invalid { raw, .. } => {
            bits(w, raw.first_nibble, 4);
            w.write_bytes(&raw.bytes);
        }
    }
    // Defensive: every layout above already yields 4 + 8n bits; keep the header's
    // length field consistent even if one of them is changed incorrectly.
    let extra = (w.len().max(4) - 4) % 8;
    if extra != 0 {
        bits(w, 0u8, (8 - extra) as u32);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(version: bool, body: EntityBody) {
        let e = SdcEntity::new(version, body);
        let bytes = e.encode().unwrap_or_else(|err| panic!("encode {e:?}: {err}"));
        assert_eq!(bytes.len(), 2 + e.length(), "{e:?}");
        let parsed = parse_sdc(&bytes);
        assert_eq!(parsed, vec![e.clone()], "bytes {bytes:02x?}");
        // Also inside a padded data field together with another entity.
        let other = SdcEntity::new(false, EntityBody::Label(Label::new(2, "x")));
        let field = encode_sdc_data(&[e.clone(), other.clone()], 200).unwrap();
        assert_eq!(field.data.len(), 200);
        assert_eq!(parse_sdc(&field.data), vec![e, other]);
    }

    #[test]
    fn roundtrip_every_entity_type() {
        roundtrip(
            false,
            EntityBody::Multiplex(MultiplexDescription::new(
                1,
                2,
                &[StreamLengths { part_a: 0, part_b: 1105 }, StreamLengths { part_a: 59, part_b: 0 }],
            )),
        );
        roundtrip(
            true,
            EntityBody::Multiplex(MultiplexDescription::new_hierarchical(
                0,
                1,
                3,
                300,
                &[StreamLengths { part_a: 10, part_b: 700 }],
            )),
        );
        roundtrip(false, EntityBody::Label(Label::new(1, "Deutsche Welle")));
        roundtrip(false, EntityBody::Label(Label::new(3, "Ünïcødé ラジオ")));
        roundtrip(
            false,
            EntityBody::ConditionalAccess(ConditionalAccess {
                short_id: 2,
                audio_ca: true,
                data_ca: false,
                system_data: vec![1, 2, 3, 250],
            }),
        );
        roundtrip(
            true,
            EntityBody::AfsMultiplex(AfsMultiplex {
                synchronous: true,
                enhancement_layer: false,
                short_id_flags: Some(0b0101),
                region_schedule: Some(RegionSchedule { region_id: 3, schedule_id: 7 }),
                frequencies: vec![DrmFrequency::from_khz(6075), DrmFrequency::from_khz(15_440), DrmFrequency::from_khz(100_000)],
            }),
        );
        roundtrip(
            false,
            EntityBody::AfsMultiplex(AfsMultiplex { frequencies: vec![DrmFrequency::from_khz(1440)], ..Default::default() }),
        );
        roundtrip(
            false,
            EntityBody::AfsSchedule(AfsSchedule { schedule_id: 15, day_code: 0x7F, start_minute: 1439, duration_minutes: 16383 }),
        );
        roundtrip(
            false,
            EntityBody::Application(ApplicationInfo {
                short_id: 1,
                stream_id: 1,
                packet_mode: true,
                data_unit_indicator: true,
                packet_id: 2,
                enhancement: false,
                app_domain: 1,
                packet_length: 45,
                application_data: vec![0x04, 0x4A, 0x00, 0x01],
            }),
        );
        roundtrip(
            true,
            EntityBody::Application(ApplicationInfo {
                short_id: 3,
                stream_id: 2,
                packet_mode: false,
                enhancement: true,
                app_domain: 0,
                application_data: vec![0x00, 0x05],
                ..Default::default()
            }),
        );
        roundtrip(
            true,
            EntityBody::Announcement(Announcement {
                short_id_flags: 0b1001,
                other_service: true,
                id: 2,
                support_flags: 0x3FF,
                switching_flags: 0b10_0000_0101,
            }),
        );
        roundtrip(
            false,
            EntityBody::AfsRegion(AfsRegion {
                region_id: 4,
                latitude: -45,
                longitude: -180,
                latitude_extent: 100,
                longitude_extent: 255,
                ciraf_zones: vec![1, 27, 85],
            }),
        );
        roundtrip(false, EntityBody::TimeDate(TimeAndDate::from_utc(2026, 9, 29, 18, 30)));
        roundtrip(
            false,
            EntityBody::TimeDate(TimeAndDate {
                local_offset: Some(LocalTimeOffset::from_minutes(-330)),
                ..TimeAndDate::from_utc(1999, 12, 31, 23, 59)
            }),
        );
        roundtrip(
            false,
            EntityBody::Audio(AudioInfo {
                short_id: 0,
                stream_id: 0,
                coding: 0,
                sbr: true,
                mode: 1,
                sample_rate: 3,
                text: true,
                enhancement: false,
                coder_field: 0,
                rfa: false,
                codec_config: vec![],
            }),
        );
        roundtrip(
            true,
            EntityBody::Audio(AudioInfo {
                short_id: 2,
                stream_id: 3,
                coding: 3,
                mode: 2,
                sample_rate: 6,
                coder_field: 0b01000,
                codec_config: vec![0x13, 0x88, 0x00, 0x42],
                ..Default::default()
            }),
        );
        roundtrip(
            true,
            EntityBody::FacChannel(Some(FacChannelParameters {
                enhancement: false,
                robustness_mode: 1,
                rm_flag: false,
                spectrum_occupancy: 3,
                short_interleaving: false,
                msc_mode: 3,
                sdc_mode: 1,
                num_services: 5,
                rfa: 0,
            })),
        );
        roundtrip(true, EntityBody::FacChannel(None));
        for (system_id, id) in [(0u8, Some(0x12_3456u32)), (2, None), (4, Some(0xD3C2)), (9, Some(0xE1_C0DE)), (11, Some(0xDEAD_BEEF))] {
            let freqs = if system_id <= 2 { vec![7325, 0x8000 | 9_500] } else { vec![64, 101, 3] };
            roundtrip(
                true,
                EntityBody::AfsOtherService(AfsOtherService {
                    announcement: system_id == 4,
                    id: 1,
                    same_service: true,
                    system_id,
                    region_schedule: (system_id == 9).then_some(RegionSchedule { region_id: 1, schedule_id: 2 }),
                    other_service_id: id,
                    frequencies: freqs,
                }),
            );
        }
        roundtrip(
            false,
            EntityBody::LanguageCountry(LanguageCountry { short_id: 1, language: *b"deu", country: *b"de" }),
        );
        roundtrip(
            true,
            EntityBody::AfsDetailedRegion(AfsDetailedRegion {
                region_id: 9,
                squares: vec![
                    RegionSquare { latitude: -1440, longitude: -2880, latitude_extent: 2047, longitude_extent: 17 },
                    RegionSquare { latitude: 800, longitude: 2879, latitude_extent: 1, longitude_extent: 2047 },
                ],
            }),
        );
        roundtrip(false, EntityBody::PacketFec(PacketStreamFec { stream_id: 3, r: 180, c: 239, packet_length: 255 }));
        roundtrip(
            false,
            EntityBody::ServiceLinking(ServiceLinking { active: true, hard: true, international: true, lsn: 0xABC, id_list: None }),
        );
        roundtrip(
            true,
            EntityBody::ServiceLinking(ServiceLinking {
                active: false,
                hard: false,
                international: false,
                lsn: 1,
                id_list: Some(LinkIdList { qualifier: 1, data: false, ids: vec![0xD3C2, 0x1234] }),
            }),
        );
        roundtrip(
            true,
            EntityBody::ServiceLinking(ServiceLinking {
                active: true,
                hard: false,
                international: true,
                lsn: 77,
                id_list: Some(LinkIdList { qualifier: 3, data: false, ids: vec![0x10_2030] }),
            }),
        );
        roundtrip(false, EntityBody::Unknown { entity_type: 15, raw: RawBody { first_nibble: 5, bytes: vec![1, 2, 3] } });
    }

    #[test]
    fn version_mechanisms() {
        assert_eq!(version_mechanism(0), VersionMechanism::Reconfiguration);
        assert_eq!(version_mechanism(9), VersionMechanism::Reconfiguration);
        assert_eq!(version_mechanism(1), VersionMechanism::Unique);
        assert_eq!(version_mechanism(3), VersionMechanism::List);
        assert_eq!(version_mechanism(15), VersionMechanism::List);
    }

    #[test]
    fn dream_transmitter_block() {
        // Entities exactly as Dream's CSDCTransmit writes them for one audio service:
        // type 0 (EEP, 1 stream of 1234 bytes), type 9 (AAC 24 kHz, SBR, mono, text),
        // type 1 ("Dream"), then zero padding.
        let mut w = BitWriter::new();
        // Type 0: length (4 + 24 - 4)/8 = 3, version 0, type 0.
        w.write(3, 7);
        w.write(0, 1);
        w.write(0, 4);
        w.write(0, 2);
        w.write(1, 2);
        w.write(0, 12);
        w.write(1234, 12);
        // Type 9: length 2.
        w.write(2, 7);
        w.write(0, 1);
        w.write(9, 4);
        w.write(0, 2); // short id
        w.write(0, 2); // stream id
        w.write(0, 2); // AAC
        w.write(1, 1); // SBR
        w.write(0, 2); // mono
        w.write(3, 3); // 24 kHz
        w.write(1, 1); // text
        w.write(0, 1);
        w.write(0, 5);
        w.write(0, 1);
        // Type 1.
        w.write(5, 7);
        w.write(0, 1);
        w.write(1, 4);
        w.write(0, 2);
        w.write(0, 2);
        w.write_bytes(b"Dream");
        let mut data = pack(&w.into_bits());
        data.resize(37, 0);
        let e = parse_sdc(&data);
        assert_eq!(e.len(), 3);
        let EntityBody::Multiplex(m) = &e[0].body else { panic!("{:?}", e[0]) };
        assert_eq!(m.streams(false), vec![StreamLengths { part_a: 0, part_b: 1234 }]);
        assert_eq!(m.protection(false), MscProtection { part_a: 0, part_b: 1, hierarchical: 0 });
        let EntityBody::Audio(a) = &e[1].body else { panic!("{:?}", e[1]) };
        assert!(a.sbr && a.text && a.sample_rate == 3 && a.coding == 0);
        let EntityBody::Label(l) = &e[2].body else { panic!("{:?}", e[2]) };
        assert_eq!(l.text(), "Dream");
    }

    #[test]
    fn invalid_entities_are_preserved() {
        // Label with rfu bits set, then a schedule with a wrong length, then a
        // language entity: the first two are Invalid, the third still parses.
        let mut data = vec![(3 << 1), 0x13, b'a', b'b', b'c'];
        data.extend([(2 << 1), 0x41, 0xFF, 0xFF]);
        let lang = SdcEntity::new(false, EntityBody::LanguageCountry(LanguageCountry { short_id: 0, language: *b"eng", country: *b"gb" }));
        data.extend(lang.encode().unwrap());
        let e = parse_sdc(&data);
        assert_eq!(e.len(), 3);
        assert!(matches!(e[0].body, EntityBody::Invalid { entity_type: 1, .. }));
        assert!(matches!(e[1].body, EntityBody::Invalid { entity_type: 4, .. }));
        assert_eq!(e[2], lang);
        // Invalid entities re-encode to the same bytes.
        let mut re = Vec::new();
        for x in &e {
            re.extend(x.encode().unwrap());
        }
        assert_eq!(re, data);
    }

    #[test]
    fn garbage_never_panics() {
        let mut x = 0x1234_5678u32;
        for len in 0..300 {
            let data: Vec<u8> = (0..len)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    x as u8
                })
                .collect();
            for e in parse_sdc(&data) {
                // Everything that parses can be re-encoded.
                let _ = e.encode();
                let _ = e.length();
            }
        }
    }

    #[test]
    fn truncated_entity() {
        let label = SdcEntity::new(false, EntityBody::Label(Label::new(0, "truncated label"))).encode().unwrap();
        let e = parse_sdc(&label[..8]);
        assert_eq!(e.len(), 1);
        assert!(matches!(e[0].body, EntityBody::Invalid { entity_type: 1, reason: "truncated", .. }));
    }

    #[test]
    fn capacity_and_skipping() {
        let big = SdcEntity::new(false, EntityBody::Label(Label::new(0, "0123456789ABCDEF")));
        let small = SdcEntity::new(false, EntityBody::Label(Label::new(1, "ab")));
        // 18 + 4 bytes; with capacity 10 only the small one fits.
        let f = encode_sdc_data(&[big.clone(), small.clone()], 10).unwrap();
        assert_eq!(f.written, vec![1]);
        assert_eq!(f.skipped, vec![0]);
        assert_eq!(parse_sdc(&f.data), vec![small.clone()]);
        // Exact fit is allowed.
        let f = encode_sdc_data(&[big.clone(), small.clone()], 22).unwrap();
        assert_eq!(f.written, vec![0, 1]);
        assert_eq!(parse_sdc(&f.data), vec![big, small]);
        assert_eq!(sdc_data_capacity(316), 37);
    }

    #[test]
    fn sdc_block_crc_matches_receiver_check() {
        let data = encode_sdc_data(&[SdcEntity::new(false, EntityBody::Label(Label::new(0, "CRC")))], 13).unwrap().data;
        let bits = sdc_block_bits(5, &data, 20 + 13 * 8 + 3);
        assert_eq!(bits.len(), 20 + 104 + 3);
        // Receiver side (rx::chain::decode_sdc): CRC over the AFS index byte and data.
        let mut r = BitReader::from_bits(&bits);
        let afs = r.read(4) as u8;
        let d: Vec<u8> = (0..13).map(|_| r.read_byte()).collect();
        let rx_crc = r.read(16);
        let mut crc = Crc::crc16();
        crc.add_byte(afs);
        crc.add_bytes(&d);
        assert_eq!((afs, d, crc.value()), (5, data, rx_crc));
    }

    #[test]
    fn mjd_conversion() {
        // Known values: 1858-11-17 = 0, 1970-01-01 = 40587, 2000-01-01 = 51544.
        assert_eq!(mjd_from_civil(1858, 11, 17), 0);
        assert_eq!(mjd_from_civil(1970, 1, 1), 40_587);
        assert_eq!(mjd_from_civil(2000, 1, 1), 51_544);
        assert_eq!(mjd_from_civil(2026, 9, 29), 61_312);
        for mjd in (0..100_000).step_by(37) {
            let (y, m, d) = civil_from_mjd(mjd);
            assert_eq!(mjd_from_civil(y, m, d), mjd);
        }
        assert_eq!(civil_from_mjd(51_603), (2000, 2, 29));
        assert_eq!(civil_from_mjd(51_604), (2000, 3, 1));
    }

    #[test]
    fn label_text_control_and_padding() {
        let l = Label { short_id: 0, bytes: b"\x04RTL  \0".to_vec() };
        assert_eq!(l.text_control(), Some(4));
        assert_eq!(l.text(), "RTL");
    }

    #[test]
    fn other_service_frequencies() {
        let o = AfsOtherService { system_id: 3, frequencies: vec![0, 204, 250], ..Default::default() };
        assert_eq!(o.frequency(0), Some(OtherFrequency::FmKhz(87_500)));
        assert_eq!(o.frequency(1), Some(OtherFrequency::FmKhz(107_900)));
        assert_eq!(o.frequency(2), Some(OtherFrequency::Raw(250)));
        let d = AfsOtherService { system_id: 0, frequencies: vec![0x8000 | 10_000], ..Default::default() };
        assert_eq!(d.frequency(0), Some(OtherFrequency::Khz(100_000)));
    }
}
