//! Who is in the room and what they are publishing.
//!
//! The one structural rule here is that nothing is keyed by account. One
//! Futureboard user may hold several participants in the same jam — a laptop
//! running Studio and a phone used as a talkback mic — so a
//! `HashMap<UserId, Participant>` would silently make the second device
//! overwrite the first. Participants are keyed by participant id and *grouped*
//! by user for display.
//!
//! Usernames appear only in labels. Every lookup, every route and every stored
//! binding goes through an id.

use std::collections::HashMap;

use crate::ids::{DeviceId, MediaAlias, ParticipantId, StreamId, UserId};
use crate::protocol::{
    AudioFormat, ChannelMetadata, ParticipantSummary, StreamSummary, UserSummary,
};

/// One participant, as the client tracks it.
#[derive(Debug, Clone)]
pub struct Participant {
    pub id: ParticipantId,
    pub user_id: UserId,
    pub device_id: DeviceId,
    pub user: UserSummary,
    pub device_name: String,
    pub role: String,
    pub connection_state: String,
    pub transport: String,
    pub summary: ParticipantSummary,
}

impl Participant {
    fn from_summary(summary: ParticipantSummary) -> Self {
        Self {
            id: ParticipantId::new(summary.id.clone()),
            user_id: UserId::new(summary.user.id.clone()),
            device_id: DeviceId::new(summary.device_id.clone()),
            user: summary.user.clone(),
            device_name: summary.device_name.clone(),
            role: summary.role.clone(),
            connection_state: summary.connection_state.clone(),
            transport: summary.transport.clone(),
            summary,
        }
    }

    /// What a UI row shows: the account, plus the device when the same account
    /// is present more than once.
    pub fn label(&self) -> String {
        if self.device_name.is_empty() {
            self.user.handle()
        } else {
            format!("{} · {}", self.user.handle(), self.device_name)
        }
    }

    pub fn online(&self) -> bool {
        self.connection_state == crate::protocol::connection_state::CONNECTED
    }
}

/// One remote stream, with whatever the receiver has learned about it.
#[derive(Debug, Clone)]
pub struct RemoteStream {
    pub id: StreamId,
    pub alias: MediaAlias,
    pub participant_id: ParticipantId,
    pub user_id: UserId,
    pub device_id: DeviceId,
    pub name: String,
    pub channels: usize,
    pub channel_metadata: Vec<ChannelMetadata>,
    /// The format the server negotiated *for this receiver*. `None` until an
    /// `audio.format_selected` arrives, which is also the signal that this
    /// client is subscribed and audio is on its way.
    pub format: Option<AudioFormat>,
    pub summary: StreamSummary,
}

impl RemoteStream {
    fn from_summary(summary: StreamSummary) -> Self {
        Self {
            id: StreamId::new(summary.id.clone()),
            alias: MediaAlias(summary.media_alias),
            participant_id: ParticipantId::new(summary.participant_id.clone()),
            user_id: UserId::new(summary.user_id.clone()),
            device_id: DeviceId::new(summary.device_id.clone()),
            name: summary.name.clone(),
            channels: summary.channels.max(0) as usize,
            channel_metadata: summary.channel_metadata.clone(),
            format: None,
            summary,
        }
    }

    /// Conventional label for one channel: the publisher's own metadata when it
    /// supplied any, otherwise the layout convention.
    pub fn channel_label(&self, index: usize) -> String {
        if let Some(meta) = self
            .channel_metadata
            .iter()
            .find(|meta| meta.index as usize == index)
        {
            if !meta.label.is_empty() {
                return meta.label.clone();
            }
            if !meta.role.is_empty() {
                return meta.role.clone();
            }
        }
        match (self.channels, index) {
            (1, _) => "Mono".to_string(),
            (2, 0) => "L".to_string(),
            (2, 1) => "R".to_string(),
            _ => format!("Ch {}", index + 1),
        }
    }

    /// Whether audio is actually expected: the stream is live and the server
    /// has told this receiver what format it will arrive in.
    pub fn receiving(&self) -> bool {
        self.summary.active && self.format.is_some()
    }
}

/// The room, as this client sees it.
#[derive(Debug, Default)]
pub struct JamRegistry {
    participants: HashMap<ParticipantId, Participant>,
    streams: HashMap<StreamId, RemoteStream>,
    /// Media alias to stream id. Arriving packets are addressed by alias, so
    /// this is the hot lookup on the receive path.
    by_alias: HashMap<u32, StreamId>,
    /// The highest room event sequence applied, used to resume without a full
    /// replay.
    seq: u64,
}

impl JamRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace everything with a fresh room snapshot.
    ///
    /// Returns the streams the old view had and the new one does not — those
    /// that ended while this client was away. Nobody sends an unpublish for
    /// them, so the caller must end them itself: a reconnect that only
    /// replaced the view left their audio slots held, and after enough
    /// dropouts there was no slot left for the next performer.
    pub fn reset(
        &mut self,
        participants: Vec<ParticipantSummary>,
        streams: Vec<StreamSummary>,
        seq: u64,
    ) -> Vec<StreamId> {
        let before: Vec<StreamId> = self.streams.keys().cloned().collect();
        self.participants.clear();
        self.streams.clear();
        self.by_alias.clear();
        for participant in participants {
            self.upsert_participant(participant);
        }
        for stream in streams {
            self.upsert_stream(stream);
        }
        self.seq = seq;
        before
            .into_iter()
            .filter(|id| !self.streams.contains_key(id))
            .collect()
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Record the sequence of an applied event. Monotonic: a replayed event
    /// must not wind the resume point backwards.
    pub fn observe_seq(&mut self, seq: u64) {
        if seq > self.seq {
            self.seq = seq;
        }
    }

    pub fn upsert_participant(&mut self, summary: ParticipantSummary) {
        if summary.id.is_empty() {
            return;
        }
        let participant = Participant::from_summary(summary);
        self.participants
            .insert(participant.id.clone(), participant);
    }

    /// Remove a participant and everything it was publishing.
    ///
    /// Its streams go with it: leaving them behind would show a guitar in the
    /// track input menu that no longer has anybody playing it.
    pub fn remove_participant(&mut self, id: &ParticipantId) -> Vec<RemoteStream> {
        self.participants.remove(id);
        let owned: Vec<StreamId> = self
            .streams
            .values()
            .filter(|stream| &stream.participant_id == id)
            .map(|stream| stream.id.clone())
            .collect();
        owned
            .into_iter()
            .filter_map(|stream_id| self.remove_stream(&stream_id))
            .collect()
    }

    pub fn set_participant_state(
        &mut self,
        id: &ParticipantId,
        connection_state: &str,
        transport: &str,
    ) {
        if let Some(participant) = self.participants.get_mut(id) {
            participant.connection_state = connection_state.to_string();
            participant.summary.connection_state = connection_state.to_string();
            if !transport.is_empty() {
                participant.transport = transport.to_string();
                participant.summary.transport = transport.to_string();
            }
        }
    }

    pub fn upsert_stream(&mut self, summary: StreamSummary) {
        if summary.id.is_empty() {
            return;
        }
        let mut stream = RemoteStream::from_summary(summary);
        // A republished stream keeps the format the server already selected for
        // this receiver, so a metadata-only update does not silently mute it.
        if let Some(existing) = self.streams.get(&stream.id) {
            stream.format = existing.format;
        }
        self.by_alias.insert(stream.alias.0, stream.id.clone());
        self.streams.insert(stream.id.clone(), stream);
    }

    pub fn remove_stream(&mut self, id: &StreamId) -> Option<RemoteStream> {
        let stream = self.streams.remove(id)?;
        self.by_alias.remove(&stream.alias.0);
        Some(stream)
    }

    /// Record the format the server negotiated for this receiver.
    pub fn set_stream_format(&mut self, id: &StreamId, format: AudioFormat) -> bool {
        match self.streams.get_mut(id) {
            Some(stream) => {
                stream.format = Some(format);
                true
            }
            None => false,
        }
    }

    /// Forget the negotiated format, which is what marks a stream as no longer
    /// arriving.
    ///
    /// A stream with no format is listed and not received — the same state it
    /// is in between a publish and its `audio.format_selected` — so an
    /// unsubscribe leaves it visible in the room and silent, which is exactly
    /// what it now is.
    pub fn clear_stream_format(&mut self, id: &StreamId) -> bool {
        match self.streams.get_mut(id) {
            Some(stream) => stream.format.take().is_some(),
            None => false,
        }
    }

    pub fn participant(&self, id: &ParticipantId) -> Option<&Participant> {
        self.participants.get(id)
    }

    pub fn stream(&self, id: &StreamId) -> Option<&RemoteStream> {
        self.streams.get(id)
    }

    /// Resolve an arriving packet's alias to the stream it belongs to.
    pub fn stream_for_alias(&self, alias: u32) -> Option<&RemoteStream> {
        let id = self.by_alias.get(&alias)?;
        self.streams.get(id)
    }

    /// Every participant, ordered so a UI list is stable across frames.
    pub fn participants(&self) -> Vec<&Participant> {
        let mut out: Vec<&Participant> = self.participants.values().collect();
        out.sort_by(|a, b| {
            a.user
                .username
                .cmp(&b.user.username)
                .then_with(|| a.device_id.as_str().cmp(b.device_id.as_str()))
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        out
    }

    /// Every participant belonging to one account. Several is normal.
    pub fn participants_for_user(&self, user: &UserId) -> Vec<&Participant> {
        let mut out: Vec<&Participant> = self
            .participants
            .values()
            .filter(|participant| &participant.user_id == user)
            .collect();
        out.sort_by(|a, b| a.device_id.as_str().cmp(b.device_id.as_str()));
        out
    }

    pub fn streams(&self) -> Vec<&RemoteStream> {
        let mut out: Vec<&RemoteStream> = self.streams.values().collect();
        out.sort_by(|a, b| {
            a.user_id
                .as_str()
                .cmp(b.user_id.as_str())
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        out
    }

    pub fn streams_for_participant(&self, participant: &ParticipantId) -> Vec<&RemoteStream> {
        let mut out: Vec<&RemoteStream> = self
            .streams
            .values()
            .filter(|stream| &stream.participant_id == participant)
            .collect();
        out.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        out
    }

    /// Find the stream a saved binding refers to.
    ///
    /// The stream id is canonical while a session lasts, so it is tried first.
    /// A project reopened against a new session will not find it — the
    /// publisher restarted and its streams were minted again — so the fallback
    /// matches the same account publishing a stream of the same name, which is
    /// what "my guitar" means to the person who saved the routing. The device
    /// narrows it when the account is present on more than one.
    pub fn resolve_binding(
        &self,
        user: &UserId,
        stream_id: Option<&StreamId>,
        device: Option<&DeviceId>,
        stream_name: Option<&str>,
    ) -> Option<&RemoteStream> {
        if let Some(id) = stream_id {
            if let Some(stream) = self.streams.get(id) {
                if &stream.user_id == user {
                    return Some(stream);
                }
            }
        }
        let name = stream_name?;
        let mut candidates: Vec<&RemoteStream> = self
            .streams
            .values()
            .filter(|stream| &stream.user_id == user && stream.name == name)
            .collect();
        if candidates.is_empty() {
            return None;
        }
        if let Some(device) = device {
            if let Some(exact) = candidates
                .iter()
                .find(|stream| &stream.device_id == device)
                .copied()
            {
                return Some(exact);
            }
        }
        // Deterministic pick, so two runs of the same project bind the same way.
        candidates.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        candidates.first().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.participants.is_empty() && self.streams.is_empty()
    }

    pub fn clear(&mut self) {
        self.participants.clear();
        self.streams.clear();
        self.by_alias.clear();
        self.seq = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AudioCodec, SampleFormat};

    fn participant(id: &str, user: &str, username: &str, device: &str) -> ParticipantSummary {
        ParticipantSummary {
            id: id.to_string(),
            jam_id: "jam_1".to_string(),
            user: UserSummary {
                id: user.to_string(),
                username: username.to_string(),
                display_name: username.to_string(),
                avatar_url: String::new(),
            },
            device_id: device.to_string(),
            device_name: device.to_string(),
            role: "performer".to_string(),
            connection_state: "connected".to_string(),
            ..Default::default()
        }
    }

    fn stream(
        id: &str,
        alias: u32,
        participant: &str,
        user: &str,
        device: &str,
        name: &str,
    ) -> StreamSummary {
        StreamSummary {
            id: id.to_string(),
            path: format!("jam/jam_1/{user}/{device}/{id}"),
            media_alias: alias,
            participant_id: participant.to_string(),
            user_id: user.to_string(),
            device_id: device.to_string(),
            name: name.to_string(),
            direction: "send".to_string(),
            codec: AudioCodec::Pcm,
            sample_rate: 48000,
            sample_format: SampleFormat::F32Le,
            channels: 2,
            channel_metadata: Vec::new(),
            clock_domain: "session".to_string(),
            latency: Default::default(),
            active: true,
        }
    }

    #[test]
    fn one_account_on_two_devices_is_two_participants() {
        let mut registry = JamRegistry::new();
        registry.upsert_participant(participant("pcp_1", "usr_1", "hachi224", "studio-mac"));
        registry.upsert_participant(participant("pcp_2", "usr_1", "hachi224", "phone"));

        assert_eq!(registry.participants().len(), 2);
        let mine = registry.participants_for_user(&UserId::new("usr_1"));
        assert_eq!(mine.len(), 2);
        assert_eq!(mine[0].device_id.as_str(), "phone");
        assert_eq!(mine[1].device_id.as_str(), "studio-mac");
    }

    /// A reconnect's fresh snapshot reports, by omission, the streams that
    /// ended while this client was away, so their audio slots can be freed.
    #[test]
    fn a_fresh_snapshot_reports_the_streams_that_ended_meanwhile() {
        let mut registry = JamRegistry::new();
        registry.upsert_participant(participant("pcp_1", "usr_1", "hachi224", "studio-mac"));
        registry.upsert_stream(stream("str_1", 1, "pcp_1", "usr_1", "studio-mac", "Guitar"));
        registry.upsert_stream(stream("str_2", 2, "pcp_1", "usr_1", "studio-mac", "Vocal"));

        let ended = registry.reset(
            vec![participant("pcp_1", "usr_1", "hachi224", "studio-mac")],
            vec![stream("str_2", 2, "pcp_1", "usr_1", "studio-mac", "Vocal")],
            7,
        );
        assert_eq!(ended, vec![StreamId::new("str_1")]);
        assert_eq!(registry.streams().len(), 1);
        assert!(registry
            .reset(Vec::new(), Vec::new(), 8)
            .contains(&StreamId::new("str_2")));
    }

    #[test]
    fn a_departing_participant_takes_its_streams_with_it() {
        let mut registry = JamRegistry::new();
        registry.upsert_participant(participant("pcp_1", "usr_1", "hachi224", "studio-mac"));
        registry.upsert_stream(stream("str_1", 1, "pcp_1", "usr_1", "studio-mac", "Guitar"));
        registry.upsert_stream(stream("str_2", 2, "pcp_1", "usr_1", "studio-mac", "Vocal"));

        let removed = registry.remove_participant(&ParticipantId::new("pcp_1"));
        assert_eq!(removed.len(), 2);
        assert!(registry.streams().is_empty());
        assert!(registry.stream_for_alias(1).is_none());
    }

    #[test]
    fn a_packet_alias_resolves_to_its_stream() {
        let mut registry = JamRegistry::new();
        registry.upsert_stream(stream(
            "str_1",
            42,
            "pcp_1",
            "usr_1",
            "studio-mac",
            "Guitar",
        ));
        let found = registry.stream_for_alias(42).expect("alias resolves");
        assert_eq!(found.id.as_str(), "str_1");
        assert!(registry.stream_for_alias(43).is_none());
    }

    #[test]
    fn unpublishing_frees_the_alias() {
        let mut registry = JamRegistry::new();
        registry.upsert_stream(stream("str_1", 7, "pcp_1", "usr_1", "studio-mac", "Guitar"));
        registry.remove_stream(&StreamId::new("str_1"));
        assert!(registry.stream_for_alias(7).is_none());
    }

    #[test]
    fn a_metadata_update_does_not_forget_the_negotiated_format() {
        let mut registry = JamRegistry::new();
        registry.upsert_stream(stream("str_1", 1, "pcp_1", "usr_1", "studio-mac", "Guitar"));
        registry.set_stream_format(
            &StreamId::new("str_1"),
            AudioFormat {
                codec: AudioCodec::Pcm,
                sample_rate: 48000,
                channels: 2,
                format: SampleFormat::F32Le,
                bitrate: 0,
                frame_samples: 128,
            },
        );
        let mut updated = stream("str_1", 1, "pcp_1", "usr_1", "studio-mac", "Guitar");
        updated.name = "Guitar (DI)".to_string();
        registry.upsert_stream(updated);

        let stream = registry.stream(&StreamId::new("str_1")).expect("present");
        assert_eq!(stream.name, "Guitar (DI)");
        assert!(
            stream.receiving(),
            "the selected format survived the update"
        );
    }

    #[test]
    fn a_saved_binding_prefers_the_stream_id_while_the_session_lasts() {
        let mut registry = JamRegistry::new();
        registry.upsert_stream(stream("str_1", 1, "pcp_1", "usr_1", "studio-mac", "Guitar"));
        registry.upsert_stream(stream("str_2", 2, "pcp_1", "usr_1", "studio-mac", "Vocal"));

        let found = registry
            .resolve_binding(
                &UserId::new("usr_1"),
                Some(&StreamId::new("str_2")),
                None,
                Some("Guitar"),
            )
            .expect("resolves");
        assert_eq!(found.id.as_str(), "str_2");
    }

    #[test]
    fn a_reopened_project_rebinds_by_account_and_stream_name() {
        let mut registry = JamRegistry::new();
        // A new session: the ids were minted again.
        registry.upsert_stream(stream(
            "str_99",
            5,
            "pcp_9",
            "usr_1",
            "studio-mac",
            "Guitar",
        ));

        let found = registry
            .resolve_binding(
                &UserId::new("usr_1"),
                Some(&StreamId::new("str_1")),
                Some(&DeviceId::new("studio-mac")),
                Some("Guitar"),
            )
            .expect("falls back to the account and name");
        assert_eq!(found.id.as_str(), "str_99");
    }

    #[test]
    fn a_rebind_never_crosses_to_another_account() {
        let mut registry = JamRegistry::new();
        registry.upsert_stream(stream("str_9", 5, "pcp_9", "usr_2", "laptop", "Guitar"));
        assert!(registry
            .resolve_binding(
                &UserId::new("usr_1"),
                Some(&StreamId::new("str_1")),
                None,
                Some("Guitar"),
            )
            .is_none());
    }

    #[test]
    fn a_rebind_prefers_the_same_device_when_an_account_has_several() {
        let mut registry = JamRegistry::new();
        registry.upsert_stream(stream("str_a", 1, "pcp_1", "usr_1", "phone", "Guitar"));
        registry.upsert_stream(stream("str_b", 2, "pcp_2", "usr_1", "studio-mac", "Guitar"));

        let found = registry
            .resolve_binding(
                &UserId::new("usr_1"),
                None,
                Some(&DeviceId::new("studio-mac")),
                Some("Guitar"),
            )
            .expect("resolves");
        assert_eq!(found.device_id.as_str(), "studio-mac");
    }

    #[test]
    fn channel_labels_prefer_the_publishers_own_metadata() {
        let mut summary = stream("str_1", 1, "pcp_1", "usr_1", "studio-mac", "Guitar");
        summary.channel_metadata = vec![
            ChannelMetadata {
                index: 0,
                label: "Neck".to_string(),
                role: "L".to_string(),
            },
            ChannelMetadata {
                index: 1,
                label: String::new(),
                role: "R".to_string(),
            },
        ];
        let mut registry = JamRegistry::new();
        registry.upsert_stream(summary);
        let stream = registry.stream(&StreamId::new("str_1")).expect("present");
        assert_eq!(stream.channel_label(0), "Neck");
        assert_eq!(stream.channel_label(1), "R");
    }

    #[test]
    fn the_resume_sequence_never_moves_backwards() {
        let mut registry = JamRegistry::new();
        registry.observe_seq(10);
        registry.observe_seq(4);
        assert_eq!(registry.seq(), 10);
    }

    #[test]
    fn a_username_change_does_not_move_a_route() {
        let mut registry = JamRegistry::new();
        registry.upsert_participant(participant("pcp_1", "usr_1", "hachi224", "studio-mac"));
        registry.upsert_stream(stream("str_1", 1, "pcp_1", "usr_1", "studio-mac", "Guitar"));

        // The same account, now under a different handle.
        registry.upsert_participant(participant("pcp_1", "usr_1", "hachi_new", "studio-mac"));

        let bound = registry
            .resolve_binding(
                &UserId::new("usr_1"),
                Some(&StreamId::new("str_1")),
                None,
                Some("Guitar"),
            )
            .expect("still resolves");
        assert_eq!(bound.id.as_str(), "str_1");
        assert_eq!(
            registry
                .participant(&ParticipantId::new("pcp_1"))
                .expect("present")
                .user
                .handle(),
            "@hachi_new"
        );
    }
}
