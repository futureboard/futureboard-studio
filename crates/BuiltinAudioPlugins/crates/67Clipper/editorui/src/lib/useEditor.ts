import { useEffect, useRef, useState } from 'react'
import { connectBridge, postParam } from '../bridge'
import { MeterHistory } from './history'

export type EditorModel<P> = {
  pluginId: string
  defaults: P
  parse: (state: unknown) => P | null
  presets: readonly { name: string; params: P }[]
  /// Index of the preset `params` matches, or `null`.
  match: (params: P) => number | null
  /// Every parameter as `(id, wire value)`, in wire order.
  wireValues: (params: P) => [string, number][]
}

/**
 * The editor's state owner: the host binding, the parameter mirror, the
 * preset it matches, and the telemetry history the displays paint from.
 *
 * Parameter state is React state (it changes on gestures); telemetry lives in
 * a ref (it changes ~30 times a second and must not re-render the controls).
 */
export function useEditor<P extends object>(model: EditorModel<P>) {
  const [params, setParams] = useState<P>(model.defaults)
  const [connected, setConnected] = useState(false)
  const [presetIndex, setPresetIndex] = useState<number | null>(() => model.match(model.defaults))
  const historyRef = useRef(new MeterHistory())
  const modelRef = useRef(model)

  useEffect(
    () =>
      connectBridge<P>({
        pluginId: modelRef.current.pluginId,
        parse: modelRef.current.parse,
        onParams: (next) => {
          setParams(next)
          setPresetIndex(modelRef.current.match(next))
          // Telemetry belongs to the previous binding.
          historyRef.current.clear()
        },
        onConnection: (isConnected) => {
          setConnected(isConnected)
          if (!isConnected) historyRef.current.clear()
        },
        onMeters: (frame) => historyRef.current.push(frame),
      }),
    [],
  )

  /// Apply an edit locally and send exactly the wire values it changed.
  const update = (patch: Partial<P>) => {
    const next = { ...params, ...patch }
    const before = new Map(modelRef.current.wireValues(params))
    for (const [id, value] of modelRef.current.wireValues(next)) {
      if (before.get(id) !== value) postParam(id, value)
    }
    setParams(next)
    setPresetIndex(modelRef.current.match(next))
  }

  const loadPreset = (index: number) => {
    const entry = modelRef.current.presets[index]
    if (!entry) return
    const next = { ...entry.params }
    setParams(next)
    setPresetIndex(index)
    for (const [id, value] of modelRef.current.wireValues(next)) postParam(id, value)
  }

  return { params, connected, presetIndex, historyRef, update, loadPreset }
}
