use super::AudioEngineState;

/// The bug this covers: the UI re-syncs the project whenever it goes dirty,
/// including while the transport is running, and `load_project` parks the
/// engine in `LoadingProject` for the whole rebuild — seconds, once
/// plug-ins have to be re-instantiated. While that state silenced the
/// output, a song simply stopped part-way through and only pressing Play
/// brought it back, because `StartTransport` is what forces `Running`.
///
/// The graph the callback holds is not touched until the swap command
/// arrives, so there is nothing to protect against here.
#[test]
fn preparing_the_next_graph_does_not_silence_the_one_being_played() {
    assert!(!AudioEngineState::LoadingProject.outputs_silence());
    assert!(AudioEngineState::LoadingProject.renders_transport());
}

/// Everything that means the graph or the device is going away still has to
/// silence: those are the states where continuing to render is a
/// use-after-free or a write into a closed device, not a dropout.
#[test]
fn teardown_states_stay_hard_silence() {
    for state in [
        AudioEngineState::ClosingProject,
        AudioEngineState::DeviceSwitching,
        AudioEngineState::Suspended,
    ] {
        assert!(state.outputs_silence(), "{state:?} must output silence");
        assert!(!state.renders_graph(), "{state:?} must not render");
        assert!(
            !state.renders_transport(),
            "{state:?} must not advance the transport"
        );
    }
}

/// `Paused` is the one state where the two questions differ, and the
/// difference is the point: the timeline stops, the engine does not. Live
/// input, monitoring, MIDI preview, instruments, FX and plugin tails all
/// keep processing. A stale `playing_local` left over from a graph swap
/// must still never advance the transport here.
#[test]
fn paused_renders_the_graph_and_never_advances_the_transport() {
    assert!(AudioEngineState::Paused.renders_graph());
    assert!(!AudioEngineState::Paused.outputs_silence());
    assert!(!AudioEngineState::Paused.renders_transport());
}

#[test]
fn running_plays() {
    assert!(!AudioEngineState::Running.outputs_silence());
    assert!(AudioEngineState::Running.renders_transport());
}

/// The state crosses to the callback as a byte, so an unknown value must
/// land on the state that keeps audio flowing rather than on one that
/// silences it.
#[test]
fn an_unknown_state_byte_decodes_to_running() {
    assert_eq!(AudioEngineState::from_u8(0), AudioEngineState::Running);
    assert_eq!(AudioEngineState::from_u8(200), AudioEngineState::Running);
    for state in [
        AudioEngineState::Running,
        AudioEngineState::Paused,
        AudioEngineState::LoadingProject,
        AudioEngineState::ClosingProject,
        AudioEngineState::DeviceSwitching,
        AudioEngineState::Suspended,
    ] {
        assert_eq!(AudioEngineState::from_u8(state as u8), state);
    }
}
