// The interface and the recorder. Device changes are drafted here and sent
// with Apply: reopening the device interrupts the sound, so it never happens
// on a stray click. Phase 4: users and remote control (admin), and what an
// engineer or a viewer may not touch shown disabled with the reason.

import { useEffect, useState } from 'react'
import type { ReactNode } from 'react'
import { AudioLines, Check, CircleAlert, Disc3, FolderOpen, Mic, RefreshCw, ShieldCheck, Speaker, Undo2 } from 'lucide-react'
import { Select } from './controls.tsx'
import { RemoteCard } from './Remote.tsx'
import { StorageCard } from './Storage.tsx'
import { UsersCard } from './Users.tsx'
import type { AudioSettings, Devices, RecordSettings, Role, Session } from './protocol.ts'
import { act, notify, request, useStore } from './store.ts'

/** This page's role: open mode (or a server without users) is an admin. */
export function useRole(): Role {
  return useStore((s) => (s.auth?.mode === 'users' ? (s.auth.user?.role ?? 'viewer') : 'admin'))
}

/** Controls a role may not use: disabled, with the reason above them. */
function Gate(props: { allowed: boolean; reason: string; children: ReactNode }) {
  if (props.allowed) return <>{props.children}</>
  return (
    <fieldset className="role-gate" disabled>
      <p className="role-gate-note">
        <ShieldCheck size={13} /> {props.reason}
      </p>
      {props.children}
    </fieldset>
  )
}

const RATES = [0, 44100, 48000, 88200, 96000]
const BUFFERS = [64, 128, 256, 512, 1024]

function Segments<T extends string | number>(props: {
  value: T
  options: [T, string][]
  onChange: (value: T) => void
}) {
  return (
    <div className="segments">
      {props.options.map(([value, label]) => (
        <button
          key={String(value)}
          type="button"
          className={value === props.value ? 'on' : ''}
          onClick={() => props.onChange(value)}
        >
          {label}
        </button>
      ))}
    </div>
  )
}

function Field(props: { label: string; hint?: string; children: ReactNode }) {
  return (
    <div className="field">
      <div className="field-label">
        <span>{props.label}</span>
        {props.hint && <span className="field-hint">{props.hint}</span>}
      </div>
      <div className="field-control">{props.children}</div>
    </div>
  )
}

export function Setup(props: { session: Session }) {
  const status = useStore((s) => s.status?.status)
  const isRecording = useStore((s) => s.status?.recording ?? false)
  const storage = useStore((s) => s.storage)
  const [draft, setDraft] = useState<AudioSettings>(props.session.audio)
  const [devices, setDevices] = useState<Devices | null>(null)
  const [scanning, setScanning] = useState(false)
  const role = useRole()
  // An older server sends no `auth`: it has no users or remote control.
  const phase4 = useStore((s) => s.auth !== null)
  const admin = role === 'admin'
  const viewer = role === 'viewer'
  const adminOnly = viewer ? 'View only: a viewer changes nothing.' : 'Admin only: an engineer cannot change this.'

  const scan = async (host: string | null) => {
    setScanning(true)
    const reply = await request({ cmd: 'devices', host })
    setScanning(false)
    if (reply.ok) setDevices(reply.devices as Devices)
    else notify(reply.error ?? 'could not list devices', true)
  }

  useEffect(() => {
    // Once on open; Rescan after that.
    void scan(props.session.audio.host)
  }, [])

  const changed = JSON.stringify(draft) !== JSON.stringify(props.session.audio)
  const recording = props.session.recording
  const setRecording = (patch: Partial<RecordSettings>) =>
    act({ cmd: 'set_record_settings', settings: { ...recording, ...patch } })

  const deviceSelect = (which: 'input_device' | 'output_device', list: Devices['inputs'] | undefined) => (
    <Select
      value={draft[which] ?? ''}
      icon={which === 'input_device' ? <Mic size={13} /> : <Speaker size={13} />}
      className="wide"
      onChange={(value) => setDraft({ ...draft, [which]: value || null })}
    >
      <option value="">System default</option>
      {(list ?? []).map((d) => (
        <option key={d.name} value={d.name}>
          {d.name} — {d.channels} ch{d.default ? ' (default)' : ''}
        </option>
      ))}
      {draft[which] && !(list ?? []).some((d) => d.name === draft[which]) && (
        <option value={draft[which] ?? ''}>{draft[which]} (not found)</option>
      )}
    </Select>
  )

  return (
    <div className="page">
      <Gate allowed={admin} reason={adminOnly}>
        <section className="card">
          <header className="card-head">
            <AudioLines size={16} />
            <div>
              <h2>Audio interface</h2>
              <p>Changes take effect on Apply; the sound stops for a moment while the device reopens.</p>
            </div>
          </header>
          {status && (
            <div className={`device-status${status.error ? ' error' : ''}`}>
              {status.error ? <CircleAlert size={15} /> : <Check size={15} />}
              <span>
                {status.error ??
                  `${status.output_device ?? 'No output'}${status.input_device ? ` · in: ${status.input_device}` : ''}`}
              </span>
              {!status.error && (
                <span className="device-numbers">
                  {status.sample_rate / 1000} kHz · {status.in_channels} in / {status.out_channels} out
                </span>
              )}
            </div>
          )}
          <Field label="Audio system">
            <Segments
              value={draft.host ?? ''}
              options={[['', 'Default'], ...(devices?.hosts ?? []).map((h): [string, string] => [h, h])]}
              onChange={(host) => {
                setDraft({ ...draft, host: host || null })
                void scan(host || null)
              }}
            />
          </Field>
          <Field label="Input">{deviceSelect('input_device', devices?.inputs)}</Field>
          <Field label="Output">{deviceSelect('output_device', devices?.outputs)}</Field>
          <Field label="Sample rate">
            <Segments
              value={draft.sample_rate}
              options={RATES.map((r): [number, string] => [r, r === 0 ? 'Device' : `${r / 1000} kHz`])}
              onChange={(sample_rate) => setDraft({ ...draft, sample_rate })}
            />
          </Field>
          <Field label="Buffer" hint="frames">
            <Segments
              value={draft.buffer_frames}
              options={BUFFERS.map((b): [number, string] => [b, `${b}`])}
              onChange={(buffer_frames) => setDraft({ ...draft, buffer_frames })}
            />
          </Field>
          <div className="card-actions">
            <button
              type="button"
              className="button ghost"
              disabled={scanning}
              onClick={() => void scan(draft.host)}
            >
              <RefreshCw size={14} className={scanning ? 'spin' : ''} />
              {scanning ? 'Scanning' : 'Rescan'}
            </button>
            <span className="spacer" />
            <button type="button" className="button" disabled={!changed} onClick={() => setDraft(props.session.audio)}>
              <Undo2 size={14} /> Revert
            </button>
            <button
              type="button"
              className="button primary"
              disabled={!changed}
              onClick={() => act({ cmd: 'set_audio', settings: draft })}
            >
              <Check size={14} /> Apply
            </button>
          </div>
        </section>
      </Gate>

      <Gate allowed={!viewer} reason={adminOnly}>
        <section className="card">
          <header className="card-head">
            <Disc3 size={16} />
            <div>
              <h2>Recording</h2>
              <p>
                Each armed strip records to its own file; every take gets its own dated folder. Which strips record,
                and from where, is on Patch → Record.
              </p>
            </div>
          </header>
          <Field label="Folder" hint={storage?.available ? 'chosen under Storage' : 'in use'}>
            <div className="path">
              <FolderOpen size={14} />
              <span>{storage?.folder ?? recording.folder ?? '—'}</span>
            </div>
          </Field>
          <Field label="Format">
            <Segments
              value={recording.format}
              options={[
                ['wav', 'WAV'],
                ['flac', 'FLAC'],
              ]}
              onChange={(format) => setRecording({ format })}
            />
          </Field>
          <Field label="Bit depth">
            <Segments
              value={recording.bit_depth}
              options={[
                [16, '16-bit'],
                [24, '24-bit'],
                [32, '32-bit float'],
              ]}
              onChange={(bit_depth) => setRecording({ bit_depth })}
            />
          </Field>
        </section>
      </Gate>

      <Gate allowed={admin} reason={adminOnly}>
          <StorageCard session={props.session} recording={isRecording} message={storage} />
      </Gate>

      {!phase4 ? (
        <section className="card">
          <header className="card-head">
            <ShieldCheck size={16} />
            <div>
              <h2>Users and remote control</h2>
              <p>This LiveStage server predates users, PINs and MIDI/OSC remote control. Update the server.</p>
            </div>
          </header>
        </section>
      ) : admin ? (
        <>
          <UsersCard session={props.session} />
          <RemoteCard session={props.session} />
        </>
      ) : (
        <section className="card">
          <header className="card-head">
            <ShieldCheck size={16} />
            <div>
              <h2>Users and remote control</h2>
              <p>Only an admin manages users, PINs and MIDI/OSC remote control.</p>
            </div>
          </header>
        </section>
      )}
    </div>
  )
}
