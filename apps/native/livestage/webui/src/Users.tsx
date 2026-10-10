// Phase 4: Setup → Users (admin). Who may log in, as what, and which mixes
// a musician may change. PINs go to the server and are kept there hashed;
// nothing here ever shows one back.

import { useEffect, useState } from 'react'
import { Headphones, Pencil, Plus, ShieldAlert, Trash2, UserPlus, UserRound, Users } from 'lucide-react'
import { ConfirmDialog } from './Dialogs.tsx'
import { Modal } from './Inserts.tsx'
import { PIN_MAX, PIN_MIN, ROLE_DETAIL, ROLE_LABEL } from './Login.tsx'
import { mixChoices, sameMix } from './MyMix.tsx'
import type { MixRef, Role, Session, UserSummary } from './protocol.ts'
import { explain, keepToken, notify, request, useStore } from './store.ts'
import './access.css'

const ROLES: Role[] = ['admin', 'engineer', 'musician', 'viewer']

function mixName(session: Session, mix: MixRef): string {
  if (mix.kind === 'bus') return session.buses.find((b) => b.id === mix.id)?.name ?? `Bus ${mix.id} (gone)`
  return session.matrices.find((m) => m.id === mix.id)?.name ?? `Matrix ${mix.id} (gone)`
}

export function UsersCard(props: { session: Session }) {
  const auth = useStore((s) => s.auth)
  const [users, setUsers] = useState<UserSummary[] | null>(null)
  const [editing, setEditing] = useState<UserSummary | 'new' | null>(null)
  const [removing, setRemoving] = useState<UserSummary | null>(null)
  const open = auth?.mode !== 'users'

  // The full list (with mixes) is the admin's `users`; `auth` says when it
  // changed (a user added or removed elsewhere).
  const authUsers = JSON.stringify(auth?.users ?? [])
  useEffect(() => {
    if (open) {
      setUsers([])
      return
    }
    void request({ cmd: 'users' }).then((reply) => {
      if (reply.ok && Array.isArray(reply.users)) setUsers(reply.users as UserSummary[])
      else if (!reply.ok) notify(explain(reply.error), true)
    })
  }, [open, authUsers])

  const remove = async (user: UserSummary) => {
    const reply = await request({ cmd: 'user_remove', name: user.name })
    if (!reply.ok) notify(explain(reply.error), true)
    else notify(`${user.name} removed`)
  }

  const admins = (users ?? []).filter((u) => u.role === 'admin').length
  return (
    <section className="card">
      <header className="card-head">
        <Users size={16} />
        <div>
          <h2>Users</h2>
          <p>Who may log in to this console, as what. Each user has a PIN; a musician changes only their own mixes.</p>
        </div>
      </header>
      {open ? (
        <div className="users-open">
          <ShieldAlert size={16} />
          <div>
            <strong>Open mode: no users.</strong> Anyone who can reach this page on the network has full control, without a
            login. Add an admin to require a PIN; this page is then logged in as that admin.
          </div>
        </div>
      ) : (
        <div className="users-list" role="list">
          {users === null && <p className="muted">Reading the users…</p>}
          {(users ?? []).map((user) => (
            <div key={user.name} className="users-row" role="listitem">
              <UserRound size={15} className="users-icon" />
              <span className="users-name">
                {user.name}
                {auth?.user?.name === user.name && <span className="muted"> (you)</span>}
              </span>
              <span className={`role-badge role-${user.role}`} title={ROLE_DETAIL[user.role]}>
                {ROLE_LABEL[user.role] ?? user.role}
              </span>
              <span className="users-mixes">
                {user.role === 'musician' ? (
                  (user.mixes ?? []).length === 0 ? (
                    <span className="warn-text">No mixes: sees nothing to change</span>
                  ) : (
                    <>
                      <Headphones size={12} /> {(user.mixes ?? []).map((m) => mixName(props.session, m)).join(', ')}
                    </>
                  )
                ) : (
                  <span className="muted">{ROLE_DETAIL[user.role]}</span>
                )}
              </span>
              <button type="button" className="icon-button large" title={`Edit ${user.name}`} onClick={() => setEditing(user)}>
                <Pencil size={14} />
              </button>
              <button
                type="button"
                className="icon-button large danger"
                disabled={user.role === 'admin' && admins <= 1}
                title={user.role === 'admin' && admins <= 1 ? 'The last admin stays: add another admin first' : `Remove ${user.name}`}
                onClick={() => setRemoving(user)}
              >
                <Trash2 size={14} />
              </button>
            </div>
          ))}
        </div>
      )}
      <div className="card-actions">
        <span className="muted users-note">
          {open
            ? 'The first user is an admin.'
            : 'A PIN is 4 to 32 digits. Changing a user applies at once on their devices; removing one logs them out.'}
        </span>
        <span className="spacer" />
        <button type="button" className="button primary" onClick={() => setEditing('new')}>
          {open ? <UserPlus size={14} /> : <Plus size={14} />} {open ? 'Add the first admin' : 'Add user'}
        </button>
      </div>
      {editing && (
        <UserDialog
          session={props.session}
          user={editing === 'new' ? null : editing}
          firstAdmin={open}
          admins={admins}
          onClose={() => setEditing(null)}
          onSaved={(list) => list && setUsers(list)}
        />
      )}
      {removing && (
        <ConfirmDialog
          icon={<Trash2 size={16} />}
          title={`Remove ${removing.name}?`}
          confirmLabel="Remove"
          danger
          onConfirm={() => void remove(removing)}
          onCancel={() => setRemoving(null)}
        >
          {removing.name} can no longer log in; any device logged in as {removing.name} is logged out now.
        </ConfirmDialog>
      )}
    </section>
  )
}

function UserDialog(props: {
  session: Session
  user: UserSummary | null
  firstAdmin: boolean
  admins: number
  onClose: () => void
  onSaved: (users: UserSummary[] | null) => void
}) {
  const { user } = props
  const [name, setName] = useState(user?.name ?? '')
  const [role, setRole] = useState<Role>(user?.role ?? (props.firstAdmin ? 'admin' : 'musician'))
  const [pin, setPin] = useState('')
  const [again, setAgain] = useState('')
  const [mixes, setMixes] = useState<MixRef[]>(user?.mixes ?? [])
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const choices = mixChoices(props.session, null)
  // A mix the user has that is no longer offered (removed, or a group bus).
  const extra = mixes.filter((m) => !choices.some((c) => sameMix(c.mix, m)))

  const pinProblem =
    pin === '' && user
      ? null
      : pin.length < PIN_MIN || pin.length > PIN_MAX
        ? `A PIN is ${PIN_MIN} to ${PIN_MAX} characters`
        : pin !== again
          ? 'The two PINs differ'
          : null
  const lastAdmin = user?.role === 'admin' && props.admins <= 1 && role !== 'admin'
  const problem =
    name.trim() === ''
      ? 'A name is needed'
      : pinProblem ?? (lastAdmin ? 'The last admin stays an admin: add another admin first' : null)

  const save = async () => {
    if (problem) {
      setError(problem)
      return
    }
    setBusy(true)
    setError(null)
    const keptMixes = role === 'musician' ? mixes : []
    const reply = user
      ? await request({
          cmd: 'user_set',
          name: user.name,
          ...(name.trim() !== user.name ? { new_name: name.trim() } : {}),
          ...(role !== user.role ? { role } : {}),
          ...(pin !== '' ? { pin } : {}),
          mixes: keptMixes,
        })
      : await request({ cmd: 'user_add', name: name.trim(), role, pin, mixes: keptMixes })
    setBusy(false)
    if (!reply.ok) {
      setError(explain(reply.error))
      return
    }
    // The first admin: this page is now logged in as them.
    if (typeof reply.token === 'string') keepToken(reply.token)
    props.onSaved(Array.isArray(reply.users) ? (reply.users as UserSummary[]) : null)
    notify(user ? `${name.trim()} saved` : `${name.trim()} added`)
    props.onClose()
  }

  const toggleMix = (mix: MixRef, on: boolean) =>
    setMixes(on ? [...mixes.filter((m) => !sameMix(m, mix)), mix] : mixes.filter((m) => !sameMix(m, mix)))

  return (
    <Modal
      small
      icon={user ? <Pencil size={16} /> : <UserPlus size={16} />}
      title={user ? `Edit ${user.name}` : props.firstAdmin ? 'The first admin' : 'Add a user'}
      subtitle={props.firstAdmin ? 'From now on this console asks for a PIN' : undefined}
      onClose={props.onClose}
      footer={
        <>
          {error && <span className="users-error">{error}</span>}
          <span className="spacer" />
          <button type="button" className="button" onClick={props.onClose}>
            Cancel
          </button>
          <button type="button" className="button primary" disabled={busy} onClick={() => void save()}>
            {user ? 'Save' : 'Add'}
          </button>
        </>
      }
    >
      <label className="dialog-field">
        <span>Name</span>
        <input
          className="text-input"
          autoFocus
          value={name}
          maxLength={40}
          autoComplete="off"
          onChange={(e) => setName(e.currentTarget.value)}
        />
      </label>
      <div className="dialog-field">
        <span>Role</span>
        <div className="segments users-roles">
          {ROLES.map((r) => (
            <button
              key={r}
              type="button"
              className={role === r ? 'on' : ''}
              disabled={props.firstAdmin && r !== 'admin'}
              title={props.firstAdmin && r !== 'admin' ? 'The first user must be an admin' : ROLE_DETAIL[r]}
              onClick={() => setRole(r)}
            >
              {ROLE_LABEL[r]}
            </button>
          ))}
        </div>
        <span className="dialog-hint">{ROLE_DETAIL[role]}</span>
      </div>
      <div className="users-pins">
        <label className="dialog-field">
          <span>{user ? 'New PIN' : 'PIN'}</span>
          <input
            className="text-input"
            type="password"
            inputMode="numeric"
            autoComplete="new-password"
            placeholder={user ? 'unchanged' : `${PIN_MIN}–${PIN_MAX} digits`}
            value={pin}
            maxLength={PIN_MAX}
            onChange={(e) => setPin(e.currentTarget.value)}
          />
        </label>
        <label className="dialog-field">
          <span>Again</span>
          <input
            className="text-input"
            type="password"
            inputMode="numeric"
            autoComplete="new-password"
            value={again}
            maxLength={PIN_MAX}
            disabled={pin === '' && user !== null}
            onChange={(e) => setAgain(e.currentTarget.value)}
          />
        </label>
      </div>
      {role === 'musician' && (
        <div className="dialog-field">
          <span>Mixes {name.trim() || 'this musician'} may change</span>
          {choices.length === 0 && extra.length === 0 ? (
            <span className="dialog-hint">The show has no aux bus or matrix yet.</span>
          ) : (
            <div className="users-mix-grid">
              {[...choices.filter((c) => !c.missing), ...extra.map((m) => ({ mix: m, name: mixName(props.session, m), detail: '' }))].map(
                (c) => {
                  const on = mixes.some((m) => sameMix(m, c.mix))
                  return (
                    <label key={`${c.mix.kind}:${c.mix.id}`} className={`tb-dest${on ? ' on' : ''}`}>
                      <input type="checkbox" checked={on} onChange={() => toggleMix(c.mix, !on)} />
                      <span className="tb-dest-name">{c.name}</span>
                    </label>
                  )
                },
              )}
            </div>
          )}
          <span className="dialog-hint">
            Sends to these mixes (level and pan), and their own fader and mute. Nothing else on the console.
          </span>
        </div>
      )}
    </Modal>
  )
}
