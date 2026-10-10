import { useCallback, useEffect, useRef, useState } from 'react'
import {
  AudioLines,
  Cable,
  CircleAlert,
  CircleCheck,
  Clapperboard,
  Cpu,
  Eye,
  Headphones,
  LoaderCircle,
  Settings2,
  SlidersVertical,
  TriangleAlert,
  WifiOff,
} from 'lucide-react'
import { HistoryButtons, SaveControls, useUndoKeys } from './EditBar.tsx'
import { EffectPicker, InsertEditor } from './Inserts.tsx'
import { LockButton, LockOverlay, UserChip } from './Lock.tsx'
import { LoginScreen } from './Login.tsx'
import type { InsertTarget } from './Mixer.tsx'
import { Mixer } from './Mixer.tsx'
import { MyMixPage, mixFromHash, mixHash } from './MyMix.tsx'
import { PATCH_TABS, Patch } from './Patch.tsx'
import type { PatchTab } from './Patch.tsx'
import { PlaybackBadge } from './Playback.tsx'
import type { MixRef, Session, StripRef } from './protocol.ts'
import { stripKey } from './protocol.ts'
import type { Bank, SofTarget } from './routing.ts'
import { bankFromKey, bankKey, stripExists } from './routing.ts'
import { SelectedChannel } from './SelectedChannel.tsx'
import { SceneBar, ScenesPage } from './Scenes.tsx'
import { Setup, useRole } from './Setup.tsx'
import type { Notice } from './store.ts'
import { act, notify, request, useStore } from './store.ts'
import { TalkbackButton, TalkbackPanel } from './Talkback.tsx'
import { WorkflowDialogs } from './Workflow.tsx'

const BANK_KEY = 'livestage.bank'
/** How long a layer just added may take to appear in the session. */
const LAYER_WAIT_MS = 3000

/** The bank this device last showed (a per-device convenience). */
function savedBank(): Bank {
  try {
    return bankFromKey(localStorage.getItem(BANK_KEY)) ?? 'inputs'
  } catch {
    return 'inputs'
  }
}

/** Whether a key press belongs to a text field or an open dialog. */
function typingOrModal(e: KeyboardEvent): boolean {
  const target = e.target as HTMLElement | null
  const tag = target?.tagName
  if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || target?.isContentEditable) return true
  return document.querySelector('.modal-backdrop') !== null
}

type View = 'mixer' | 'mix' | 'patch' | 'scenes' | 'setup'

const VIEWS: { id: View; label: string; icon: typeof SlidersVertical }[] = [
  { id: 'mixer', label: 'Mixer', icon: SlidersVertical },
  // The personal monitor mix (Phase 4), as a musician sees it on a phone.
  { id: 'mix', label: 'My mix', icon: Headphones },
  { id: 'patch', label: 'Patch', icon: Cable },
  { id: 'scenes', label: 'Scenes', icon: Clapperboard },
  { id: 'setup', label: 'Setup', icon: Settings2 },
]

function viewFromHash(): View {
  const page = location.hash.slice(1).split('/')[0]
  return VIEWS.some((v) => v.id === page) ? (page as View) : 'mixer'
}

/** The Selected Channel's address (channelHash's inverse). */
function channelFromHash(): StripRef | null {
  const match = /^#channel\/(?:(channel|bus|matrix)\/(\d+)|master)$/.exec(location.hash)
  if (!match) return null
  return match[1] ? { kind: match[1] as 'channel' | 'bus' | 'matrix', id: Number(match[2]) } : { kind: 'master' }
}

/** `#channel/channel/3`, `#channel/bus/7`, `#channel/master`. */
function channelHash(strip: StripRef): string {
  return strip.kind === 'master' ? '#channel/master' : `#channel/${strip.kind}/${strip.id}`
}

/** `#patch/outputs`: the patch page's tab; `#patch` is its first. */
function patchTabFromHash(): PatchTab {
  const [page, tab] = location.hash.slice(1).split('/')
  return page === 'patch' && PATCH_TABS.includes(tab as PatchTab) ? (tab as PatchTab) : 'inputs'
}

/** `#insert/<id>`: an insert's editor, open over the mixer. */
function insertFromHash(): number | null {
  const match = /^#insert\/(\d+)$/.exec(location.hash)
  return match ? Number(match[1]) : null
}

function findInsert(session: Session, insert: number): InsertTarget | null {
  for (const channel of session.channels) {
    if (channel.inserts.some((s) => s.id === insert)) return { strip: { kind: 'channel', id: channel.id }, insert }
  }
  for (const bus of session.buses) {
    if (bus.inserts.some((s) => s.id === insert)) return { strip: { kind: 'bus', id: bus.id }, insert }
  }
  for (const matrix of session.matrices) {
    if (matrix.inserts.some((s) => s.id === insert)) return { strip: { kind: 'matrix', id: matrix.id }, insert }
  }
  return session.master.inserts.some((s) => s.id === insert) ? { strip: { kind: 'master' }, insert } : null
}

function clock(seconds: number): string {
  const s = Math.floor(seconds)
  const hh = Math.floor(s / 3600)
  const mm = Math.floor((s % 3600) / 60)
  const ss = s % 60
  const two = (n: number) => n.toString().padStart(2, '0')
  return `${hh > 0 ? `${hh}:` : ''}${two(mm)}:${two(ss)}`
}

/** Who is at this page decides what it is: the login, a musician's own
 *  mixes, or the console. */
export function App() {
  const auth = useStore((s) => s.auth)
  const notice = useStore((s) => s.notice)
  if (auth?.mode === 'users' && !auth.user) {
    return (
      <>
        <LoginScreen />
        <Toast notice={notice} />
      </>
    )
  }
  if (auth?.user?.role === 'musician') return <MusicianApp />
  return <ConsoleApp />
}

/** The server's latest notice, for a few seconds. */
function Toast(props: { notice: Notice | null }) {
  const { notice } = props
  const [visible, setVisible] = useState(false)
  useEffect(() => {
    if (!notice) return
    setVisible(true)
    const timer = window.setTimeout(() => setVisible(false), notice.error ? 6000 : 3500)
    return () => window.clearTimeout(timer)
  }, [notice])
  if (!notice || !visible) return null
  return (
    <div className={`toast${notice.error ? ' error' : ''}`} role="status">
      {notice.error ? <CircleAlert size={15} /> : <CircleCheck size={15} />}
      <span>{notice.text}</span>
    </div>
  )
}

/** The `#mix` address, kept in step with the page. */
function useMixAddress(): [MixRef | null, (mix: MixRef | null) => void] {
  const [mix, setMixState] = useState<MixRef | null>(() => mixFromHash())
  useEffect(() => {
    const onHash = () => setMixState(mixFromHash())
    window.addEventListener('hashchange', onHash)
    return () => window.removeEventListener('hashchange', onHash)
  }, [])
  const setMix = useCallback((next: MixRef | null) => {
    setMixState(next)
    history.replaceState(null, '', mixHash(next))
  }, [])
  return [mix, setMix]
}

function ConnectionPill() {
  const connection = useStore((s) => s.connection)
  return (
    <span className={`status-pill link link-${connection}`} title={`Connection: ${connection}`}>
      <span className="link-dot" />
      {connection === 'open' ? 'Live' : connection === 'connecting' ? 'Connecting' : 'Offline'}
    </span>
  )
}

/** A musician: their mixes only, with lock and log out. */
function MusicianApp() {
  const connection = useStore((s) => s.connection)
  const session = useStore((s) => s.session)
  const own = useStore((s) => s.auth?.user?.mixes ?? null)
  const notice = useStore((s) => s.notice)
  const [mix, setMix] = useMixAddress()
  // A musician lands on #mix whatever the address was.
  useEffect(() => {
    if (!location.hash.startsWith('#mix')) history.replaceState(null, '', mixHash(mix))
  }, [mix])
  useEffect(() => {
    document.title = session ? `My mix — ${session.name}` : 'LiveStage'
  }, [session])
  return (
    <div className="app musician">
      <header className="toolbar">
        <div className="brand">
          <span className="brand-mark">
            <Headphones size={16} strokeWidth={2.25} />
          </span>
          <div className="brand-text">
            <strong>My mix</strong>
            <span>{session?.name ?? '—'}</span>
          </div>
        </div>
        <span className="spacer" />
        <ConnectionPill />
        <LockButton />
        <UserChip />
      </header>
      <main className="content">
        {!session ? (
          <div className="empty">
            <LoaderCircle size={22} className="spin" />
            {connection === 'open' ? 'Waiting for the session…' : 'Connecting to the LiveStage server…'}
          </div>
        ) : (
          <MyMixPage session={session} mix={mix} onMix={setMix} own={own ?? []} readOnly={false} />
        )}
      </main>
      {connection !== 'open' && session && (
        <div className="offline mm-offline">
          <div className="offline-card">
            <WifiOff size={22} />
            <strong>Connection lost</strong>
            <span>Reconnecting. Your mix is paused until the console answers.</span>
          </div>
        </div>
      )}
      <Toast notice={notice} />
      <LockOverlay />
    </div>
  )
}

function ConsoleApp() {
  const connection = useStore((s) => s.connection)
  const session = useStore((s) => s.session)
  const status = useStore((s) => s.status)
  const notice = useStore((s) => s.notice)
  const role = useRole()
  const viewOnly = role === 'viewer'
  const [mix, setMix] = useMixAddress()
  // The page lives in the URL, so a reload or a tablet's bookmark lands on it.
  const [view, setViewState] = useState<View>(() => viewFromHash())
  const [patchTab, setPatchTabState] = useState<PatchTab>(() => patchTabFromHash())
  const setView = useCallback(
    (next: View) => {
      setViewState(next)
      setSelected(null)
      const hash =
        next === 'patch' && patchTab !== 'inputs' ? `#patch/${patchTab}` : next === 'mix' ? mixHash(mix) : `#${next}`
      history.replaceState(null, '', next === 'mixer' ? location.pathname : hash)
    },
    [patchTab, mix],
  )
  const setPatchTab = useCallback((tab: PatchTab) => {
    setPatchTabState(tab)
    history.replaceState(null, '', tab === 'inputs' ? '#patch' : `#patch/${tab}`)
  }, [])
  // The Selected Channel, open in place of the mixer; the mixer's layer and
  // spill outlive it.
  const [selected, setSelected] = useState<StripRef | null>(() => channelFromHash())
  const selectedRef = useRef(selected)
  selectedRef.current = selected
  const [bank, setBankState] = useState<Bank>(() => savedBank())
  // A layer just added is shown before the server's session has it.
  const awaitingLayer = useRef<{ layer: number; at: number } | null>(null)
  const setBank = useCallback((next: Bank) => {
    awaitingLayer.current = typeof next === 'string' ? null : { layer: next.layer, at: performance.now() }
    setBankState(next)
    // A bank is chosen: a DCA's spill gives way to it.
    setSpill(null)
    try {
      localStorage.setItem(BANK_KEY, bankKey(next))
    } catch {
      // Blocked storage: the bank lasts this page only.
    }
  }, [])
  const [spill, setSpill] = useState<number | null>(null)
  // Sends on Fader: the bus (or matrix) whose sends the faders are. It
  // outlives a bank change, so a monitor mix can be built bank by bank.
  const [sof, setSof] = useState<SofTarget | null>(null)
  const [talkOpen, setTalkOpen] = useState(false)
  const openChannel = useCallback((strip: StripRef) => {
    setSelected(strip)
    setViewState('mixer')
    history.replaceState(null, '', channelHash(strip))
  }, [])
  const closeChannel = useCallback(() => {
    setSelected(null)
    if (channelFromHash() !== null) history.replaceState(null, '', location.pathname)
  }, [])
  const [editing, setEditing] = useState<InsertTarget | null>(null)
  const [adding, setAdding] = useState<StripRef | null>(null)
  // A changed hash (a link, the back button) moves the page, and closes an
  // editor the hash no longer names; the effect below opens the one it does.
  const [hashInsert, setHashInsert] = useState(() => insertFromHash())
  useEffect(() => {
    const onHash = () => {
      setViewState(viewFromHash())
      if (viewFromHash() === 'patch') setPatchTabState(patchTabFromHash())
      setHashInsert(insertFromHash())
      if (insertFromHash() === null) {
        setEditing(null)
        setSelected(channelFromHash())
      }
    }
    window.addEventListener('hashchange', onHash)
    return () => window.removeEventListener('hashchange', onHash)
  }, [])
  // An open editor is in the URL too: a tablet can keep a plug-in window as
  // a bookmark.
  const openEditor = useCallback((target: InsertTarget) => {
    setEditing(target)
    setHashInsert(target.insert)
    history.replaceState(null, '', `#insert/${target.insert}`)
  }, [])
  const closeEditor = useCallback(() => {
    setEditing(null)
    setHashInsert(null)
    // Back to the Selected Channel it was opened from, if one is open.
    const channel = selectedRef.current
    if (insertFromHash() !== null) history.replaceState(null, '', channel ? channelHash(channel) : location.pathname)
  }, [])
  useEffect(() => {
    if (!session || hashInsert === null || editing?.insert === hashInsert) return
    const target = findInsert(session, hashInsert)
    if (target) setEditing(target)
  }, [session, editing, hashInsert])
  const closePicker = useCallback(() => setAdding(null), [])
  const openScenes = useCallback(() => setView('scenes'), [setView])
  useUndoKeys()

  // From a Selected Channel: back to the mixer, its faders on that bus.
  const sofFromChannel = useCallback(
    (target: SofTarget) => {
      setSof(target)
      closeChannel()
    },
    [closeChannel],
  )

  // A removed bus, matrix or layer takes its SoF or bank with it.
  useEffect(() => {
    if (!session) return
    if (sof && !stripExists(session, sof)) setSof(null)
    if (typeof bank !== 'string' && bank.layer >= session.layers.length) {
      const awaiting = awaitingLayer.current
      if (awaiting?.layer === bank.layer && performance.now() - awaiting.at < LAYER_WAIT_MS) {
        // Not there yet: look again once the server has had its chance.
        const timer = window.setTimeout(() => setBankState((b) => (typeof b === 'string' ? b : { ...b })), LAYER_WAIT_MS)
        return () => window.clearTimeout(timer)
      }
      setBankState('inputs')
    }
  }, [session, sof, bank])

  // Escape: the talkback panel first, then Sends on Fader (never while
  // typing, and never past an open dialog, which takes its own Escape).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape' || e.defaultPrevented || typingOrModal(e)) return
      if (talkOpen) setTalkOpen(false)
      else if (sof) setSof(null)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [talkOpen, sof])

  useEffect(() => {
    document.title = session ? `${session.name} — LiveStage` : 'LiveStage'
  }, [session])

  const engine = status?.status
  const recording = status?.recording ?? false
  const load = Math.min(1, engine?.load ?? 0)

  const toggleRecording = async () => {
    if (!recording) {
      act({ cmd: 'start_recording' })
      return
    }
    const reply = await request({ cmd: 'stop_recording' })
    if (reply.ok) {
      const files = (reply.files as string[] | undefined)?.length ?? 0
      notify(`Recorded ${files} file${files === 1 ? '' : 's'} to ${String(reply.folder)}`)
    } else {
      notify(reply.error ?? 'could not stop', true)
    }
  }

  return (
    <div className={`app${viewOnly ? ' view-only' : ''}`}>
      <header className="toolbar">
        <div className="brand">
          <span className="brand-mark">
            <AudioLines size={16} strokeWidth={2.25} />
          </span>
          <div className="brand-text">
            <strong>LiveStage</strong>
            <span>{session?.name ?? '—'}</span>
          </div>
        </div>

        <nav className="views" aria-label="Pages">
          {VIEWS.map(({ id, label, icon: Icon }) => (
            <button key={id} type="button" className={view === id ? 'on' : ''} title={label} onClick={() => setView(id)}>
              <Icon size={15} />
              <span>{label}</span>
            </button>
          ))}
        </nav>

        <div className="toolbar-scene">{session && <SceneBar session={session} onOpen={openScenes} />}</div>

        <div className="toolbar-status">
          {engine &&
            (engine.error ? (
              <span className="status-pill error" title={engine.error}>
                <CircleAlert size={13} />
                <span className="status-text">{engine.error}</span>
              </span>
            ) : (
              <>
                <span className="status-pill" title={engine.output_device ?? ''}>
                  <AudioLines size={13} />
                  {engine.sample_rate / 1000} kHz · {engine.in_channels} in · {engine.out_channels} out
                </span>
                <span className="status-pill" title="Audio thread load">
                  <Cpu size={13} />
                  <span className={`load-bar${load > 0.8 ? ' hot' : load > 0.5 ? ' warm' : ''}`}>
                    <span style={{ width: `${Math.max(3, load * 100)}%` }} />
                  </span>
                  {Math.round(load * 100)}%
                </span>
                {engine.input_underruns > 0 && (
                  <span className="status-pill warn" title="Input dropouts since the device opened">
                    <TriangleAlert size={13} />
                    {engine.input_underruns}
                  </span>
                )}
              </>
            ))}
          <ConnectionPill />
        </div>

        <div className="toolbar-edit">
          <HistoryButtons />
          <SaveControls />
        </div>

        <div className="toolbar-actions">
          <PlaybackBadge />
          {session && <TalkbackButton open={talkOpen} onToggle={() => setTalkOpen(!talkOpen)} />}
          <button
            type="button"
            className={`rec${recording ? ' on' : ''}`}
            disabled={viewOnly}
            onClick={() => void toggleRecording()}
            title={viewOnly ? 'View only: a viewer cannot record' : recording ? 'Stop recording' : 'Record the armed strips'}
          >
            <span className="rec-dot" />
            <span className="rec-label">{recording ? clock(engine?.recording_seconds ?? 0) : 'REC'}</span>
            {recording && (engine?.recording_dropped ?? 0) > 0 && (
              <span className="rec-dropped" title="Samples lost: the disk could not keep up">
                <TriangleAlert size={12} />
              </span>
            )}
          </button>
          {viewOnly && (
            <span className="status-pill view-only-pill" title="Signed in as a viewer: everything is shown, nothing can be changed">
              <Eye size={13} /> View only
            </span>
          )}
          <LockButton />
          <UserChip />
        </div>
      </header>

      <main className="content">
        {!session ? (
          <div className="empty">
            <LoaderCircle size={22} className="spin" />
            {connection === 'open' ? 'Waiting for the session…' : 'Connecting to the LiveStage server…'}
          </div>
        ) : view === 'mixer' && selected ? (
          <SelectedChannel
            key={stripKey(selected)}
            session={session}
            strip={selected}
            inputs={engine?.in_channels ?? 0}
            sampleRate={engine?.sample_rate ?? 0}
            onClose={closeChannel}
            onSelect={openChannel}
            onOpenInsert={openEditor}
            onAddEffect={setAdding}
            onSof={sofFromChannel}
          />
        ) : view === 'mixer' ? (
          <Mixer
            session={session}
            inputs={engine?.in_channels ?? 0}
            onOpenInsert={openEditor}
            onAddEffect={setAdding}
            onSelect={openChannel}
            bank={bank}
            onBank={setBank}
            spill={spill}
            onSpill={setSpill}
            sof={sof}
            onSof={setSof}
          />
        ) : view === 'patch' ? (
          <Patch
            session={session}
            inputs={engine?.in_channels ?? 0}
            outputs={engine?.out_channels ?? 2}
            recording={recording}
            tab={patchTab}
            onTab={setPatchTab}
          />
        ) : view === 'mix' ? (
          <MyMixPage session={session} mix={mix} onMix={setMix} own={null} readOnly={viewOnly} />
        ) : view === 'scenes' ? (
          <ScenesPage session={session} />
        ) : (
          <Setup session={session} />
        )}
      </main>

      {connection !== 'open' && session && (
        <div className="offline">
          <div className="offline-card">
            <WifiOff size={22} />
            <strong>Connection lost</strong>
            <span>Reconnecting. Controls are paused until the server answers.</span>
          </div>
        </div>
      )}
      <Toast notice={notice} />
      {session && editing && <InsertEditor session={session} target={editing} onClose={closeEditor} />}
      {session && adding && <EffectPicker session={session} strip={adding} onClose={closePicker} />}
      {session && <WorkflowDialogs session={session} />}
      {session && talkOpen && (
        <TalkbackPanel session={session} inputs={engine?.in_channels ?? 0} onClose={() => setTalkOpen(false)} />
      )}
      <LockOverlay />
    </div>
  )
}
