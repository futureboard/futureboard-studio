// Phase 4: the top bar's user chip and Lock key, and the lock itself.
//
// Lock: users mode locks at once (the logged-in user's PIN, or any admin's,
// unlocks); open mode first asks for a PIN twice, which the server keeps
// hashed in memory. While locked the page still shows everything live —
// the overlay is a scrim, so meters keep moving under it — but every
// change is refused (by the server, and by store.ts before sending).

import { useState } from 'react'
import { KeyRound, Lock, LogOut, ShieldCheck, UserRound } from 'lucide-react'
import { Modal } from './Inserts.tsx'
import { PIN_MAX, PIN_MIN, PinPad, ROLE_DETAIL, ROLE_LABEL } from './Login.tsx'
import { MenuItem } from './Workflow.tsx'
import { MenuButton } from './Workflow.tsx'
import { explain, lock, logout, notify, request, unlock, useStore } from './store.ts'
import './access.css'

/** The top bar's Lock key. */
export function LockButton() {
  const auth = useStore((s) => s.auth)
  const connection = useStore((s) => s.connection)
  const [choosing, setChoosing] = useState(false)
  // An older server sends no `auth`: it has no lock to offer.
  if (!auth) return null
  const open = auth.mode === 'open'
  return (
    <>
      <button
        type="button"
        className="button icon-only lock-key"
        disabled={connection !== 'open'}
        title={open ? 'Lock this page: choose a PIN, then nothing here changes until it is typed' : 'Lock this page: your PIN (or an admin’s) unlocks it'}
        aria-label="Lock"
        onClick={() => {
          if (open) setChoosing(true)
          else void lock().then((refused) => refused && notify(refused, true))
        }}
      >
        <Lock size={15} />
      </button>
      {choosing && <ChooseLockPin onClose={() => setChoosing(false)} />}
    </>
  )
}

/** Open mode: the lock's PIN, typed twice. */
function ChooseLockPin(props: { onClose: () => void }) {
  const [first, setFirst] = useState<string | null>(null)
  const [pin, setPin] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const submit = async () => {
    if (first === null) {
      setFirst(pin)
      setPin('')
      setError(null)
      return
    }
    if (pin !== first) {
      setFirst(null)
      setPin('')
      setError('The two PINs differ: choose it again.')
      return
    }
    setBusy(true)
    const refused = await lock(pin)
    setBusy(false)
    if (refused) {
      setError(refused)
      setFirst(null)
      setPin('')
    } else props.onClose()
  }
  return (
    <Modal
      small
      icon={<Lock size={16} />}
      title="Lock this page"
      subtitle={first === null ? 'Choose a PIN for this lock' : 'Type the same PIN again'}
      onClose={props.onClose}
    >
      <p className="dialog-text">
        No users are set up, so the lock asks for its own PIN ({PIN_MIN}–{PIN_MAX} digits). Only this page is locked; it
        stays locked after a reload.
      </p>
      <PinPad
        value={pin}
        onChange={setPin}
        onSubmit={() => void submit()}
        busy={busy}
        error={error}
        label={first === null ? 'New PIN' : 'Again'}
        submitLabel={first === null ? 'Next' : 'Lock'}
      />
    </Modal>
  )
}

/** Full screen while this page is locked: a scrim over the live console. */
export function LockOverlay() {
  const auth = useStore((s) => s.auth)
  // A kept token is being offered (it may be a lock): nothing is pressed
  // until the server says whether this page is locked.
  const resuming = useStore((s) => s.resuming)
  const [pin, setPin] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  if (resuming && !auth?.locked) return <div className="lock-overlay resuming" aria-busy="true" />
  if (!auth?.locked) return null
  const submit = async () => {
    setBusy(true)
    setError(null)
    const refused = await unlock(pin)
    setBusy(false)
    setPin('')
    if (refused) setError(refused)
  }
  return (
    <div className="lock-overlay" role="dialog" aria-modal="true" aria-label="Locked">
      <div className="lock-card">
        <div className="lock-head">
          <span className="lock-mark">
            <Lock size={18} />
          </span>
          <div className="login-title">
            <strong>Locked</strong>
            <span>
              {auth.mode === 'open'
                ? 'Type the lock’s PIN'
                : `${auth.user?.name ?? 'This page'}: your PIN, or an admin’s`}
            </span>
          </div>
        </div>
        <PinPad
          value={pin}
          onChange={(v) => {
            setPin(v)
            if (v.length > 0) setError(null)
          }}
          onSubmit={() => void submit()}
          busy={busy}
          error={error}
          submitLabel="Unlock"
        />
        <p className="login-foot">Everything keeps running; nothing changes from this page until it is unlocked.</p>
      </div>
    </div>
  )
}

/** Users mode: who is logged in here, with Change PIN and Log out. */
export function UserChip() {
  const user = useStore((s) => s.auth?.user ?? null)
  const mode = useStore((s) => s.auth?.mode)
  const [changing, setChanging] = useState(false)
  if (mode !== 'users' || !user) return null
  return (
    <>
      <MenuButton
        label={`${user.name} · ${ROLE_LABEL[user.role]}`}
        className="user-chip"
        icon={<UserRound size={14} />}
        text={
          <>
            <span className="user-chip-name">{user.name}</span>
            <span className={`role-badge role-${user.role}`}>{ROLE_LABEL[user.role]}</span>
          </>
        }
      >
        {(close) => (
          <>
            <div className="menu-head" title={ROLE_DETAIL[user.role]}>
              {user.name} · {ROLE_DETAIL[user.role]}
            </div>
            <MenuItem
              icon={<KeyRound size={14} />}
              label="Change PIN"
              onClick={() => {
                close()
                setChanging(true)
              }}
            />
            <MenuItem
              icon={<LogOut size={14} />}
              label="Log out"
              detail="This device forgets the login"
              onClick={() => {
                close()
                void logout()
              }}
            />
          </>
        )}
      </MenuButton>
      {changing && <ChangePin name={user.name} onClose={() => setChanging(false)} />}
    </>
  )
}

/** A user's own PIN: the old one, then the new one twice. */
function ChangePin(props: { name: string; onClose: () => void }) {
  const [step, setStep] = useState<'old' | 'new' | 'again'>('old')
  const [old, setOld] = useState('')
  const [next, setNext] = useState('')
  const [pin, setPin] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const submit = async () => {
    setError(null)
    if (step === 'old') {
      setOld(pin)
      setPin('')
      setStep('new')
      return
    }
    if (step === 'new') {
      setNext(pin)
      setPin('')
      setStep('again')
      return
    }
    if (pin !== next) {
      setPin('')
      setStep('new')
      setError('The two new PINs differ: type the new one again.')
      return
    }
    setBusy(true)
    const reply = await request({ cmd: 'user_set', name: props.name, pin: next, old_pin: old })
    setBusy(false)
    if (reply.ok) {
      notify('PIN changed')
      props.onClose()
    } else {
      setError(explain(reply.error))
      setPin('')
      setStep('old')
    }
  }
  return (
    <Modal
      small
      icon={<ShieldCheck size={16} />}
      title="Change PIN"
      subtitle={step === 'old' ? 'Your current PIN' : step === 'new' ? 'The new PIN' : 'The new PIN again'}
      onClose={props.onClose}
    >
      <PinPad
        value={pin}
        onChange={setPin}
        onSubmit={() => void submit()}
        busy={busy}
        error={error}
        label={step === 'old' ? 'Current PIN' : step === 'new' ? 'New PIN' : 'Again'}
        submitLabel={step === 'again' ? 'Change' : 'Next'}
      />
    </Modal>
  )
}
