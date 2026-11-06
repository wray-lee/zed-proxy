// One modal implementation for both the key prompt and the unlink confirmation.
// Focus enters on open, Tab cycles inside, Esc cancels, focus returns on close.

import { h } from './dom.ts'

export interface DialogAction {
  label: string
  primary?: boolean
  /** Runs before the dialog closes; returning false keeps it open. */
  onSelect?: () => boolean | void
}

export interface DialogOptions {
  title: string
  body: Node[]
  actions: DialogAction[]
  /** Called when the dialog closes without a primary action. */
  onCancel?: () => void
  initialFocus?: HTMLElement
  closeLabel: string
}

export interface DialogHandle {
  close(): void
  cancel(): void
}

function dialogEl(): HTMLDialogElement {
  const el = document.getElementById('dialog')
  if (!(el instanceof HTMLDialogElement)) throw new Error('dialog element missing')
  return el
}

export function openDialog(options: DialogOptions): DialogHandle {
  const dialog = dialogEl()
  const title = document.getElementById('dialog-title')
  const body = document.getElementById('dialog-body')
  const actions = document.getElementById('dialog-actions')
  if (!title || !body || !actions) throw new Error('dialog scaffold missing')

  title.textContent = options.title
  body.replaceChildren(...options.body)
  actions.replaceChildren()

  const previouslyFocused = document.activeElement instanceof HTMLElement ? document.activeElement : null
  let closed = false

  const finish = (cancelled: boolean): void => {
    if (closed) return
    closed = true
    if (cancelled) options.onCancel?.()
    if (dialog.open) dialog.close()
    document.removeEventListener('keydown', onKeydown, true)
    if (previouslyFocused && previouslyFocused.isConnected) previouslyFocused.focus()
    else document.getElementById('content')?.focus()
  }

  const onKeydown = (event: KeyboardEvent): void => {
    if (!dialog.open) return
    if (event.key === 'Escape') {
      event.preventDefault()
      finish(true)
      return
    }
    if (event.key !== 'Tab') return
    const focusable = [...dialog.querySelectorAll<HTMLElement>('button, input, a[href], select')].filter(
      (node) => !node.hasAttribute('disabled') && node.offsetParent !== null,
    )
    if (focusable.length === 0) return
    const first = focusable[0]
    const last = focusable[focusable.length - 1]
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault()
      last.focus()
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault()
      first.focus()
    }
  }

  const handle: DialogHandle = {
    close: () => finish(false),
    cancel: () => finish(true),
  }

  for (const action of options.actions) {
    actions.append(
      h(
        'button',
        {
          type: 'button',
          class: action.primary ? 'btn btn-primary' : 'btn',
          onclick: () => {
            if (action.onSelect?.() === false) return
            finish(false)
          },
        },
        action.label,
      ),
    )
  }

  actions.append(
    h(
      'button',
      {
        type: 'button',
        class: 'btn btn-icon dialog-close',
        'aria-label': options.closeLabel,
        title: options.closeLabel,
        onclick: () => finish(true),
      },
      '×',
    ),
  )

  document.addEventListener('keydown', onKeydown, true)
  dialog.showModal()
  const target = options.initialFocus ?? actions.querySelector<HTMLElement>('.btn-primary') ?? actions.firstElementChild
  if (target instanceof HTMLElement) target.focus()
  return handle
}

/** Two-action confirmation. Esc and the cancel button close it without running onConfirm. */
export function confirmDialog(
  title: string,
  body: string,
  confirmLabel: string,
  cancelLabel: string,
  closeLabel: string,
  onConfirm: () => void,
): void {
  openDialog({
    title,
    body: [h('p', { class: 'dialog-text' }, body)],
    actions: [{ label: cancelLabel }, { label: confirmLabel, primary: true, onSelect: onConfirm }],
    closeLabel,
  })
}
