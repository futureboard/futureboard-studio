import { useCallback, useEffect, useState } from 'react'
import {
  AudioLines,
  Cable,
  CircleAlert,
  CircleCheck,
  Cpu,
  LoaderCircle,
  Save,
  Settings2,
  SlidersVertical,
  TriangleAlert,
  WifiOff,
} from 'lucide-react'
import { EffectPicker, InsertEditor } from './Inserts.tsx'
import type { InsertTarget } from './Mixer.tsx'
import { Mixer } from './Mixer.tsx'
import { PATCH_TABS, Patch } from './Patch.tsx'
import type { PatchTab } from './Patch.tsx'
import type { Session, StripRef } from './protocol.ts'
import { Setup } from './Setup.tsx'
import { act, notify, request, useStore } from './store.ts'

type View = 'mixer' | 'patch' | 'setup'

const VIEWS: { id: View; label: string; icon: typeof SlidersVertical }[] = [
  { id: 'mixer', label: 'Mixer', icon: SlidersVertical },
  { id: 'patch', label: 'Patch', icon: Cable },
  { id: 'setup', label: 'Setup', icon: Settings2 },
]

function viewFromHash(): View {
  const page = location.hash.slice(1).split('/')[0]
  return VIEWS.some((v) => v.id === page) ? (page as View) : 'mixer'
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

export function App() {
  const connection = useStore((s) => s.connection)
  const session = useStore((s) => s.session)
  const hello = useStore((s) => s.hello)
  const status = useStore((s) => s.status)
  const notice = useStore((s) => s.notice)
  // The page lives in the URL, so a reload or a tablet's bookmark lands on it.
  const [view, setViewState] = useState<View>(() => viewFromHash())
  const [patchTab, setPatchTabState] = useState<PatchTab>(() => patchTabFromHash())
  const setView = useCallback(
    (next: View) => {
      setViewState(next)
      const hash = next === 'patch' && patchTab !== 'inputs' ? `#patch/${patchTab}` : `#${next}`
      history.replaceState(null, '', next === 'mixer' ? location.pathname : hash)
    },
    [patchTab],
  )
  const setPatchTab = useCallback((tab: PatchTab) => {
    setPatchTabState(tab)
    history.replaceState(null, '', tab === 'inputs' ? '#patch' : `#patch/${tab}`)
  }, [])
  const [editing, setEditing] = useState<InsertTarget | null>(null)
  const [adding, setAdding] = useState<StripRef | null>(null)
  const [noticeVisible, setNoticeVisible] = useState(false)
  // A changed hash (a link, the back button) moves the page, and closes an
  // editor the hash no longer names; the effect below opens the one it does.
  const [hashInsert, setHashInsert] = useState(() => insertFromHash())
  useEffect(() => {
    const onHash = () => {
      setViewState(viewFromHash())
      if (viewFromHash() === 'patch') setPatchTabState(patchTabFromHash())
      setHashInsert(insertFromHash())
      if (insertFromHash() === null) setEditing(null)
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
    if (insertFromHash() !== null) history.replaceState(null, '', location.pathname)
  }, [])
  useEffect(() => {
    if (!session || hashInsert === null || editing?.insert === hashInsert) return
    const target = findInsert(session, hashInsert)
    if (target) setEditing(target)
  }, [session, editing, hashInsert])
  const closePicker = useCallback(() => setAdding(null), [])

  useEffect(() => {
    if (!notice) return
    setNoticeVisible(true)
    const timer = window.setTimeout(() => setNoticeVisible(false), notice.error ? 6000 : 3500)
    return () => window.clearTimeout(timer)
  }, [notice])

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

  const save = async () => {
    const reply = await request({ cmd: 'save' })
    if (reply.ok) notify(`Saved ${String(reply.path)}`)
    else notify(reply.error ?? 'not saved', true)
  }

  return (
    <div className="app">
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
            <button key={id} type="button" className={view === id ? 'on' : ''} onClick={() => setView(id)}>
              <Icon size={15} />
              <span>{label}</span>
            </button>
          ))}
        </nav>

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
          <span className={`status-pill link link-${connection}`} title={`Connection: ${connection}`}>
            <span className="link-dot" />
            {connection === 'open' ? 'Live' : connection === 'connecting' ? 'Connecting' : 'Offline'}
          </span>
        </div>

        <div className="toolbar-actions">
          <button
            type="button"
            className="button icon-only"
            disabled={!hello?.session_path}
            title={hello?.session_path ? `Save to ${hello.session_path}` : 'The server was started without --session'}
            onClick={() => void save()}
          >
            <Save size={15} />
          </button>
          <button
            type="button"
            className={`rec${recording ? ' on' : ''}`}
            onClick={() => void toggleRecording()}
            title={recording ? 'Stop recording' : 'Record the armed strips'}
          >
            <span className="rec-dot" />
            <span className="rec-label">{recording ? clock(engine?.recording_seconds ?? 0) : 'REC'}</span>
            {recording && (engine?.recording_dropped ?? 0) > 0 && (
              <span className="rec-dropped" title="Samples lost: the disk could not keep up">
                <TriangleAlert size={12} />
              </span>
            )}
          </button>
        </div>
      </header>

      <main className="content">
        {!session ? (
          <div className="empty">
            <LoaderCircle size={22} className="spin" />
            {connection === 'open' ? 'Waiting for the session…' : 'Connecting to the LiveStage server…'}
          </div>
        ) : view === 'mixer' ? (
          <Mixer
            session={session}
            inputs={engine?.in_channels ?? 0}
            onOpenInsert={openEditor}
            onAddEffect={setAdding}
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
      {notice && noticeVisible && (
        <div className={`toast${notice.error ? ' error' : ''}`} role="status">
          {notice.error ? <CircleAlert size={15} /> : <CircleCheck size={15} />}
          <span>{notice.text}</span>
        </div>
      )}
      {session && editing && <InsertEditor session={session} target={editing} onClose={closeEditor} />}
      {session && adding && <EffectPicker session={session} strip={adding} onClose={closePicker} />}
    </div>
  )
}
