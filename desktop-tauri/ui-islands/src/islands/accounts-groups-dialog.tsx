/**
 * 统一排序组管理弹窗（账号页工具栏「统一排序」那颗按钮打开的）。
 *
 * ── 它解决的是什么 ────────────────────────────────────────────
 * 账号页的消耗规则一直是**按家**生效的：workbuddy 内部按「到期近→远」排好了，
 * 但 workbuddy 整块仍然排在 trae 前面 —— 只有等它家积分耗尽才会轮到下一家。
 * 把两家的账号放进同一个组之后，它们进同一块、共用一套规则，
 * 「到期近→远 / 余额大优先」才真正跨家生效。
 *
 * ── 边界：只影响路由 ──────────────────────────────────────────
 * 组只是一份账号 id 的引用列表。账号不搬家（`provider`、凭证都不变），
 * 签到 / 旅行 / 成长这些后台任务**完全读不到它** —— 它们读的是账号集合，
 * 从来没有读过排序配置。所以「加入组」不会让任何定时任务多跑或少跑一个账号。
 *
 * ── 规则从哪儿来 ──────────────────────────────────────────────
 * 复用 `accountSort` 里以**组 id** 为键的那一份，规则行直接用分组设置弹窗的
 * `RuleRow`，界面与账号页的「消耗顺序」完全一致；没配过时按默认（块内有账号
 * 支持到期就按到期近→远）。
 */

import * as React from 'react'
import {
  Button, Checkbox, Dialog, DialogBody, DialogContent, DialogFooter, DialogHeader, DialogTitle,
  Input, Label, cn,
} from '@ui'
import { t } from '../i18n'
import {
  shared, type AccountGroupRecord, type AccountRecord, type AccountSorting, type GroupSortRule,
} from './accounts-shared'
import { providerOf, supportsCreditExpiry, supportsUsage } from './accounts-domain'
import { RuleRow, type RuleState } from './accounts-group-settings'
import { balanceSortStateOf } from '../../../ui-shared/account-sorting'
import '../styles/account-groups-dialog.css'

function newGroupId(): string {
  return `g${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`
}

function defaultRules(creditSupported: boolean): GroupSortRule[] {
  return [
    { key: 'credit', on: creditSupported, dir: 'soon' },
    { key: 'balance', on: false, dir: 'high' },
    { key: 'added', on: false, dir: 'new' },
  ]
}

export function AccountGroupsDialog({ accounts, onClose }: {
  accounts: AccountRecord[]
  onClose(): void
}) {
  const api = shared().workbuddyDesktop
  const [groups, setGroups] = React.useState<AccountGroupRecord[]>([])
  const [sorting, setSorting] = React.useState<AccountSorting | null>(null)
  /** 本次改过的组规则（键 = 组 id）；没改过的组沿用落库值 */
  const [drafts, setDrafts] = React.useState<Record<string, GroupSortRule[]>>({})
  /** 打开时已有的组 id：保存时据此清掉被删组的孤儿规则 */
  const loadedIds = React.useRef<string[]>([])
  const [loading, setLoading] = React.useState(true)
  const [busy, setBusy] = React.useState(false)
  const [error, setError] = React.useState('')
  const [query, setQuery] = React.useState('')

  React.useEffect(() => {
    let alive = true
    void (async () => {
      try {
        // 组列表与规则集一起取：规则可用性要按成员真实 provider 的余额 / 到期读数判。
        const [groupResult, sortingResult] = await Promise.all([
          api?.getAccountGroups?.(), api?.getAccountSorting?.(),
        ])
        if (!alive) return
        const list = Array.isArray(groupResult?.groups) ? groupResult!.groups! : []
        setGroups(list)
        loadedIds.current = list.map(group => group.id)
        setSorting(sortingResult || null)
      } catch (failure) {
        if (alive) setError(failure instanceof Error ? failure.message : String(failure))
      } finally { if (alive) setLoading(false) }
    })()
    return () => { alive = false }
  }, [api])

  // 一个账号只能属于一个组：归块必须唯一，否则一次请求可能对它试两次。
  const ownerOf = React.useMemo(() => {
    const owners = new Map<string, string>()
    for (const group of groups) for (const id of group.accounts) owners.set(id, group.id)
    return owners
  }, [groups])

  const labelOf = React.useMemo(() => {
    const labels = new Map<string, string>()
    for (const account of accounts) {
      const name = String(account.name || account.email || account.nickname || account.id)
      labels.set(account.id, `${name} · ${providerOf(account)}`)
    }
    return labels
  }, [accounts])

  const filtered = React.useMemo(() => {
    const keyword = query.trim().toLowerCase()
    return accounts.filter(account => !keyword || (labelOf.get(account.id) || '').toLowerCase().includes(keyword))
  }, [accounts, labelOf, query])

  const membersOf = React.useCallback((group: AccountGroupRecord): AccountRecord[] => {
    const ids = new Set(group.accounts)
    return accounts.filter(account => ids.has(account.id))
  }, [accounts])

  /** 组的有效规则：改过的用改后的，否则用落库值，再否则按块内能力生成默认。 */
  const rulesOf = React.useCallback((group: AccountGroupRecord): GroupSortRule[] => {
    const edited = drafts[group.id]
    if (edited) return edited
    const saved = sorting?.configured?.[group.id]
    if (Array.isArray(saved) && saved.length) return saved
    const members = membersOf(group)
    return defaultRules(members.some(account => supportsCreditExpiry(providerOf(account))))
  }, [drafts, sorting, membersOf])

  /**
   * 规则可用性。**按成员的真实 provider 判能力** —— 组 id 不是一家，
   * 拿它去问 `supportsCreditExpiry` 会恒假、整条规则静默消失。
   */
  const stateOf = (group: AccountGroupRecord, key: GroupSortRule['key']): RuleState => {
    if (key === 'added') return 'ready'
    const members = membersOf(group)
    if (key === 'credit') {
      if (!members.some(account => supportsCreditExpiry(providerOf(account)))) return 'unsupported'
      return members.some(account => sorting?.creditExpiresAt?.[account.id] != null) ? 'ready' : 'no-data'
    }
    if (!members.some(account => supportsUsage(account))) return 'unsupported'
    const balances = members
      .map(account => sorting?.balances?.[account.id])
      .filter(balance => balance !== undefined)
    if (!balances.length) return 'no-data'
    const state = balanceSortStateOf(balances)
    return state === 'mixedUnits' ? 'mixed-units' : state === 'noData' ? 'no-data' : 'ready'
  }

  const mutate = (id: string, patch: Partial<AccountGroupRecord>) => {
    setGroups(current => current.map(group => group.id === id ? { ...group, ...patch } : group))
    setError('')
  }
  const toggleAccount = (groupId: string, accountId: string) => {
    setGroups(current => current.map(group => {
      if (group.id !== groupId) return group
      const has = group.accounts.includes(accountId)
      return { ...group, accounts: has ? group.accounts.filter(id => id !== accountId) : [...group.accounts, accountId] }
    }))
    setError('')
  }

  const changeRule = (group: AccountGroupRecord, key: GroupSortRule['key'], patch: Partial<GroupSortRule>) => {
    const next = rulesOf(group).map(rule => ({ ...rule }))
    const index = next.findIndex(rule => rule.key === key)
    if (index < 0) return
    const move = (patch as unknown as { __move?: 'up' | 'down' }).__move
    if (move) {
      // 上移 / 下移改的是优先级（数组位置），与账号页「消耗顺序」同一口径
      const to = move === 'up' ? index - 1 : index + 1
      if (to < 0 || to >= next.length) return
      const [moved] = next.splice(index, 1)
      next.splice(to, 0, moved)
    } else {
      next[index] = { ...next[index], ...patch } as GroupSortRule
    }
    setDrafts(current => ({ ...current, [group.id]: next }))
    setError('')
  }

  const save = async () => {
    if (busy) return
    const empty = groups.find(group => !group.name.trim())
    if (empty) { setError(t('组名不能为空')); return }
    setBusy(true)
    setError('')
    try {
      // 先落组（后端校验账号真实存在），再落规则 —— 组 id 必须先存在才能当键。
      const result = await api?.saveAccountGroups?.({ groups })
      setGroups(Array.isArray(result?.groups) ? result!.groups : groups)
      // 被删掉的组：规则一起清掉，否则 `accountSort` 里留下没有组可用的孤儿键。
      // 判据用「打开时的列表 - 保存后的列表」，不能用本地 `groups` —— 本地那份
      // 可能因为保存失败而被回滚，拿它当依据会漏清。
      const remaining = new Set((Array.isArray(result?.groups) ? result!.groups! : groups).map(group => group.id))
      for (const id of loadedIds.current) {
        if (remaining.has(id)) continue
        if (sorting?.configured?.[id]) await api?.saveAccountSorting?.({ provider: id, rules: null })
      }
      for (const group of groups) {
        if (!drafts[group.id]) continue
        await api?.saveAccountSorting?.({ provider: group.id, rules: rulesOf(group) })
      }
      // 账号页的排序标记要跟着变（组规则也在 accountSort 里）。
      await shared().wbApp?.refresh?.().catch(() => undefined)
      onClose()
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure))
    } finally { setBusy(false) }
  }

  return <Dialog open onOpenChange={(open, details) => {
    if (open) return
    if (busy) { details.cancel(); return }
    onClose()
  }}><DialogContent className='account-groups-dialog'>
    <DialogHeader><DialogTitle>{t('统一排序组')}</DialogTitle></DialogHeader>
    <DialogBody className='account-groups-body'>
      <p className='routing-config-note'>{t('把多家的账号放进同一个组，它们会按同一套消耗规则排在一起 —— 积分到期近→远、余额大优先因此能跨家生效。账号本身不搬家，签到、旅行、成长任务都不受影响。')}</p>
      {error ? <p className='routing-config-error' role='alert'>{error}</p> : null}
      {loading ? <p className='routing-config-note'>{t('加载中…')}</p> : null}
      {!loading && !groups.length ? <p className='routing-config-note'>{t('还没有统一排序组。新建一个，然后把各家的账号勾进来。')}</p> : null}
      {groups.map(group => <section key={group.id} className='account-group-card'>
        <div className='account-group-head'>
          <Input value={group.name} disabled={busy} aria-label={t('组名')} placeholder={t('组名，例如「主力池」')}
            onChange={event => mutate(group.id, { name: event.currentTarget.value })} />
          <span className='account-group-count'>{t('{n} 个账号', { n: group.accounts.length })}</span>
          <Button variant='ghost' size='sm' disabled={busy}
            onClick={() => setGroups(current => current.filter(item => item.id !== group.id))}>{t('删除')}</Button>
        </div>
        {group.accounts.length ? <div className='account-group-members'>
          {group.accounts.map(id => <span key={id} className='account-group-chip'>
            {labelOf.get(id) || id}
            <Button variant='ghost' size='sm' disabled={busy} aria-label={t('移出组')}
              onClick={() => toggleAccount(group.id, id)}>×</Button>
          </span>)}
        </div> : <p className='routing-config-note'>{t('组内还没有账号。')}</p>}
        <div className='account-group-rules'>
          <span className='account-group-rules-title'>{t('消耗顺序')}</span>
          {rulesOf(group).map((rule, index, list) => <RuleRow key={rule.key} rule={rule}
            priority={index + 1} isLast={index === list.length - 1}
            state={stateOf(group, rule.key)} busy={busy}
            onChange={patch => changeRule(group, rule.key, patch)} />)}
          <p className='account-group-rules-hint'>{t('按优先级从上到下比较，勾选的项才参与')}</p>
        </div>
      </section>)}
      <div className='account-group-add'>
        <Button variant='outline' size='sm' disabled={busy}
          onClick={() => setGroups(current => [...current, { id: newGroupId(), name: t('新排序组'), accounts: [] }])}>
          {t('新建组')}</Button>
      </div>
      {groups.length ? <>
        <Input value={query} disabled={busy} placeholder={t('搜索账号')} aria-label={t('搜索账号')}
          onChange={event => setQuery(event.currentTarget.value)} />
        <div className='account-group-picker'>
          {filtered.map(account => {
            const owner = ownerOf.get(account.id)
            const ownerName = owner ? groups.find(group => group.id === owner)?.name || owner : ''
            return <div key={account.id} className='account-group-row' data-taken={owner ? '' : undefined}>
              <span className='account-group-name'>{labelOf.get(account.id)}</span>
              {ownerName ? <span className='account-group-owner'>{t('已在「{name}」', { name: ownerName })}</span> : null}
              <span className='account-group-targets'>
                {groups.map(group => <Label key={group.id} className={cn('account-group-target', owner && owner !== group.id && 'is-disabled')}>
                  <Checkbox checked={group.accounts.includes(account.id)}
                    disabled={busy || Boolean(owner && owner !== group.id)}
                    onCheckedChange={() => toggleAccount(group.id, account.id)} />
                  {group.name}
                </Label>)}
              </span>
            </div>
          })}
        </div>
      </> : null}
    </DialogBody>
    <DialogFooter>
      <Button variant='outline' disabled={busy} onClick={onClose}>{t('取消')}</Button>
      <Button variant='default' disabled={busy || loading} onClick={() => void save()}>{busy ? t('保存中…') : t('保存')}</Button>
    </DialogFooter>
  </DialogContent></Dialog>
}
