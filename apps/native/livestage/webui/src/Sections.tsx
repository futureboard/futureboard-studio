// The section picker a copy and a library item share: which parts of a
// strip they carry. "Processing" covers its five parts, which then show as
// included.

import { Check } from 'lucide-react'
import type { Section } from './protocol.ts'
import { PROCESSING_PARTS, SECTIONS } from './workstate.ts'

/** Which sections a copy (or a library item) carries. "Processing" covers
 *  its five parts, which then show as included. */
export function SectionPicker(props: {
  value: Section[]
  onChange: (sections: Section[]) => void
  available: Section[]
}) {
  const set = new Set(props.value)
  const whole = set.has('processing')
  const toggle = (id: Section) => {
    const next = new Set(set)
    if (next.has(id)) next.delete(id)
    else next.add(id)
    if (id === 'processing' && next.has('processing')) for (const p of PROCESSING_PARTS) next.delete(p)
    props.onChange(SECTIONS.map((s) => s.id).filter((s) => next.has(s)))
  }
  const chip = (id: Section) => {
    const section = SECTIONS.find((s) => s.id === id)!
    const included = whole && PROCESSING_PARTS.includes(id)
    const on = set.has(id) || included
    const missing = !props.available.includes(id)
    return (
      <button
        key={id}
        type="button"
        role="checkbox"
        aria-checked={on}
        className={`section-chip${on ? ' on' : ''}${included ? ' included' : ''}`}
        disabled={missing || included}
        title={missing ? `${section.label}: this strip has none` : included ? 'Included in Processing' : section.detail}
        onClick={() => toggle(id)}
      >
        <span className="section-box">{on && <Check size={11} strokeWidth={3} />}</span>
        <span className="section-text">
          <span className="section-label">{section.label}</span>
          <span className="section-detail">{section.detail}</span>
        </span>
      </button>
    )
  }
  return (
    <div className="section-picker">
      <div className="section-group">{chip('processing')}</div>
      <div className="section-group parts" aria-label="Or parts of it">
        {PROCESSING_PARTS.map(chip)}
      </div>
      <div className="section-group">
        {(['inserts', 'sends', 'fader_pan', 'input', 'name_color'] as Section[]).map(chip)}
      </div>
    </div>
  )
}
