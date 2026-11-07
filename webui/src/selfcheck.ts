// Minimal checks for the branches that are easy to regress and hard to see.
// Not imported by the bundle; run with: node --experimental-strip-types src/selfcheck.ts

import { applyStatusPatch, emptyStatusRow, parseProbe } from './data.ts'
import { billingNoteCopy, withAccountName } from './format.ts'
import { dict } from './text.ts'

let failures = 0

function check(name: string, ok: boolean, detail: unknown = ''): void {
  if (ok) return
  failures += 1
  console.error(`FAIL ${name}`, detail)
}

// 1. A probe response only carries probe fields; credit and billing terms stay.
const linked = { ...emptyStatusRow('alice'), credit: { ok: true, balanceUsd: 1, capUsd: 2, usedUsd: 1, usedPct: 50, resetsAt: null, error: null }, orbConfigured: true, overdue: true }
const probe = parseProbe({ probe: { model: 'm' }, accounts: [{ name: 'alice', model_state: 'healthy', model_checked_at: 5 }] })
const merged = applyStatusPatch(linked, probe.patches[0]!)
check('probe keeps credit', merged.credit?.ok === true)
check('probe keeps orb link', merged.orbConfigured === true)
check('probe keeps overdue flag', merged.overdue === true)
check('probe writes model state', merged.modelState === 'healthy')
check('probe does not invent a token verdict', merged.tokenOk === null)

// 2. Billing flags only speak when the server said true.
check('no flags yields no note', billingNoteCopy(null, null, 'en') === null)
check('false flags stay silent', billingNoteCopy(false, false, 'zh') === null)
check('overdue names its reason', (billingNoteCopy(true, null, 'en') ?? '').includes('overdue'))
check('too young names its reason', (billingNoteCopy(null, true, 'zh') ?? '').includes('过新'))

// 3. Visible text must lead the accessible name for each account action.
for (const key of ['billingLink', 'probeOne', 'orbSave', 'orbUnlink', 'switchTo', 'probeOneBusy'] as const) {
  for (const lang of [0, 1] as const) {
    const visible = dict[key][lang]
    const label = withAccountName(visible, 'alice')
    check(`label keeps visible text: ${key}/${lang}`, label.toLowerCase().includes(visible.toLowerCase()), label)
  }
}

// 4. None of the retired interface pairs survive verbatim.
const retired: [string, string][] = [
  ['控制台', 'Console'],
  ['模型', 'Models'],
  ['未知', 'Unknown'],
  ['未知', 'unknown'],
  ['冷却中', 'Cooling down'],
  ['未选择', 'None selected'],
  ['未选择', 'none selected'],
  ['套餐', 'Plan'],
  ['额度', 'Quota'],
  ['尚未检查', 'Not checked'],
  ['默认档位', 'Default level'],
  ['重新检查', 'Run checks again'],
]
for (const [zh, en] of retired) {
  const hit = Object.entries(dict).filter(([, pair]) => pair[0] === zh || pair[1] === en)
  check(`retired pair gone: ${zh}/${en}`, hit.length === 0, hit.map(([key]) => key))
}

if (failures > 0) throw new Error(`${failures} self-checks failed`)
console.log('SELFCHECK OK')
