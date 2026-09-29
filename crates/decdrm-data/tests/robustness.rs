//! Off-air data is untrusted: no input may make a parser or decoder panic.

use decdrm_data::crc::append_crc16;
use decdrm_data::datagroup::{DataGroup, SegmentField, UserAccess};
use decdrm_data::journaline::NmlObject;
use decdrm_data::mot::{MotDecoder, MotDirectory, MotHeader};
use decdrm_data::packet::{Packet, PacketDemux};
use decdrm_data::{AppDomain, DataDecoder, DataServiceConfig, UserApplication, epg};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }

    /// Random bytes biased towards small values and structure-like patterns.
    fn structured(&mut self, n: usize) -> Vec<u8> {
        (0..n)
            .map(|_| match self.below(4) {
                0 => self.below(8) as u8,
                1 => 0x80 | self.below(8) as u8,
                2 => [0x00, 0x01, 0x02, 0x03, 0x04, 0x10, 0x1A, 0x1C, 0xFE, 0xFF][self.below(10)],
                _ => self.next() as u8,
            })
            .collect()
    }
}

#[test]
fn parsers_survive_garbage() {
    let mut rng = Rng(0xDEC0DE);
    for i in 0..20_000 {
        let n = rng.below(300);
        let data = if i % 2 == 0 {
            rng.bytes(n)
        } else {
            rng.structured(n)
        };
        let _ = MotHeader::parse(&data);
        let _ = MotDirectory::parse(&data);
        let _ = NmlObject::parse(&data, rng.below(3));
        let _ = epg::decode(&data);
        let _ = DataGroup::parse(&data);
        let _ = Packet::parse(&data);
        // Same again with a valid CRC so the parsers behind the CRC checks run too.
        let mut with_crc = data.clone();
        append_crc16(&mut with_crc);
        let _ = DataGroup::parse(&with_crc);
        let _ = Packet::parse(&with_crc);
    }
}

#[test]
fn decoders_survive_garbage() {
    let mut rng = Rng(0xBAD5EED);
    let apps = [
        UserApplication::SlideShow,
        UserApplication::BroadcastWebsite,
        UserApplication::Epg,
        UserApplication::Journaline,
        UserApplication::Tpeg,
    ];
    for app in apps {
        let cfg = DataServiceConfig::packet(app, 0, 20);
        let mut dec = DataDecoder::new(cfg.clone());
        let mut mot = MotDecoder::new();
        for _ in 0..3000 {
            // Whole frames of random packets, some with valid CRCs.
            let mut frame = Vec::new();
            for _ in 0..5 {
                let mut p = rng.structured(cfg.packet_len - 2);
                p[0] &= 0xCF; // packet id 0
                append_crc16(&mut p);
                frame.extend(p);
            }
            let _ = dec.push_frame(&frame);
            // Random data units with valid data group CRCs.
            let len = rng.below(120);
            let mut unit = rng.structured(len);
            if !unit.is_empty() {
                unit[0] = (unit[0] & 0xB0) | 0x40 | (rng.below(8) as u8);
            }
            append_crc16(&mut unit);
            let _ = dec.push_data_unit(&unit);
            let _ = mot.push_data_unit(&unit);
        }
        assert_eq!(dec.stats().frames, 3000);
    }
    // Well-formed MOT data groups whose segments carry garbage: reassembly completes and
    // the header/directory parsers see random entities.
    let mut mot = MotDecoder::new();
    for _ in 0..20_000 {
        let len = rng.below(40);
        let seg = rng.structured(len);
        let mut data = vec![(len >> 8) as u8, len as u8];
        data.extend(seg);
        let dg = DataGroup {
            segment: Some(SegmentField {
                last: rng.below(3) == 0,
                number: rng.below(4) as u16,
            }),
            user_access: Some(UserAccess {
                transport_id: Some(rng.below(4) as u16),
                end_user_address: vec![],
            }),
            ..DataGroup::new([3, 4, 6, 7][rng.below(4)], data)
        };
        let _ = mot.push_data_unit(&dg.to_bytes());
    }
    assert!(mot.stats().malformed > 0);

    let mut odd = DataDecoder::new(DataServiceConfig {
        app_domain: AppDomain::Other(7),
        ..DataServiceConfig::packet(UserApplication::SlideShow, 3, 0)
    });
    let _ = odd.push_frame(&rng.bytes(100));
    let mut demux = PacketDemux::new(4).unwrap();
    for _ in 0..1000 {
        let _ = demux.push_frame(&rng.structured(37));
    }
}
