import { useEffect, useRef, useState, type RefObject } from 'react'
import { AnimatePresence, Reorder, useDragControls } from 'motion/react'
import { DotsSixVerticalIcon, PlusIcon } from '@phosphor-icons/react'
import type { MeterFrame } from './bridge'
import { clamp } from './math'
import type { RackModule } from './modules'
import { Popover } from './Popover'

const FLOOR_DB = -48
const levelUnit = (linear: number | undefined) =>
  linear && linear > 0 ? clamp((20 * Math.log10(linear) - FLOOR_DB) / -FLOOR_DB, 0, 1) : 0

export type ChainListProps = {
  modules: readonly RackModule[]
  available: readonly RackModule[]
  selected: number | null
  enabled: (module: RackModule) => boolean
  powered: boolean
  stageRef: RefObject<MeterFrame | null>
  onSelect: (code: number) => void
  onReorder: (order: number[]) => void
  onToggle: (module: RackModule) => void
  onAdd: (module: RackModule) => void
}

/**
 * The rack in signal order: input at the top, output at the bottom. Rows
 * select the module shown on the right; drag the grip (or ↑ ↓ on it) to
 * reorder. Each row's two bars are the level entering and leaving that
 * stage, from the host's per-position telemetry.
 */
export function ChainList({
  modules,
  available,
  selected,
  enabled,
  powered,
  stageRef,
  onSelect,
  onReorder,
  onToggle,
  onAdd,
}: ChainListProps) {
  const [pickerOpen, setPickerOpen] = useState(false)
  const pickerAnchor = useRef<HTMLButtonElement | null>(null)
  const order = modules.map((module) => module.code)

  const move = (code: number, delta: -1 | 1) => {
    const from = order.indexOf(code)
    const to = from + delta
    if (from < 0 || to < 0 || to >= order.length) return
    const next = [...order]
    ;[next[from], next[to]] = [next[to]!, next[from]!]
    onReorder(next)
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-1.5">
      <div className="flex items-center justify-between px-1">
        <span className="cap">Signal path</span>
        <span className="num text-[10.5px] text-ink-4">{modules.length} / 6</span>
      </div>
      <FlowCap label="In" />
      <div className="min-h-0 flex-1 overflow-y-auto">
        {modules.length === 0 ? (
          <p className="rounded-md border border-dashed border-line-hi px-3 py-6 text-center text-[11px] text-ink-3">
            The rack is empty. Add a module to start the chain.
          </p>
        ) : (
          <Reorder.Group axis="y" values={order} onReorder={onReorder} className="flex flex-col gap-1">
            <AnimatePresence initial={false}>
              {modules.map((module, index) => (
                <ChainRow
                  key={module.code}
                  module={module}
                  index={index}
                  selected={module.code === selected}
                  on={enabled(module)}
                  powered={powered}
                  stageRef={stageRef}
                  onSelect={() => onSelect(module.code)}
                  onToggle={() => onToggle(module)}
                  onMove={(delta) => move(module.code, delta)}
                />
              ))}
            </AnimatePresence>
          </Reorder.Group>
        )}
        <button
          ref={pickerAnchor}
          type="button"
          aria-haspopup="menu"
          aria-expanded={pickerOpen}
          disabled={available.length === 0}
          onClick={() => setPickerOpen((open) => !open)}
          className="mt-1 flex h-9 w-full cursor-pointer items-center gap-2 rounded-md border border-dashed border-line-hi px-2.5 text-left text-[11.5px] text-ink-2 transition-colors duration-150 hover:border-accent/60 hover:text-ink disabled:cursor-not-allowed disabled:opacity-40"
        >
          <PlusIcon size={13} weight="bold" />
          {available.length === 0 ? 'All six modules are in the chain' : 'Add module'}
        </button>
        <AnimatePresence>
          {pickerOpen && available.length > 0 && (
            <Popover anchorRef={pickerAnchor} onClose={() => setPickerOpen(false)} align="start" width={240} className="p-1">
              <p className="cap px-2 pt-1.5 pb-1">Append to the chain</p>
              <div role="menu" className="flex flex-col">
                {available.map((module) => (
                  <button
                    key={module.code}
                    type="button"
                    role="menuitem"
                    onClick={() => {
                      setPickerOpen(false)
                      onAdd(module)
                    }}
                    className="flex cursor-pointer flex-col rounded px-2.5 py-1.5 text-left transition-colors duration-150 hover:bg-white/6"
                  >
                    <span className="text-[12px] font-semibold">{module.name}</span>
                    <span className="truncate text-[10.5px] text-ink-3">{module.hint}</span>
                  </button>
                ))}
              </div>
            </Popover>
          )}
        </AnimatePresence>
      </div>
      <FlowCap label="Out" />
    </div>
  )
}

function FlowCap({ label }: { label: string }) {
  return (
    <div className="flex items-center gap-2 px-1" aria-hidden="true">
      <span className="cap w-7">{label}</span>
      <span className="h-px flex-1 bg-line-hi" />
    </div>
  )
}

function ChainRow({
  module,
  index,
  selected,
  on,
  powered,
  stageRef,
  onSelect,
  onToggle,
  onMove,
}: {
  module: RackModule
  index: number
  selected: boolean
  on: boolean
  powered: boolean
  stageRef: RefObject<MeterFrame | null>
  onSelect: () => void
  onToggle: () => void
  onMove: (delta: -1 | 1) => void
}) {
  const controls = useDragControls()
  const live = on && powered
  return (
    <Reorder.Item
      value={module.code}
      dragListener={false}
      dragControls={controls}
      layout="position"
      initial={{ opacity: 0, y: 6 }}
      animate={{ opacity: 1, y: 0 }}
      exit={{ opacity: 0, transition: { duration: 0.12 } }}
      transition={{ type: 'spring', stiffness: 460, damping: 40 }}
      whileDrag={{ zIndex: 20, boxShadow: '0 10px 28px rgb(0 0 0 / 0.5)' }}
      className="list-none"
    >
      <div
        className={`relative flex h-12 items-center gap-1.5 overflow-hidden rounded-md border pr-2 transition-colors duration-150 ${
          selected ? 'border-line-hi bg-raised' : 'border-line bg-panel hover:bg-white/[0.03]'
        }`}
      >
        {selected && <span className="absolute inset-y-0 left-0 w-0.5 bg-accent" aria-hidden="true" />}
        <button
          type="button"
          aria-label={`Reorder ${module.name} — drag, or use the arrow keys`}
          title="Drag to reorder (↑ ↓ when focused)"
          onPointerDown={(event) => controls.start(event)}
          onKeyDown={(event) => {
            if (event.key !== 'ArrowUp' && event.key !== 'ArrowDown') return
            event.preventDefault()
            onMove(event.key === 'ArrowUp' ? -1 : 1)
          }}
          className="grid h-full w-6 shrink-0 cursor-grab place-items-center text-ink-4 hover:text-ink active:cursor-grabbing"
        >
          <DotsSixVerticalIcon size={13} weight="bold" />
        </button>
        <button
          type="button"
          onClick={onSelect}
          aria-pressed={selected}
          className={`flex min-w-0 flex-1 cursor-pointer items-center gap-2 text-left ${on ? '' : 'opacity-55'}`}
        >
          <span className="num w-3 shrink-0 text-[10.5px] text-ink-4">{index + 1}</span>
          <span className="flex min-w-0 flex-1 flex-col gap-1">
            <span className="truncate text-[12px] font-semibold">{module.name}</span>
            <StageBars stageRef={stageRef} slot={index} live={live} />
          </span>
        </button>
        <button
          type="button"
          role="switch"
          aria-checked={on}
          aria-label={`${module.name} ${on ? 'on' : 'bypassed'}`}
          title={on ? `Bypass ${module.name}` : `Turn ${module.name} on`}
          disabled={!powered}
          onClick={onToggle}
          className={`grid h-6 w-6 shrink-0 cursor-pointer place-items-center rounded border transition-colors duration-150 disabled:cursor-not-allowed disabled:opacity-40 ${
            on ? 'border-accent/55 bg-accent/15' : 'border-line-hi'
          }`}
        >
          <span className={`h-1.5 w-1.5 rounded-full ${on ? 'bg-accent' : 'bg-ink-4'}`} />
        </button>
      </div>
    </Reorder.Item>
  )
}

/// Level into and out of one rack position, painted from the latest frame.
/// Dark when the host published no per-stage levels.
function StageBars({ stageRef, slot, live }: { stageRef: RefObject<MeterFrame | null>; slot: number; live: boolean }) {
  const inRef = useRef<HTMLDivElement>(null)
  const outRef = useRef<HTMLDivElement>(null)
  const liveRef = useRef(live)
  useEffect(() => {
    liveRef.current = live
  }, [live])

  useEffect(() => {
    let raf = 0
    const paint = () => {
      raf = requestAnimationFrame(paint)
      const frame = liveRef.current ? stageRef.current : null
      if (inRef.current) inRef.current.style.transform = `scaleX(${levelUnit(frame?.slotInPeak[slot])})`
      if (outRef.current) outRef.current.style.transform = `scaleX(${levelUnit(frame?.slotOutPeak[slot])})`
    }
    raf = requestAnimationFrame(paint)
    return () => cancelAnimationFrame(raf)
  }, [stageRef, slot])

  return (
    <span className="flex flex-col gap-[3px]" aria-hidden="true">
      <span className="block h-[3px] overflow-hidden bg-canvas">
        <span ref={inRef} className="block h-full w-full origin-left bg-ink-4" style={{ transform: 'scaleX(0)' }} />
      </span>
      <span className="block h-[3px] overflow-hidden bg-canvas">
        <span ref={outRef} className="block h-full w-full origin-left bg-accent" style={{ transform: 'scaleX(0)' }} />
      </span>
    </span>
  )
}
