#![allow(dead_code)]

use std::path::PathBuf;

use mkff_audio_format::{open_path, DemuxPacket, Demuxer};

pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("testdata")
}

pub fn fixture(name: &str) -> PathBuf {
    root().join("audio/format").join(name)
}

pub fn clip(name: &str) -> PathBuf {
    root().join("codecs").join(name)
}

pub fn open(p: &PathBuf) -> Box<dyn Demuxer> {
    open_path(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

pub fn read_all(d: &mut dyn Demuxer) -> Vec<DemuxPacket> {
    let mut out = Vec::new();
    while let Some(p) = d.next_packet().unwrap() {
        out.push(p);
        assert!(out.len() < 1_000_000);
    }
    out
}

/// (packet count, sum of durations, first pts) of one track.
pub fn stats(pkts: &[DemuxPacket], track: usize) -> (usize, u64, i64) {
    let t: Vec<_> = pkts.iter().filter(|p| p.track == track).collect();
    (
        t.len(),
        t.iter().map(|p| u64::from(p.duration)).sum(),
        t.first().map_or(0, |p| p.pts),
    )
}
