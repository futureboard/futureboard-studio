// The console: channels, then buses, with the master pinned on the right.
// Laid out like the desktop mixer, top to bottom: name, input, trim, inserts,
// sends, pan, fader and meter, mute/solo/arm, output.

import { memo, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { ArrowRight, Circle, CircleAlert, Layers, LoaderCircle, Mic, Plus, Power, Speaker, X } from 'lucide-react'
import { Fader, Knob, Latch, Meter, Select } from './controls.tsx'
import { formatDb, formatPan } from './faderLaw.ts'
import type {
  BusStrip,
  ChannelStrip,
  InputPatch,
  InsertSlot,
  InsertState,
  Session,
  StripCore,
  StripOutput,
  StripRef,
} from './protocol.ts'
import { MAX_FADER_DB, MIN_FADER_DB, stripKey } from './protocol.ts'
import { act, actLatest, useStore } from './store.ts'

/** Insert rows always shown, so every rack lines up. */
const RACK_ROWS = 4

export interface InsertTarget {
  strip: StripRef
  insert: number
}

export function Mixer(props: {
  session: Session
  inputs: number
  onOpenInsert: (target: InsertTarget) => void
  onAddEffect: (strip: StripRef) => void
}) {
  const { session } = props
  const insertStates = useStore((s) => s.insertStates)
  // The highest "Ch N" asked for and not yet in the session.
  const asked = useRef(0)
  const common: Common = {
    buses: session.buses,
    inputs: props.inputs,
    insertStates,
    onOpenInsert: props.onOpenInsert,
    onAddEffect: props.onAddEffect,
  }

  const addChannel = () => {
    const number = Math.max(session.channels.length, asked.current) + 1
    asked.current = number
    // The next input nobody has yet, if there is one.
    const used = new Set(session.channels.flatMap((c) => [c.input.left, c.input.right]))
    let next: number | null = null
    for (let i = 0; i < props.inputs; i++) {
      if (!used.has(i)) {
        next = i
        break
      }
    }
    act({ cmd: 'add_channel', name: `Ch ${number}`, input: { left: next, right: null } })
  }

  return (
    <div className="mixer">
      <div className="mixer-scroll">
        {session.channels.map((channel, index) => (
          <ChannelView key={channel.id} channel={channel} number={index + 1} {...common} />
        ))}
        <AddTile label="Channel" onClick={addChannel} />
        <div className="mixer-divider" />
        {session.buses.map((bus) => (
          <BusView
            key={bus.id}
            bus={bus}
            feeds={
              session.channels.filter(
                (c) =>
                  (c.output.kind === 'bus' && c.output.id === bus.id) || c.sends.some((s) => s.bus === bus.id),
              ).length
            }
            {...common}
          />
        ))}
        <AddTile label="Bus" onClick={() => act({ cmd: 'add_bus', name: `Bus ${session.buses.length + 1}` })} />
      </div>
      <div className="mixer-master">
        <StripFrame
          {...common}
          strip={{ kind: 'master' }}
          badge={<Speaker size={12} />}
          name="Master"
          core={session.master}
          recordArm={session.master.record_arm}
          top={<div className="strip-note">Main mix</div>}
        />
      </div>
    </div>
  )
}

function AddTile(props: { label: string; onClick: () => void }) {
  return (
    <button type="button" className="add-tile" onClick={props.onClick} title={`Add a ${props.label.toLowerCase()}`}>
      <span className="add-tile-icon">
        <Plus size={16} />
      </span>
      <span className="add-tile-label">{props.label}</span>
    </button>
  )
}

interface Common {
  buses: BusStrip[]
  inputs: number
  insertStates: Record<string, InsertState>
  onOpenInsert: (target: InsertTarget) => void
  onAddEffect: (strip: StripRef) => void
}

const ChannelView = memo(function ChannelView(props: Common & { channel: ChannelStrip; number: number }) {
  const { channel } = props
  return (
    <StripFrame
      {...props}
      strip={{ kind: 'channel', id: channel.id }}
      badge={props.number}
      name={channel.name}
      core={channel}
      recordArm={channel.record_arm}
      top={
        <>
          <InputSelect channel={channel} inputs={props.inputs} />
          <div className="strip-row trim-row">
            <Knob
              value={channel.trim_db}
              min={-24}
              max={24}
              defaultValue={0}
              bipolar
              size={30}
              label="Trim"
              hideValue
              format={(v) => `${v > 0 ? '+' : ''}${v.toFixed(1)}`}
              onChange={(db) => actLatest(`trim:${channel.id}`, { cmd: 'set_trim', channel: channel.id, db })}
            />
            <div className="trim-text">
              <span className="caption">Trim</span>
              <span className="value">
                {channel.trim_db > 0 ? '+' : ''}
                {channel.trim_db.toFixed(1)}
              </span>
            </div>
            <Latch
              kind="plain"
              on={channel.phase_invert}
              title="Polarity invert"
              onClick={() => act({ cmd: 'set_phase_invert', channel: channel.id, invert: !channel.phase_invert })}
            >
              Ø
            </Latch>
          </div>
        </>
      }
      middle={<Sends channel={channel} buses={props.buses} />}
      output={channel.output}
      meterTap
    />
  )
})

const BusView = memo(function BusView(props: Common & { bus: BusStrip; feeds: number }) {
  const { bus } = props
  return (
    <StripFrame
      {...props}
      strip={{ kind: 'bus', id: bus.id }}
      badge={<Layers size={12} />}
      name={bus.name}
      core={bus}
      recordArm={bus.record_arm}
      top={
        <div className="strip-note">
          {props.feeds === 0 ? 'Nothing feeds it yet' : `Fed by ${props.feeds} channel${props.feeds === 1 ? '' : 's'}`}
        </div>
      }
      output={bus.output}
      isBus
    />
  )
})

function StripFrame(
  props: Common & {
    strip: StripRef
    badge: ReactNode
    name: string
    core: StripCore
    recordArm: boolean
    top?: ReactNode
    middle?: ReactNode
    output?: StripOutput
    isBus?: boolean
    meterTap?: boolean
  },
) {
  const { strip, core } = props
  const key = stripKey(strip)
  const [showInput, setShowInput] = useState(false)
  return (
    <div className={`strip strip-${strip.kind}${core.mute ? ' muted' : ''}`}>
      <StripName strip={strip} name={props.name} badge={props.badge} />
      <div className="strip-top">{props.top}</div>
      <Inserts
        inserts={core.inserts}
        states={props.insertStates}
        onOpen={(insert) => props.onOpenInsert({ strip, insert })}
        onAdd={() => props.onAddEffect(strip)}
      />
      <div className="strip-middle">{props.middle}</div>
      <div className="strip-row pan-row">
        <Knob
          value={core.pan}
          min={-1}
          max={1}
          defaultValue={0}
          bipolar
          size={34}
          label={strip.kind === 'channel' ? 'Pan' : 'Balance'}
          format={formatPan}
          onChange={(pan) => actLatest(`pan:${key}`, { cmd: 'set_pan', strip, pan })}
        />
      </div>
      <Fader
        db={core.fader_db}
        onChange={(db) => actLatest(`fader:${key}`, { cmd: 'set_fader', strip, db })}
        meter={
          <div className="strip-meters">
            {props.meterTap && showInput && <Meter strip={key} tap="input" />}
            <Meter strip={key} />
          </div>
        }
      />
      <div className="strip-row latches">
        <Latch kind="mute" on={core.mute} title="Mute" onClick={() => act({ cmd: 'set_mute', strip, mute: !core.mute })}>
          M
        </Latch>
        {strip.kind !== 'master' && (
          <Latch
            kind="solo"
            on={core.solo}
            title="Solo"
            onClick={() => act({ cmd: 'set_solo', strip, solo: !core.solo })}
          >
            S
          </Latch>
        )}
        <Latch
          kind="arm"
          on={props.recordArm}
          title="Record this strip"
          onClick={() => act({ cmd: 'set_record_arm', strip, arm: !props.recordArm })}
        >
          <Circle size={10} fill="currentColor" strokeWidth={0} />
        </Latch>
        {props.meterTap && (
          <Latch kind="plain" on={showInput} title="Show the input meter" onClick={() => setShowInput(!showInput)}>
            IN
          </Latch>
        )}
      </div>
      {props.output ? (
        <OutputSelect strip={strip} output={props.output} buses={props.isBus ? [] : props.buses} />
      ) : (
        <div className="strip-output-label">
          <Speaker size={12} /> Main out
        </div>
      )}
    </div>
  )
}

function StripName(props: { strip: StripRef; name: string; badge: ReactNode }) {
  const [editing, setEditing] = useState(false)
  const removable = props.strip.kind !== 'master'
  return (
    <div className="strip-head">
      <span className="strip-badge">{props.badge}</span>
      {editing ? (
        <input
          className="strip-name-edit"
          autoFocus
          defaultValue={props.name}
          onBlur={(e) => {
            setEditing(false)
            const name = e.currentTarget.value.trim()
            if (name && name !== props.name) act({ cmd: 'rename_strip', strip: props.strip, name })
          }}
          onKeyDown={(e) => {
            if (e.key === 'Enter') e.currentTarget.blur()
            if (e.key === 'Escape') setEditing(false)
          }}
        />
      ) : (
        <span
          className="strip-name"
          title={removable ? 'Double-click to rename' : undefined}
          onDoubleClick={() => removable && setEditing(true)}
        >
          {props.name}
        </span>
      )}
      {removable && !editing && (
        <button
          type="button"
          className="icon-button strip-remove"
          title={`Remove ${props.name}`}
          onClick={() => {
            if (!window.confirm(`Remove ${props.name}?`)) return
            if (props.strip.kind === 'channel') act({ cmd: 'remove_channel', channel: props.strip.id })
            if (props.strip.kind === 'bus') act({ cmd: 'remove_bus', bus: props.strip.id })
          }}
        >
          <X size={13} />
        </button>
      )}
    </div>
  )
}

function encodeInput(input: InputPatch): string {
  if (input.left === null) return ''
  return input.right === null ? `${input.left}` : `${input.left},${input.right}`
}

function InputSelect(props: { channel: ChannelStrip; inputs: number }) {
  const current = encodeInput(props.channel.input)
  const options: [string, string][] = [['', 'No input']]
  for (let i = 0; i < props.inputs; i++) options.push([`${i}`, `In ${i + 1}`])
  for (let i = 0; i + 1 < props.inputs; i += 2) options.push([`${i},${i + 1}`, `In ${i + 1}-${i + 2}`])
  // Keep an input the device no longer has visible rather than lying.
  if (!options.some(([value]) => value === current)) {
    options.push([current, `In ${current.split(',').map((n) => Number(n) + 1).join('-')} (gone)`])
  }
  return (
    <Select
      value={current}
      icon={<Mic size={12} />}
      title="Input"
      className={current === '' ? 'unset' : ''}
      onChange={(value) => {
        const parts = value.split(',').filter(Boolean).map(Number)
        act({
          cmd: 'set_channel_input',
          channel: props.channel.id,
          input: { left: parts[0] ?? null, right: parts[1] ?? null },
        })
      }}
    >
      {options.map(([value, label]) => (
        <option key={value} value={value}>
          {label}
        </option>
      ))}
    </Select>
  )
}

function encodeOutput(output: StripOutput): string {
  return output.kind === 'bus' ? `bus:${output.id}` : output.kind
}

function OutputSelect(props: { strip: StripRef; output: StripOutput; buses: BusStrip[] }) {
  return (
    <Select
      value={encodeOutput(props.output)}
      icon={<ArrowRight size={12} />}
      title="Output"
      onChange={(value) => {
        const output: StripOutput = value.startsWith('bus:')
          ? { kind: 'bus', id: Number(value.slice(4)) }
          : value === 'none'
            ? { kind: 'none' }
            : { kind: 'master' }
        act({ cmd: 'set_strip_output', strip: props.strip, output })
      }}
    >
      <option value="master">Master</option>
      {props.buses.map((bus) => (
        <option key={bus.id} value={`bus:${bus.id}`}>
          {bus.name}
        </option>
      ))}
      <option value="none">Direct out only</option>
    </Select>
  )
}

function insertName(slot: InsertSlot, effects: Map<string, string>): string {
  return slot.plugin.type === 'builtin' ? (effects.get(slot.plugin.stem) ?? slot.plugin.stem) : slot.plugin.name
}

function Inserts(props: {
  inserts: InsertSlot[]
  states: Record<string, InsertState>
  onOpen: (insert: number) => void
  onAdd: () => void
}) {
  const hello = useStore((s) => s.hello)
  const names = new Map(hello?.effects.map((e) => [e.stem, e.name]) ?? [])
  const blanks = Math.max(0, RACK_ROWS - props.inserts.length - 1)
  return (
    <section className="rack">
      <div className="rack-head">
        <span>Inserts</span>
        {props.inserts.length > 0 && <span className="rack-count">{props.inserts.length}</span>}
      </div>
      <div className="rack-list inserts">
        {props.inserts.map((slot) => {
          const state = props.states[slot.id] ?? 'ready'
          const failed = typeof state === 'object'
          return (
            <div
              key={slot.id}
              className={`slot${slot.bypass ? ' bypassed' : ''}${failed ? ' failed' : ''}`}
              title={failed ? state.failed : undefined}
            >
              <button
                type="button"
                className="slot-power"
                title={slot.bypass ? 'Bypassed: click to switch on' : 'Click to bypass'}
                onClick={() => act({ cmd: 'set_insert_bypass', insert: slot.id, bypass: !slot.bypass })}
              >
                <Power size={10} strokeWidth={2.75} />
              </button>
              <button type="button" className="slot-name" onClick={() => props.onOpen(slot.id)}>
                {insertName(slot, names)}
              </button>
              {state === 'loading' && <LoaderCircle size={11} className="spin" />}
              {failed && <CircleAlert size={11} className="slot-alert" />}
            </div>
          )
        })}
        <button type="button" className="slot slot-add" onClick={props.onAdd}>
          <Plus size={12} /> Effect
        </button>
        {Array.from({ length: blanks }, (_, i) => (
          <div key={i} className="slot slot-blank" />
        ))}
      </div>
    </section>
  )
}

function Sends(props: { channel: ChannelStrip; buses: BusStrip[] }) {
  const { channel } = props
  const unused = props.buses.filter((bus) => !channel.sends.some((s) => s.bus === bus.id))
  return (
    <section className="rack sends">
      <div className="rack-head">
        <span>Sends</span>
        {channel.sends.length > 0 && <span className="rack-count">{channel.sends.length}</span>}
      </div>
      <div className="rack-list send-list">
        {channel.sends.map((send) => {
          const bus = props.buses.find((b) => b.id === send.bus)
          return (
            <div key={send.bus} className="send">
              <Knob
                value={send.level_db}
                min={MIN_FADER_DB}
                max={MAX_FADER_DB}
                defaultValue={0}
                size={24}
                hideValue
                label={`Send to ${bus?.name ?? 'bus'}`}
                format={formatDb}
                onChange={(level_db) =>
                  actLatest(`send:${channel.id}:${send.bus}`, {
                    cmd: 'set_send',
                    channel: channel.id,
                    bus: send.bus,
                    level_db,
                    pre_fader: send.pre_fader,
                  })
                }
              />
              <div className="send-side">
                <span className="send-title">
                  <span className="send-name">{bus?.name ?? '?'}</span>
                  <button
                    type="button"
                    className="icon-button send-remove"
                    title="Remove this send"
                    onClick={() => act({ cmd: 'remove_send', channel: channel.id, bus: send.bus })}
                  >
                    <X size={10} />
                  </button>
                </span>
                <span className="send-meta">
                  <span className="value">{formatDb(send.level_db)}</span>
                  <button
                    type="button"
                    className={`pill${send.pre_fader ? ' on' : ''}`}
                    title="Pre-fader (monitor mix) or post-fader (effect send)"
                    onClick={() =>
                      act({
                        cmd: 'set_send',
                        channel: channel.id,
                        bus: send.bus,
                        level_db: send.level_db,
                        pre_fader: !send.pre_fader,
                      })
                    }
                  >
                    {send.pre_fader ? 'PRE' : 'POST'}
                  </button>
                </span>
              </div>
            </div>

          )
        })}
        {channel.sends.length === 0 && (
          <div className="rack-empty">{props.buses.length === 0 ? 'Add a bus to send to' : 'No sends'}</div>
        )}
      </div>
      {unused.length > 0 && (
        <Select
          value=""
          icon={<Plus size={12} />}
          className="add-send"
          onChange={(value) => {
            const bus = Number(value)
            if (!bus) return
            // Opens at silence, like a fader: it is turned up, not found loud.
            act({ cmd: 'set_send', channel: channel.id, bus, level_db: MIN_FADER_DB, pre_fader: false })
          }}
        >
          <option value="">Send</option>
          {unused.map((bus) => (
            <option key={bus.id} value={bus.id}>
              {bus.name}
            </option>
          ))}
        </Select>
      )}
    </section>
  )
}
