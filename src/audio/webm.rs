//! Minimal mono Opus WebM muxer, with a millisecond clock.

pub(super) struct Webm {
    segment: Vec<u8>,
    cluster: Vec<u8>,
    cluster_start: u64,
    timestamp: u64,
}

impl Webm {
    pub(super) fn new(pre_skip: u16, samples: usize) -> Self {
        let mut info = Vec::new();
        unsigned(&mut info, 0x2ad7b1, 1_000_000); // TimecodeScale: 1 ms
        element(&mut info, 0x4489, &(samples as f64 / 16.0).to_be_bytes());
        element(&mut info, 0x4d80, b"WispX");
        element(&mut info, 0x5741, b"WispX");

        let mut head = b"OpusHead\x01\x01".to_vec();
        head.extend_from_slice(&pre_skip.to_le_bytes());
        head.extend_from_slice(&16_000_u32.to_le_bytes()); // original input rate
        head.extend_from_slice(&[0, 0, 0]); // gain = 0, channel mapping family = 0

        let mut track = Vec::new();
        unsigned(&mut track, 0xd7, 1); // TrackNumber
        unsigned(&mut track, 0x73c5, 1); // TrackUID
        unsigned(&mut track, 0x83, 2); // audio
        unsigned(&mut track, 0x9c, 0); // no lacing
        element(&mut track, 0x86, b"A_OPUS");
        element(&mut track, 0x63a2, &head);
        unsigned(
            &mut track,
            0x56aa,
            u64::from(pre_skip) * 1_000_000_000 / 48_000,
        );
        unsigned(&mut track, 0x56bb, 80_000_000); // SeekPreRoll: 80 ms
        let mut audio = Vec::new();
        element(&mut audio, 0xb5, &48_000_f64.to_be_bytes());
        unsigned(&mut audio, 0x9f, 1);
        element(&mut track, 0xe1, &audio);
        let mut tracks = Vec::new();
        element(&mut tracks, 0xae, &track);

        let mut segment = Vec::new();
        element(&mut segment, 0x1549a966, &info);
        element(&mut segment, 0x1654ae6b, &tracks);
        Self {
            segment,
            cluster: Vec::new(),
            cluster_start: 0,
            timestamp: 0,
        }
    }

    /// Packets are exactly 20 ms. Only the final packet may discard padded silence.
    pub(super) fn push_packet(&mut self, packet: &[u8], discard_padding_ns: u64) {
        if self.timestamp - self.cluster_start > i16::MAX as u64 {
            self.flush_cluster();
            self.cluster_start = self.timestamp;
        }
        if self.cluster.is_empty() {
            unsigned(&mut self.cluster, 0xe7, self.cluster_start);
        }
        let relative = (self.timestamp - self.cluster_start) as i16;
        let mut block = vec![0x81]; // track 1, no lacing
        block.extend_from_slice(&relative.to_be_bytes());
        block.push(if discard_padding_ns == 0 { 0x80 } else { 0 });
        block.extend_from_slice(packet);
        if discard_padding_ns == 0 {
            element(&mut self.cluster, 0xa3, &block);
        } else {
            // SimpleBlock cannot carry DiscardPadding. Use a final BlockGroup so
            // decoders retain the speech tail without playing frame-fill silence.
            let mut group = Vec::new();
            element(&mut group, 0xa1, &block);
            element(
                &mut group,
                0x75a2,
                &(discard_padding_ns as i64).to_be_bytes(),
            );
            element(&mut self.cluster, 0xa0, &group);
        }
        self.timestamp += 20;
    }

    pub(super) fn finish(mut self) -> Vec<u8> {
        self.flush_cluster();
        let mut header = Vec::new();
        for (id, value) in [(0x4286, 1), (0x42f7, 1), (0x42f2, 4), (0x42f3, 8)] {
            unsigned(&mut header, id, value);
        }
        element(&mut header, 0x4282, b"webm");
        unsigned(&mut header, 0x4287, 4);
        unsigned(&mut header, 0x4285, 2);
        let mut file = Vec::new();
        element(&mut file, 0x1a45dfa3, &header);
        element(&mut file, 0x18538067, &self.segment);
        file
    }

    fn flush_cluster(&mut self) {
        if !self.cluster.is_empty() {
            element(&mut self.segment, 0x1f43b675, &self.cluster);
            self.cluster.clear();
        }
    }
}

fn unsigned(out: &mut Vec<u8>, id: u32, value: u64) {
    let bytes = value.to_be_bytes();
    let start = (value.leading_zeros() as usize / 8).min(7);
    element(out, id, &bytes[start..]);
}

fn element(out: &mut Vec<u8>, id: u32, bytes: &[u8]) {
    let id_bytes = id.to_be_bytes();
    out.extend_from_slice(&id_bytes[id.leading_zeros() as usize / 8..]);
    // All-ones is the unknown-size sentinel, so equality needs another byte.
    let width = (1..=8)
        .find(|width| (bytes.len() as u64) < (1_u64 << (7 * width)) - 1)
        .expect("WebM element fits in 56 bits");
    let size = bytes.len() as u64 | (1_u64 << (7 * width));
    out.extend_from_slice(&size.to_be_bytes()[8 - width..]);
    out.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    // Failure cases: reserved/incorrect EBML lengths corrupt nesting; missing Opus
    // metadata loses decoder delay; block timestamps wrap on long recordings;
    // packet bytes or final padding are lost at cluster boundaries.
    use super::*;

    fn vint(bytes: &[u8], keep_marker: bool) -> (u64, usize) {
        let width = bytes[0].leading_zeros() as usize + 1;
        let mut value = u64::from(if keep_marker {
            bytes[0]
        } else {
            bytes[0] & (0xff >> width)
        });
        for &byte in &bytes[1..width] {
            value = value * 256 + u64::from(byte);
        }
        (value, width)
    }
    fn children(mut bytes: &[u8]) -> Vec<(u64, &[u8])> {
        let mut elements = Vec::new();
        while !bytes.is_empty() {
            let (id, id_len) = vint(bytes, true);
            let (size, size_len) = vint(&bytes[id_len..], false);
            let start = id_len + size_len;
            let end = start + size as usize;
            elements.push((id, &bytes[start..end]));
            bytes = &bytes[end..];
        }
        elements
    }
    fn value(elements: &[(u64, &[u8])], id: u64) -> Vec<u8> {
        elements
            .iter()
            .find(|(key, _)| *key == id)
            .unwrap()
            .1
            .to_vec()
    }
    fn uint(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0, |n, b| n * 256 + u64::from(*b))
    }

    #[test]
    fn ebml_lengths_do_not_use_the_unknown_size_sentinel() {
        for length in [0, 1, 126, 127, 128, 16382, 16383, 16384] {
            let payload = vec![0xab; length];
            let mut encoded = Vec::new();
            element(&mut encoded, 0xec, &payload);
            let (size, width) = vint(&encoded[1..], false);
            assert_eq!(size, length as u64);
            assert_ne!(size, (1 << (7 * width)) - 1);
            assert_eq!(children(&encoded), vec![(0xec, payload.as_slice())]);
        }
    }

    #[test]
    fn declares_webm_opus_and_consistent_delay() {
        let file = Webm::new(312, 16_001).finish();
        let top = children(&file);
        let header_bytes = value(&top, 0x1a45dfa3);
        assert_eq!(value(&children(&header_bytes), 0x4282), b"webm");
        let segment_bytes = value(&top, 0x18538067);
        let segment = children(&segment_bytes);
        let info_bytes = value(&segment, 0x1549a966);
        let info = children(&info_bytes);
        assert_eq!(uint(&value(&info, 0x2ad7b1)), 1_000_000);
        assert_eq!(
            f64::from_be_bytes(value(&info, 0x4489).try_into().unwrap()),
            1000.0625
        );
        let tracks_bytes = value(&segment, 0x1654ae6b);
        let track_bytes = value(&children(&tracks_bytes), 0xae);
        let track = children(&track_bytes);
        assert_eq!(value(&track, 0x86), b"A_OPUS");
        let head = value(&track, 0x63a2);
        assert_eq!(&head[..10], b"OpusHead\x01\x01");
        assert_eq!(u16::from_le_bytes(head[10..12].try_into().unwrap()), 312);
        assert_eq!(uint(&value(&track, 0x56aa)), 6_500_000);
        assert_eq!(uint(&value(&track, 0x56bb)), 80_000_000);
        let audio_bytes = value(&track, 0xe1);
        let audio = children(&audio_bytes);
        assert_eq!(uint(&value(&audio, 0x9f)), 1);
        assert_eq!(
            f64::from_be_bytes(value(&audio, 0xb5).try_into().unwrap()),
            48_000.0
        );
    }

    #[test]
    fn long_recordings_keep_packets_and_timecodes_across_clusters() {
        let mut muxer = Webm::new(312, 16_000 * 600);
        let count = 30_001;
        for index in 0..count {
            muxer.push_packet(
                &(index as u32).to_be_bytes(),
                if index + 1 == count { 13_500_000 } else { 0 },
            );
        }
        let file = muxer.finish();
        let segment_bytes = value(&children(&file), 0x18538067);
        let segment = children(&segment_bytes);
        let mut index = 0;
        let mut clusters = 0;
        for (_, cluster_bytes) in segment.iter().filter(|(id, _)| *id == 0x1f43b675) {
            clusters += 1;
            let cluster = children(cluster_bytes);
            let base = uint(&value(&cluster, 0xe7));
            for (id, bytes) in cluster.iter().filter(|(id, _)| *id == 0xa3 || *id == 0xa0) {
                let block = if *id == 0xa0 {
                    let group = children(bytes);
                    assert_eq!(uint(&value(&group, 0x75a2)), 13_500_000);
                    value(&group, 0xa1)
                } else {
                    bytes.to_vec()
                };
                assert_eq!(block[0], 0x81);
                let relative = i16::from_be_bytes(block[1..3].try_into().unwrap());
                assert!(relative >= 0);
                assert_eq!(base + relative as u64, index * 20);
                assert_eq!(&block[4..], &(index as u32).to_be_bytes());
                index += 1;
            }
        }
        assert!(clusters > 1);
        assert_eq!(index, count);
    }
}
