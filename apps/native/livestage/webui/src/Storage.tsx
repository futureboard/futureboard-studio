// The appliance's drives: where takes are recorded, how long they can run,
// and the drives to record to, eject or format. The appliance's storage
// service does the work (apps/native/livestage/src/storage.rs); the server
// refuses what would pull the disk from under a take, and this card says
// why before anyone tries.

import { useEffect, useState } from 'react'
import {
  ArrowUpFromLine,
  CircleAlert,
  Disc3,
  Eraser,
  FolderOpen,
  HardDrive,
  LoaderCircle,
  Server,
  Timer,
  TriangleAlert,
  Usb,
} from 'lucide-react'
import { Modal } from './Inserts.tsx'
import { recordWidth } from './Patch.tsx'
import type { Session, StorageDisk, StorageMessage, StorageRequest, StorageState, StorageVolume, StripRef } from './protocol.ts'
import { notify, request, useStore } from './store.ts'
import { demoFormatDisk } from './storageDemo.ts'

const INTERNAL = 'internal'
const DEFAULT_LABEL = 'LIVESTAGE'
const MAX_LABEL = 11

/** Decimal units, as drives are sold: 64 GB is 64 000 000 000 bytes. */
export function formatBytes(bytes: number): string {
  const units: [number, string][] = [
    [1e12, 'TB'],
    [1e9, 'GB'],
    [1e6, 'MB'],
    [1e3, 'kB'],
  ]
  for (const [size, unit] of units) {
    if (bytes >= size) {
      const value = bytes / size
      return `${value >= 100 ? value.toFixed(0) : value.toFixed(1)} ${unit}`
    }
  }
  return `${bytes} B`
}

function volumeName(volume: StorageVolume): string {
  if (volume.id === INTERNAL) return 'Internal'
  // Unlabelled: its partition, since the drive's model heads its group.
  return volume.label || volume.device
}

function diskName(disk: { disk: string; model: string | null }): string {
  return disk.model ?? disk.disk
}

/** What a take writes per second: every armed strip's channels at the
 *  device's rate, at the record settings' sample width (WAV size; FLAC is
 *  smaller). Strips record as Patch → Record says. */
export function recordBytesPerSecond(session: Session, sampleRate: number): { bytes: number; strips: number } {
  const armed: StripRef[] = [
    ...session.channels.filter((c) => c.record_arm).map((c): StripRef => ({ kind: 'channel', id: c.id })),
    ...session.buses.filter((b) => b.record_arm).map((b): StripRef => ({ kind: 'bus', id: b.id })),
    ...session.matrices.filter((m) => m.record_arm).map((m): StripRef => ({ kind: 'matrix', id: m.id })),
    ...(session.master.record_arm ? [{ kind: 'master' } as StripRef] : []),
  ]
  const channels = armed.reduce((sum, strip) => sum + recordWidth(session, strip), 0)
  const bytesPerSample = session.recording.bit_depth / 8
  return { bytes: channels * sampleRate * bytesPerSample, strips: armed.length }
}

/** "3 h 12 min", "38 min", "under a minute". */
export function duration(seconds: number): string {
  if (seconds < 60) return 'under a minute'
  const minutes = Math.floor(seconds / 60)
  const hours = Math.floor(minutes / 60)
  if (hours === 0) return `${minutes} min`
  if (hours >= 100) return `${hours} h`
  return `${hours} h ${minutes % 60} min`
}

/** A label for Format as the storage service takes it (`valid_label`). */
function labelProblem(label: string): string | null {
  if (!label.trim()) return null // the default
  if (label.length > MAX_LABEL) return `At most ${MAX_LABEL} characters.`
  if (!/^[A-Za-z0-9 _-]*$/.test(label)) return "Only letters, digits, spaces, '-' and '_'."
  if (label !== label.trim()) return 'No space at the start or end.'
  return null
}

function busyText(busy: StorageRequest, storage: StorageState): string {
  switch (busy.op) {
    case 'use': {
      const volume = storage.volumes.find((v) => v.id === busy.id)
      return `Switching recording to ${volume ? volumeName(volume) : busy.id}…`
    }
    case 'eject': {
      const volume = storage.volumes.find((v) => v.id === busy.id)
      return `Ejecting ${volume ? volumeName(volume) : busy.id}…`
    }
    case 'format': {
      const disk = storage.disks.find((d) => d.disk === busy.disk)
      return `Formatting ${disk ? diskName(disk) : busy.disk}…`
    }
    default:
      return 'Working…'
  }
}

/** Drives in the order they matter: the system's, then the rest as listed. */
function groups(storage: StorageState): { disk: string; info: StorageDisk | null; volumes: StorageVolume[] }[] {
  const names: string[] = []
  for (const v of storage.volumes) if (!names.includes(v.disk)) names.push(v.disk)
  for (const d of storage.disks) if (!names.includes(d.disk)) names.push(d.disk)
  const list = names.map((disk) => ({
    disk,
    info: storage.disks.find((d) => d.disk === disk) ?? null,
    volumes: storage.volumes.filter((v) => v.disk === disk),
  }))
  const holdsSystem = (g: (typeof list)[number]) => g.info?.system || g.volumes.some((v) => v.id === INTERNAL)
  return [...list.filter(holdsSystem), ...list.filter((g) => !holdsSystem(g))]
}

function UsageBar(props: { size: number; free: number }) {
  const used = Math.max(0, props.size - props.free)
  const fraction = props.size > 0 ? used / props.size : 0
  const low = props.size > 0 && props.free / props.size < 0.1
  return (
    <div className="usage">
      <div className={`usage-bar${low ? ' low' : ''}`} role="meter" aria-valuenow={Math.round(fraction * 100)} aria-valuemin={0} aria-valuemax={100} aria-label="Used">
        <span style={{ width: `${Math.max(1, fraction * 100)}%` }} />
      </div>
      <span className="usage-numbers">
        <strong>{formatBytes(props.free)} free</strong> of {formatBytes(props.size)}
      </span>
    </div>
  )
}

function VolumeState(props: { volume: StorageVolume }) {
  const { volume } = props
  if (!volume.supported) return <span className="volume-state off">Unsupported{volume.fs ? ` · ${volume.fs}` : ''}</span>
  if (volume.ejected) return <span className="volume-state off">Ejected</span>
  if (volume.mounted === 'rw') return <span className="volume-state rw">Read-write</span>
  if (volume.mounted === 'ro') return <span className="volume-state">Read-only</span>
  return <span className="volume-state off">Not mounted</span>
}

function FormatDialog(props: {
  disk: StorageDisk
  volumes: StorageVolume[]
  holdsTarget: boolean
  /** Why it cannot be formatted now. */
  blocked: string | null
  onClose: () => void
}) {
  const { disk } = props
  // Empty, with the default as its placeholder: what is typed is the whole
  // name, not something added to LIVESTAGE.
  const [label, setLabel] = useState('')
  const [confirm, setConfirm] = useState('')
  const [running, setRunning] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const model = disk.model?.trim() || null
  const confirmed = confirm.trim() === 'ERASE' || (model !== null && confirm.trim() === model)
  const problem = labelProblem(label)
  const name = label.trim() || DEFAULT_LABEL
  const onClose = running ? () => {} : props.onClose

  const format = async () => {
    setRunning(true)
    setError(null)
    const reply = await request({ cmd: 'storage', op: 'format', disk: disk.disk, label: name })
    setRunning(false)
    if (reply.ok) {
      notify(`${diskName(disk)} is formatted as ${name}`)
      props.onClose()
    } else {
      setError(reply.error ?? 'The disk was not formatted.')
    }
  }

  return (
    <Modal icon={<Eraser size={16} />} title="Format disk" subtitle={`${diskName(disk)} · ${disk.disk}`} onClose={onClose}>
      <div className="format-warning">
        <TriangleAlert size={18} />
        <div>
          <strong>
            This erases everything on {model ?? disk.disk} ({formatBytes(disk.size_bytes)}).
          </strong>
          <span>
            {props.volumes.length > 0
              ? `${props.volumes.map(volumeName).join(', ')} and every file on ${props.volumes.length === 1 ? 'it' : 'them'} will be gone. `
              : ''}
            The disk becomes one exFAT volume, readable on Windows, macOS and Linux.
            {props.holdsTarget && ' It stays the recording disk.'}
          </span>
        </div>
      </div>
      <label className="dialog-field">
        <span>Name</span>
        <input
          className="text-input"
          value={label}
          maxLength={MAX_LABEL}
          disabled={running}
          spellCheck={false}
          placeholder={DEFAULT_LABEL}
          onChange={(e) => setLabel(e.target.value)}
        />
        <span className={problem ? 'dialog-hint error-text' : 'dialog-hint'}>
          {problem ?? `Up to ${MAX_LABEL} characters; ${DEFAULT_LABEL} if left empty.`}
        </span>
      </label>
      <label className="dialog-field">
        <span>
          Type <code>{model ?? 'ERASE'}</code>
          {model && (
            <>
              {' '}
              or <code>ERASE</code>
            </>
          )}{' '}
          to confirm
        </span>
        <input
          className="text-input"
          value={confirm}
          disabled={running}
          spellCheck={false}
          autoComplete="off"
          autoFocus
          onChange={(e) => setConfirm(e.target.value)}
        />
      </label>
      {(error ?? props.blocked) && (
        <div className="device-status error">
          <CircleAlert size={15} />
          <span>{error ?? props.blocked}</span>
        </div>
      )}
      <div className="card-actions">
        <span className="spacer" />
        <button type="button" className="button" disabled={running} onClick={props.onClose}>
          Cancel
        </button>
        <button
          type="button"
          className="button danger"
          disabled={!confirmed || problem !== null || running || props.blocked !== null}
          onClick={() => void format()}
        >
          {running ? <LoaderCircle size={14} className="spin" /> : <Eraser size={14} />}
          {running ? 'Formatting…' : 'Erase and format'}
        </button>
      </div>
    </Modal>
  )
}

export function StorageCard(props: { session: Session; recording: boolean; message: StorageMessage | null }) {
  const { session, recording, message } = props
  const sampleRate = useStore((s) => s.status?.status.sample_rate ?? 0)
  const [formatting, setFormatting] = useState<string | null>(() => (import.meta.env.DEV ? demoFormatDisk() : null))
  const [pending, setPending] = useState(false)
  const storage = message?.available ? message.storage : null
  const folder = message?.folder ?? session.recording.folder ?? '—'

  // A disk that went away closes its dialog.
  useEffect(() => {
    if (formatting && storage && !storage.disks.some((d) => d.disk === formatting)) setFormatting(null)
  }, [formatting, storage])

  if (!storage) {
    return (
      <section className="card">
        <header className="card-head">
          <HardDrive size={16} />
          <div>
            <h2>Storage</h2>
            <p>Drives are managed by the LiveStage appliance.</p>
          </div>
        </header>
        <div className="card-empty">
          <Server size={18} />
          <span>{message?.reason ?? 'Waiting for the server…'}</span>
        </div>
        <div className="field">
          <div className="field-label">
            <span>Recording to</span>
          </div>
          <div className="field-control">
            <div className="path">
              <FolderOpen size={14} />
              <span>{folder}</span>
            </div>
          </div>
        </div>
      </section>
    )
  }

  const { target } = storage
  const busy = message?.busy ?? null
  const inUse = target.available
    ? storage.volumes.find((v) => v.id === target.id)
    : storage.volumes.find((v) => v.id === INTERNAL)
  const targetDisk = target.available ? storage.volumes.find((v) => v.id === target.id)?.disk : undefined
  const rate = recordBytesPerSecond(session, sampleRate)
  const free = inUse?.free_bytes ?? null
  const timeLeft = rate.bytes > 0 && free !== null ? duration(free / rate.bytes) : '—'
  const timeHint =
    rate.strips === 0
      ? 'nothing armed'
      : sampleRate === 0
        ? 'no device running'
        : `${rate.strips} strip${rate.strips === 1 ? '' : 's'} · ${sampleRate / 1000} kHz · ${session.recording.bit_depth}-bit${session.recording.format === 'flac' ? ' · FLAC lasts longer' : ''}`
  const locked = busy !== null || pending
  const lockedWhy = busy ? busyText(busy, storage) : 'Waiting for the storage service…'

  const run = async (op: 'use' | 'eject', volume: StorageVolume) => {
    setPending(true)
    const reply = await request({ cmd: 'storage', op, volume: volume.id })
    setPending(false)
    if (!reply.ok) notify(reply.error ?? 'refused', true)
    else if (op === 'use') notify(`Recording to ${volumeName(volume)} from the next take`)
    else notify(`${volumeName(volume)} is ejected: it can be unplugged`)
  }

  const formattingDisk = formatting ? storage.disks.find((d) => d.disk === formatting) : undefined

  return (
    <section className="card">
      <header className="card-head">
        <HardDrive size={16} />
        <div>
          <h2>Storage</h2>
          <p>
            Where takes are recorded. Other drives are mounted read-only; the one recorded to is mounted for writing.
          </p>
        </div>
      </header>

      {!target.available && (
        <div className="storage-alert">
          <TriangleAlert size={16} />
          {storage.volumes.some((v) => v.id === target.id && v.ejected) ? (
            <span>
              <strong>{target.label} is ejected</strong> — recording to Internal. Choose Record here on it, or plug it
              in again, to record to it.
            </span>
          ) : storage.volumes.some((v) => v.id === target.id) ? (
            <span>
              <strong>{target.label} cannot be written to</strong> — recording to Internal.
            </span>
          ) : (
            <span>
              <strong>{target.label} is not connected</strong> — recording to Internal. LiveStage switches back when it
              is plugged in again and nothing is recording.
            </span>
          )}
        </div>
      )}

      <div className="storage-summary">
        <div className="storage-stat">
          <span className="storage-stat-label">
            <Disc3 size={13} /> Recording to
          </span>
          <strong>{inUse ? volumeName(inUse) : target.label}</strong>
          <span className="path">
            <FolderOpen size={14} />
            <span>{target.recordings_dir}</span>
          </span>
        </div>
        <div className="storage-stat">
          <span className="storage-stat-label">
            <Timer size={13} /> Recording time left
          </span>
          <strong className="value">{timeLeft}</strong>
          <span className="storage-stat-hint">{timeHint}</span>
        </div>
      </div>

      {(recording || locked) && (
        <div className="storage-note">
          {locked ? <LoaderCircle size={14} className="spin" /> : <span className="rec-dot" />}
          <span>
            {locked
              ? lockedWhy
              : 'Recording now: the recording drive cannot change, be ejected or be formatted until the take ends.'}
          </span>
        </div>
      )}

      <div className="drives">
        {groups(storage).map(({ disk, info, volumes }) => {
          const system = info?.system || volumes.some((v) => v.id === INTERNAL)
          const holdsTarget = disk === targetDisk
          const formatBlocked = locked ? lockedWhy : recording && holdsTarget ? 'Stop recording to format the recording drive' : null
          const formatBusy = busy?.op === 'format' && busy.disk === disk
          return (
            <div key={disk} className="drive">
              <div className="drive-head">
                {info?.removable ? <Usb size={15} /> : <HardDrive size={15} />}
                <span className="drive-name">{info ? diskName(info) : volumes[0]?.model ?? disk}</span>
                <span className="drive-meta">
                  {disk}
                  {info ? ` · ${formatBytes(info.size_bytes)}` : ''}
                  {system ? ' · system' : ''}
                </span>
                <span className="spacer" />
                {info && !info.system && (
                  <button
                    type="button"
                    className="button small"
                    disabled={formatBlocked !== null}
                    title={formatBlocked ?? `Erase ${diskName(info)} and make it one exFAT volume`}
                    onClick={() => setFormatting(disk)}
                  >
                    {formatBusy ? <LoaderCircle size={13} className="spin" /> : <Eraser size={13} />}
                    {formatBusy ? 'Formatting…' : 'Format disk…'}
                  </button>
                )}
              </div>
              {volumes.length === 0 && <div className="volume volume-empty">No volume LiveStage can read.</div>}
              {volumes.map((volume) => {
                const here = inUse !== undefined && volume === inUse
                const isTarget = target.available && volume.id === target.id
                // The chosen drive too, when it cannot be used now (ejected):
                // choosing it again mounts it.
                const canUse =
                  volume.supported && volume.id !== '' && (volume.id !== target.id || !target.available)
                const useBlocked = locked
                  ? lockedWhy
                  : recording
                    ? 'Stop recording to change where it records'
                    : null
                const canEject = volume.id !== INTERNAL && volume.id !== '' && volume.mounted !== null
                const ejectBlocked = locked ? lockedWhy : recording && isTarget ? 'Stop recording to eject the recording drive' : null
                const usingBusy = busy?.op === 'use' && busy.id === volume.id
                const ejectBusy = busy?.op === 'eject' && busy.id === volume.id
                return (
                  <div key={`${volume.device}`} className={`volume${here ? ' here' : ''}`}>
                    <div className="volume-main">
                      <div className="volume-title">
                        <strong>{volumeName(volume)}</strong>
                        {here && (
                          <span className="volume-badge">
                            <span className="rec-dot" />
                            {target.available ? 'Recording here' : 'Recording here for now'}
                          </span>
                        )}
                        <VolumeState volume={volume} />
                      </div>
                      <div className="volume-meta">
                        {[volume.fs ?? 'no filesystem', volume.device, volume.mount_path].filter(Boolean).join(' · ')}
                      </div>
                      {volume.free_bytes !== null ? (
                        <UsageBar size={volume.size_bytes} free={volume.free_bytes} />
                      ) : (
                        <div className="volume-meta">{formatBytes(volume.size_bytes)}</div>
                      )}
                    </div>
                    <div className="volume-actions">
                      {canUse && (
                        <button
                          type="button"
                          className="button small"
                          disabled={useBlocked !== null}
                          title={useBlocked ?? `Record the next takes to ${volumeName(volume)}`}
                          onClick={() => void run('use', volume)}
                        >
                          {usingBusy ? <LoaderCircle size={13} className="spin" /> : <Disc3 size={13} />}
                          Record here
                        </button>
                      )}
                      {canEject && (
                        <button
                          type="button"
                          className="button small"
                          disabled={ejectBlocked !== null}
                          title={ejectBlocked ?? `Unmount ${volumeName(volume)} so it can be unplugged`}
                          onClick={() => void run('eject', volume)}
                        >
                          {ejectBusy ? <LoaderCircle size={13} className="spin" /> : <ArrowUpFromLine size={13} />}
                          Eject
                        </button>
                      )}
                    </div>
                  </div>
                )
              })}
            </div>
          )
        })}
      </div>

      {formattingDisk && (
        <FormatDialog
          disk={formattingDisk}
          volumes={storage.volumes.filter((v) => v.disk === formattingDisk.disk)}
          holdsTarget={formattingDisk.disk === targetDisk}
          blocked={recording && formattingDisk.disk === targetDisk ? 'Stop recording first: the take is being written to this disk.' : null}
          onClose={() => setFormatting(null)}
        />
      )}
    </section>
  )
}
