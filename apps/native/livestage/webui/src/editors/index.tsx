// Which editor draws which built-in. Each family module exports
// `editors: Record<stem, EditorComponent>`, ported from its native GPUI
// family in crates/SphereUIComponents. An effect no family covers gets the
// generic editor: one control per descriptor parameter.

import type { BuiltinEffect, BuiltinParam, InsertSlot } from '../protocol.ts'
import { editors as band } from './band.tsx'
import { editors as dynamics } from './dynamics.tsx'
import { editors as eq } from './eq.tsx'
import { editors as fx } from './fx.tsx'
import type { Editor, EditorComponent } from './kit.tsx'
import { Card, EditorShell, KitKnob, ParamCheck, ParamChoice, useEditor } from './kit.tsx'
import type { KnobSpec, Unit } from './knobspec.ts'
import { editors as mixstation } from './mixstation.tsx'
import { editors as rodhareist } from './rodhareist.tsx'
import { editors as whitesharp } from './whitesharp.tsx'

const REGISTRY: Record<string, EditorComponent> = {
  ...dynamics,
  ...band,
  ...mixstation,
  ...eq,
  ...fx,
  ...whitesharp,
  ...rodhareist,
}

export function PluginEditor(props: { slot: InsertSlot; effect: BuiltinEffect }) {
  const { slot, effect } = props
  if (slot.plugin.type !== 'builtin' || !effect.spec) return null
  return <Mounted slot={slot} effect={effect} />
}

function Mounted(props: { slot: InsertSlot; effect: BuiltinEffect }) {
  const editor = useEditor(props.slot, props.effect, props.effect.spec!)
  const Family = REGISTRY[props.effect.stem] ?? GenericEditor
  return <Family editor={editor} />
}

function unitOf(param: BuiltinParam): Unit {
  switch (param.unit) {
    case 'dB':
      return 'db'
    case 'ms':
      return 'ms'
    case 's':
      return 'sec'
    case 'Hz':
      return 'hz'
    case '%':
      return 'percent'
    case 'µs':
      return 'us'
    case 'st':
      return 'semitones'
    case 'cents':
      return 'cents'
    default:
      return 'plain'
  }
}

function knobFor(param: BuiltinParam): KnobSpec {
  const unit = unitOf(param)
  const log = (unit === 'hz' || unit === 'ms' || unit === 'sec') && param.min > 0
  return {
    id: param.id,
    label: param.name,
    min: param.min,
    max: param.max,
    taper: log ? 'log' : 'linear',
    unit,
    bipolar: param.min < 0 && param.max > 0,
    centre: 0,
  }
}

/** One control per descriptor parameter, in the kit's frame. */
function GenericEditor(props: { editor: Editor }) {
  const { editor } = props
  const params = editor.effect.params.filter((p) => p.id !== 'power')
  return (
    <EditorShell editor={editor} title={editor.effect.name} subtitle={editor.effect.category}>
      <Card title="Parameters">
        <div className="pe-knobs">
          {params.map((param) =>
            param.unit === 'bool' ? (
              <ParamCheck key={param.id} editor={editor} id={param.id} label={param.name} />
            ) : param.unit === 'enum' && param.max - param.min <= 8 ? (
              <div key={param.id} className="pe-knob">
                <ParamChoice
                  editor={editor}
                  id={param.id}
                  options={Array.from({ length: param.max - param.min + 1 }, (_, i): [number, string] => [
                    param.min + i,
                    `${param.min + i}`,
                  ])}
                />
                <span className="knob-caption">{param.name}</span>
              </div>
            ) : (
              <KitKnob key={param.id} editor={editor} spec={knobFor(param)} />
            ),
          )}
        </div>
      </Card>
    </EditorShell>
  )
}
