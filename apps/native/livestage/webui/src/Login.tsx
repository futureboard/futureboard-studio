// Phase 4: who is at this screen. The login page (users mode, nobody
// logged in here) and the PIN pad it shares with the lock and Change PIN.
//
// The server says who may log in (`auth.users`); the PIN goes to it and its
// answer, wrong PIN or the wait after too many tries, is shown as it said
// it. A kept token logs a reload straight back in (store.ts `resume`).

import { useEffect, useRef, useState } from 'react'
import { AudioLines, ChevronLeft, CircleAlert, Delete, LoaderCircle, LogIn, UserRound, WifiOff } from 'lucide-react'
import type { Role, UserSummary } from './protocol.ts'
import { login, useStore } from './store.ts'
import './access.css'

export const PIN_MIN = 4
export const PIN_MAX = 32

export const ROLE_LABEL: Record<Role, string> = {
  admin: 'Admin',
  engineer: 'Engineer',
  musician: 'Musician',
  viewer: 'Viewer',
}

export const ROLE_DETAIL: Record<Role, string> = {
  admin: 'Everything, including users, storage, the audio device and remote control',
  engineer: 'The whole mix; not users, storage, the audio device or remote settings',
  musician: 'Only their own monitor mixes, from the My mix page',
  viewer: 'Sees everything, changes nothing',
}

/** Big digits for a finger, the keyboard too (digits, Backspace, Enter).
 *  Digits only on the pad; the server takes any 4–32 characters. */
export function PinPad(props: {
  value: string
  onChange: (value: string) => void
  onSubmit: () => void
  busy?: boolean
  submitLabel: string
  /** Shown under the dots: the server's refusal, verbatim. */
  error?: string | null
  /** Listen to the keyboard (only the pad that has the screen). */
  keyboard?: boolean
  label?: string
}) {
  const { value, onChange, onSubmit, busy } = props
  const ready = value.length >= PIN_MIN && !busy
  const latest = useRef({ value, onChange, onSubmit, ready })
  latest.current = { value, onChange, onSubmit, ready }
  useEffect(() => {
    if (props.keyboard === false) return
    const onKey = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null
      if (target && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.tagName === 'SELECT')) return
      if (e.ctrlKey || e.metaKey || e.altKey) return
      const now = latest.current
      if (/^\d$/.test(e.key)) {
        e.preventDefault()
        if (now.value.length < PIN_MAX) now.onChange(now.value + e.key)
      } else if (e.key === 'Backspace') {
        e.preventDefault()
        now.onChange(now.value.slice(0, -1))
      } else if (e.key === 'Enter') {
        e.preventDefault()
        if (now.ready) now.onSubmit()
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [props.keyboard])
  const press = (digit: string) => {
    if (value.length < PIN_MAX) onChange(value + digit)
  }
  return (
    <div className="pin-pad">
      <div className="pin-dots" aria-live="polite" aria-label={`${props.label ?? 'PIN'}: ${value.length} digits`}>
        {value.length === 0 ? (
          <span className="pin-placeholder">{props.label ?? 'PIN'}</span>
        ) : (
          Array.from({ length: value.length }, (_, i) => <span key={i} className="pin-dot" />)
        )}
      </div>
      <p className={`pin-error${props.error ? ' on' : ''}`} role={props.error ? 'alert' : undefined}>
        {props.error ? (
          <>
            <CircleAlert size={13} /> {props.error}
          </>
        ) : (
          ' '
        )}
      </p>
      <div className="pin-keys">
        {['1', '2', '3', '4', '5', '6', '7', '8', '9'].map((d) => (
          <button key={d} type="button" className="pin-key" disabled={busy} onClick={() => press(d)}>
            {d}
          </button>
        ))}
        <button
          type="button"
          className="pin-key quiet"
          disabled={busy || value.length === 0}
          aria-label="Delete a digit"
          title="Delete a digit (Backspace)"
          onClick={() => onChange(value.slice(0, -1))}
        >
          <Delete size={20} />
        </button>
        <button type="button" className="pin-key" disabled={busy} onClick={() => press('0')}>
          0
        </button>
        <button
          type="button"
          className="pin-key go"
          disabled={!ready}
          title={value.length < PIN_MIN ? `At least ${PIN_MIN} digits` : `${props.submitLabel} (Enter)`}
          onClick={onSubmit}
        >
          {busy ? <LoaderCircle size={18} className="spin" /> : props.submitLabel}
        </button>
      </div>
    </div>
  )
}

/** Users mode, nobody logged in on this page: pick who you are, then the PIN. */
export function LoginScreen() {
  const auth = useStore((s) => s.auth)
  const connection = useStore((s) => s.connection)
  const resuming = useStore((s) => s.resuming)
  const users = auth?.users ?? []
  const [name, setName] = useState<string | null>(() => (users.length === 1 ? users[0].name : null))
  const [pin, setPin] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  // A user removed while their name is chosen: back to the list.
  const chosen: UserSummary | undefined = users.find((u) => u.name === name)
  useEffect(() => {
    if (name !== null && !chosen && users.length > 0) setName(null)
  }, [name, chosen, users.length])

  const submit = async () => {
    if (!name || busy) return
    setBusy(true)
    setError(null)
    const refused = await login(name, pin)
    setBusy(false)
    setPin('')
    if (refused) setError(refused)
  }

  return (
    <div className="login">
      <div className="login-card">
        <header className="login-head">
          <span className="brand-mark">
            <AudioLines size={16} strokeWidth={2.25} />
          </span>
          <div className="login-title">
            <strong>LiveStage</strong>
            <span>{chosen ? 'Enter your PIN' : 'Who is mixing?'}</span>
          </div>
          <span className={`status-pill link link-${connection}`}>
            <span className="link-dot" />
            {connection === 'open' ? 'Live' : connection === 'connecting' ? 'Connecting' : 'Offline'}
          </span>
        </header>
        {connection !== 'open' ? (
          <div className="login-wait">
            <WifiOff size={18} /> Waiting for the LiveStage server…
          </div>
        ) : resuming ? (
          <div className="login-wait">
            <LoaderCircle size={18} className="spin" /> Signing back in…
          </div>
        ) : !chosen ? (
          <div className="login-users" role="list">
            {users.length === 0 && <p className="muted">The server lists no users.</p>}
            {users.map((u) => (
              <button
                key={u.name}
                type="button"
                role="listitem"
                className="login-user"
                onClick={() => {
                  setName(u.name)
                  setPin('')
                  setError(null)
                }}
              >
                <UserRound size={18} />
                <span className="login-user-name">{u.name}</span>
                <span className={`role-badge role-${u.role}`}>{ROLE_LABEL[u.role] ?? u.role}</span>
              </button>
            ))}
          </div>
        ) : (
          <div className="login-pin">
            <button type="button" className="login-back" onClick={() => setName(null)}>
              <ChevronLeft size={16} />
              <UserRound size={15} />
              <strong>{chosen.name}</strong>
              <span className={`role-badge role-${chosen.role}`}>{ROLE_LABEL[chosen.role] ?? chosen.role}</span>
              {users.length > 1 && <span className="login-change">Not you?</span>}
            </button>
            <PinPad
              value={pin}
              onChange={(v) => {
                setPin(v)
                if (v.length > 0) setError(null)
              }}
              onSubmit={() => void submit()}
              busy={busy}
              error={error}
              submitLabel="Log in"
            />
          </div>
        )}
        <p className="login-foot">
          <LogIn size={12} /> Your login stays on this device until you log out (12 h without use, or a server restart,
          asks again).
        </p>
      </div>
    </div>
  )
}
