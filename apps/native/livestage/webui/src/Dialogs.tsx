// Small Studio-shell dialogs: a confirmation (explicit Cancel, the action
// named on its button, destructive styling only when it destroys) and a
// one-field form.

import { useState } from 'react'
import type { ReactNode } from 'react'
import { Modal } from './Inserts.tsx'

export function ConfirmDialog(props: {
  icon: ReactNode
  title: string
  children: ReactNode
  confirmLabel: string
  danger?: boolean
  onConfirm: () => void
  onCancel: () => void
}) {
  return (
    <Modal
      small
      icon={props.icon}
      title={props.title}
      onClose={props.onCancel}
      footer={
        <>
          <button type="button" className="button" onClick={props.onCancel}>
            Cancel
          </button>
          <button
            type="button"
            className={`button ${props.danger ? 'danger' : 'primary'}`}
            autoFocus
            onClick={() => {
              props.onConfirm()
              props.onCancel()
            }}
          >
            {props.confirmLabel}
          </button>
        </>
      }
    >
      <div className="dialog-text">{props.children}</div>
    </Modal>
  )
}

export function PromptDialog(props: {
  icon: ReactNode
  title: string
  label: string
  initial: string
  confirmLabel: string
  hint?: ReactNode
  /** Multi-line (a note). */
  multiline?: boolean
  /** An empty value is allowed (a note can be cleared). */
  allowEmpty?: boolean
  onConfirm: (value: string) => void
  onCancel: () => void
}) {
  const [value, setValue] = useState(props.initial)
  const ok = props.allowEmpty || value.trim() !== ''
  const submit = () => {
    if (!ok) return
    props.onConfirm(props.multiline ? value : value.trim())
    props.onCancel()
  }
  return (
    <Modal
      small
      icon={props.icon}
      title={props.title}
      onClose={props.onCancel}
      footer={
        <>
          <button type="button" className="button" onClick={props.onCancel}>
            Cancel
          </button>
          <button type="button" className="button primary" disabled={!ok} onClick={submit}>
            {props.confirmLabel}
          </button>
        </>
      }
    >
      <label className="dialog-field">
        <span>{props.label}</span>
        {props.multiline ? (
          <textarea
            className="text-input text-area"
            autoFocus
            rows={4}
            value={value}
            onChange={(e) => setValue(e.currentTarget.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) submit()
            }}
          />
        ) : (
          <input
            className="text-input"
            autoFocus
            value={value}
            onFocus={(e) => e.currentTarget.select()}
            onChange={(e) => setValue(e.currentTarget.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') submit()
            }}
          />
        )}
      </label>
      {props.hint && <span className="dialog-hint">{props.hint}</span>}
    </Modal>
  )
}
