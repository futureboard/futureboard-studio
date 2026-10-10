// Phase 4: playback and virtual soundcheck (Patch → Playback). A take the
// recorder wrote plays back into the channels it came from, in place of the
// interface inputs, so the mix can be built without the band.
//
// Unmistakable while on: a top-bar badge, "PB" on every channel strip that
// takes the take, and the switch here in the warning hue. Errors (a take at
// another sample rate, underruns) are the server's words.

import { useEffect, useRef, useState } from 'react'
import {
  AudioWaveform,
  FolderOpen,
  ListMusic,
  LoaderCircle,
  Pause,
  Play,
  RefreshCw,
  Repeat,
  Square,
  TriangleAlert,
  X,
} from 'lucide-react'
import { Select } from './controls.tsx'
import type { PlaybackStatus, Session, Take } from './protocol.ts'
import { act, explain, notify, request, useStore } from './store.ts'
import './playback.css'

/** The recorder's file stem for a channel name (recorder.rs `file_stem`). */
function fileStem(name: string): string {
  // eslint-disable-next-line no-control-regex
  const cleaned = name.replace(/[/\\:*?"<>|\u0000-\u001f\u007f]/g, '_')
  const trimmed = cleaned.trim().replace(/\.+$/, '')
  return trimmed === '' ? 'Track' : trimmed
}

function stemOf(file: string): string {
  const dot = file.lastIndexOf('.')
  return dot > 0 ? file.slice(0, dot) : file
}

export function clock(seconds: number): string {
  const s = Math.max(0, seconds)
  const m = Math.floor(s / 60)
  const rest = s - m * 60
  return `${m}:${rest.toFixed(1).padStart(4, '0')}`
}

function folderName(path: string): string {
  return path.split(/[/\\]/).filter(Boolean).pop() ?? path
}

/** The take's transport, as the status says (null from an older server). */
export function usePlaybackStatus(): PlaybackStatus | null {
  return useStore((s) => s.status?.status.playback ?? null)
}

/** Whether channel `id` hears the take now (virtual soundcheck, assigned). */
export function usePlaybackFed(id: number): string | null {
  return useStore((s) => {
    const pb = s.session?.playback
    if (!pb?.virtual_soundcheck) return null
    return pb.tracks.find((t) => t.channel === id)?.file ?? null
  })
}

/** The top bar's badge: shown while virtual soundcheck is on (loud) or a
 *  take is playing or paused. Opens Patch → Playback. */
export function PlaybackBadge() {
  const loaded = useStore((s) => s.session?.playback.folder ?? null)
  const vsc = useStore((s) => s.session?.playback.virtual_soundcheck === true)
  const state = useStore((s) => s.status?.status.playback?.state ?? 'stopped')
  const position = useStore((s) => Math.floor(s.status?.status.playback?.position ?? 0))
  const error = useStore((s) => s.status?.status.playback?.error ?? null)
  if (!vsc && (!loaded || state === 'stopped')) return null
  return (
    <a
      className={`pb-badge${vsc ? ' vsc' : ''}${error ? ' error' : ''}`}
      href="#patch/playback"
      title={`${vsc ? 'Virtual soundcheck is ON: channels with a track hear the take, not the stage. ' : ''}Take: ${loaded ? folderName(loaded) : '—'} · ${state}${error ? ` · ${error}` : ''}`}
    >
      {vsc ? <span className="pb-badge-vsc">VSC</span> : <ListMusic size={14} />}
      {state === 'playing' ? <Play size={12} /> : state === 'paused' ? <Pause size={12} /> : <Square size={11} />}
      <span className="value">{clock(position).replace(/\.\d$/, '')}</span>
    </a>
  )
}

export function PlaybackPage(props: { session: Session; recording: boolean }) {
  const { session } = props
  const playback = session.playback
  const status = usePlaybackStatus()
  const phase4 = useStore((s) => s.phase4)
  const [takes, setTakes] = useState<Take[] | null>(null)
  const [listing, setListing] = useState(false)
  const [loading, setLoading] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  const [listError, setListError] = useState<string | null>(null)
  const list = async () => {
    setListing(true)
    const reply = await request({ cmd: 'takes' })
    setListing(false)
    if (reply.ok) {
      setTakes(Array.isArray(reply.takes) ? (reply.takes as Take[]) : [])
      setListError(null)
    } else setListError(explain(reply.error))
  }
  // An older server knows no takes: the note above says so once.
  useEffect(() => {
    if (phase4 !== false) void list()
  }, [phase4])
  // A take just recorded shows up when the recorder stops.
  const wasRecording = useRef(props.recording)
  useEffect(() => {
    if (wasRecording.current && !props.recording) void list()
    wasRecording.current = props.recording
  }, [props.recording])

  const load = async (take: Take) => {
    setLoading(take.path)
    const reply = await request({ cmd: 'playback_load', folder: take.path })
    setLoading(null)
    if (!reply.ok) setError(explain(reply.error))
    else {
      setError(null)
      notify(`Loaded ${take.name}`)
    }
  }

  return (
    <div className="patch-page pb-page">
      <header className="patch-head">
        <div>
          <h2>Playback &amp; virtual soundcheck</h2>
          <p>
            Play a take the recorder made back into the channels it came from — before trim and processing, exactly where
            the interface input enters — and mix without the band.
          </p>
        </div>
      </header>
      {phase4 === false && (
        <p className="pb-error">
          <TriangleAlert size={14} /> This LiveStage server predates playback: these controls are refused. Update the
          server.
        </p>
      )}
      {error && (
        <p className="pb-error" role="alert">
          <TriangleAlert size={14} /> {error}
          <button type="button" className="icon-button" aria-label="Dismiss" onClick={() => setError(null)}>
            <X size={13} />
          </button>
        </p>
      )}

      <div className="pb-grid">
        <section className="pb-section pb-takes">
          <div className="pb-section-head">
            <span className="knob-caption">Takes</span>
            <span className="spacer" />
            <button type="button" className="button ghost" disabled={listing} onClick={() => void list()}>
              <RefreshCw size={14} className={listing ? 'spin' : ''} /> Refresh
            </button>
          </div>
          {listError ? (
            <p className="pb-list-error">
              <TriangleAlert size={13} /> {listError}
            </p>
          ) : phase4 === false ? (
            <p className="muted">—</p>
          ) : takes === null ? (
            <p className="muted">Listing the takes…</p>
          ) : takes.length === 0 ? (
            <p className="card-empty">
              No takes in the recordings folder yet. Arm channels on Patch → Record and press REC.
            </p>
          ) : (
            <div className="pb-take-list" role="list">
              {takes.map((take) => {
                const seconds = Math.max(0, ...take.files.map((f) => f.seconds))
                const rate = take.files[0]?.rate
                const current = playback.folder !== null && samePath(playback.folder, take.path)
                return (
                  <div key={take.path} className={`pb-take${current ? ' current' : ''}`} role="listitem">
                    <FolderOpen size={15} className="pb-take-icon" />
                    <div className="pb-take-text">
                      <strong>{take.name}</strong>
                      <span className="value">
                        {clock(seconds).replace(/\.\d$/, '')} · {take.files.length} file{take.files.length === 1 ? '' : 's'}
                        {rate ? ` · ${rate / 1000} kHz` : ''}
                      </span>
                    </div>
                    {current ? (
                      <span className="pb-loaded">Loaded</span>
                    ) : (
                      <button
                        type="button"
                        className="button"
                        disabled={loading !== null || take.files.length === 0}
                        onClick={() => void load(take)}
                      >
                        {loading === take.path ? <LoaderCircle size={14} className="spin" /> : null} Load
                      </button>
                    )}
                  </div>
                )
              })}
            </div>
          )}
        </section>

        <section className="pb-section pb-loaded-take">
          {playback.folder === null ? (
            <p className="card-empty">No take loaded. Load one on the left.</p>
          ) : (
            <LoadedTake session={session} status={status} takes={takes} />
          )}
        </section>
      </div>
    </div>
  )
}

function samePath(a: string, b: string): boolean {
  return a.replace(/\\/g, '/').replace(/\/+$/, '') === b.replace(/\\/g, '/').replace(/\/+$/, '')
}

function LoadedTake(props: { session: Session; status: PlaybackStatus | null; takes: Take[] | null }) {
  const { session, status } = props
  const playback = session.playback
  const vsc = playback.virtual_soundcheck
  const state = status?.state ?? 'stopped'
  const duration = status?.duration ?? 0
  const assigned = playback.tracks.filter((t) => t.channel !== null).length
  const take = props.takes?.find((t) => samePath(t.path, playback.folder ?? ''))
  // The slider follows the position, except while it is held.
  const [scrub, setScrub] = useState<number | null>(null)
  const position = scrub ?? status?.position ?? 0
  const locate = (seconds: number) => act({ cmd: 'playback_locate', seconds })

  return (
    <>
      <div className="pb-section-head">
        <span className="knob-caption">Loaded</span>
        <strong className="pb-take-name" title={playback.folder ?? ''}>
          {folderName(playback.folder ?? '')}
        </strong>
        <span className="spacer" />
        <button type="button" className="button ghost" onClick={() => act({ cmd: 'playback_unload' })}>
          <X size={14} /> Unload
        </button>
      </div>

      <div className={`pb-vsc${vsc ? ' on' : ''}`}>
        <button
          type="button"
          className={`pb-vsc-switch${vsc ? ' on' : ''}`}
          role="switch"
          aria-checked={vsc}
          disabled={!vsc && assigned === 0}
          title={assigned === 0 ? 'Assign a track to a channel first' : undefined}
          onClick={() => act({ cmd: 'set_virtual_soundcheck', on: !vsc })}
        >
          <AudioWaveform size={16} />
          Virtual soundcheck {vsc ? 'ON' : 'off'}
        </button>
        <span className="pb-vsc-text">
          {vsc
            ? `${assigned} channel${assigned === 1 ? '' : 's'} hear the take instead of the stage (marked PB on the mixer). The others stay live.`
            : 'Off: every channel hears its interface input. On: channels with a track below hear the take instead.'}
        </span>
      </div>

      <div className="pb-transport">
        <button
          type="button"
          className={`button icon-only${state === 'playing' ? ' on' : ''}`}
          title={state === 'playing' ? 'Pause' : 'Play'}
          aria-label={state === 'playing' ? 'Pause' : 'Play'}
          onClick={() => act({ cmd: 'playback', action: state === 'playing' ? 'pause' : 'play' })}
        >
          {state === 'playing' ? <Pause size={15} /> : <Play size={15} />}
        </button>
        <button
          type="button"
          className="button icon-only"
          title="Stop (back to the start)"
          aria-label="Stop"
          onClick={() => act({ cmd: 'playback', action: 'stop' })}
        >
          <Square size={13} />
        </button>
        <span className="value pb-time">{clock(position)}</span>
        <input
          type="range"
          className="pb-position"
          min={0}
          max={Math.max(duration, 0.1)}
          step={0.1}
          value={Math.min(position, Math.max(duration, 0.1))}
          aria-label="Position (drag to locate)"
          onChange={(e) => setScrub(Number(e.currentTarget.value))}
          onPointerUp={(e) => {
            locate(Number(e.currentTarget.value))
            window.setTimeout(() => setScrub(null), 400)
          }}
          onKeyUp={(e) => {
            locate(Number(e.currentTarget.value))
            window.setTimeout(() => setScrub(null), 400)
          }}
        />
        <span className="value pb-time muted">{clock(duration)}</span>
        <button
          type="button"
          className={`pill pb-loop${status?.loop ? ' on' : ''}`}
          aria-pressed={status?.loop === true}
          title={status?.loop === undefined ? 'Loop the whole take (the server does not report whether it is on)' : 'Loop the whole take'}
          onClick={() => act({ cmd: 'playback_loop', on: !(status?.loop ?? false) })}
        >
          <Repeat size={11} /> LOOP
        </button>
      </div>
      <div className="pb-state">
        <span className={`pb-state-text ${state}`}>{state === 'playing' ? 'Playing' : state === 'paused' ? 'Paused' : 'Stopped'}</span>
        {(status?.underruns ?? 0) > 0 && (
          <span className="pb-warn" title="The disk did not keep up: silence was played for a moment">
            <TriangleAlert size={12} /> {status?.underruns} underrun{status?.underruns === 1 ? '' : 's'}
          </span>
        )}
        {status?.error && (
          <span className="pb-warn error">
            <TriangleAlert size={12} /> {status.error}
          </span>
        )}
      </div>

      <table className="record-table pb-tracks">
        <thead>
          <tr>
            <th>File</th>
            <th>Plays into</th>
          </tr>
        </thead>
        <tbody>
          {playback.tracks.map((track) => {
            const channel = session.channels.find((c) => c.id === track.channel)
            const auto = channel && fileStem(channel.name) === stemOf(track.file)
            const info = take?.files.find((f) => f.name === track.file)
            return (
              <tr key={track.file} className={channel ? (vsc ? 'pb-live' : '') : 'pb-unassigned'}>
                <td>
                  <span className="pb-file">{track.file}</span>
                  <span className="muted value pb-file-meta">
                    {' '}
                    {track.channels === 1 ? 'mono' : track.channels === 2 ? 'stereo' : `${track.channels} ch`}
                    {info ? ` · ${clock(info.seconds).replace(/\.\d$/, '')}` : ''}
                  </span>
                </td>
                <td>
                  <div className="pb-assign">
                    <Select
                      value={track.channel === null ? '' : String(track.channel)}
                      className={track.channel === null ? 'unset' : ''}
                      onChange={(v) => act({ cmd: 'playback_assign', file: track.file, channel: v === '' ? null : Number(v) })}
                    >
                      <option value="">Not played</option>
                      {session.channels.map((c, i) => (
                        <option key={c.id} value={c.id}>
                          {i + 1} · {c.name}
                        </option>
                      ))}
                    </Select>
                    {auto && <span className="pb-auto" title="Matched by name, as the recorder named the file">by name</span>}
                    {vsc && channel && <span className="pb-chip">PB</span>}
                  </div>
                </td>
              </tr>
            )
          })}
        </tbody>
      </table>
      <p className="pe-note">
        A mono channel takes a stereo file as (L+R)/2; a stereo channel takes a mono file on both sides. While stopped or
        paused, a channel in virtual soundcheck is silent. Recording while playing back records whatever each channel's
        input is.
      </p>
    </>
  )
}
