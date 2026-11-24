/**
 * 统一排序组（一级菜单「统一排序」）。
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
 * ── 版面 ──────────────────────────────────────────────────────
 * 账号页同款面板：顶部工具栏 + 一张按组折叠的表。每个组一段（组头显示人数、
 * 提供商与规则摘要，右侧「编辑规则」开右侧抽屉）；组内是成员行，表尾是未分组
 * 账号，行内下拉直接加入某个组。保存语义（整份组列表 + 按组 id 存 accountSort）
 * 与旧弹窗一致，后端零改动。
 */

import * as React from 'react'
import { createRoot } from 'react-dom/client'
import { flushSync } from 'react-dom'
import {
  Button, Card, Empty, Input, Select, SelectContent, SelectItem, SelectTrigger, SelectValue,
  Sheet, SheetBody, SheetContent, SheetFooter, SheetHeader, SheetTitle, Spinner, cn,
} from '@ui'
import { mountOnPage } from '../island-lazy'
import { t } from '../i18n'
import {
  shared, toast, type AccountGroupRecord, type AccountRecord, type AccountSorting, type GroupSortRule,
} from './accounts-shared'
import { allAccounts } from './accounts-data'
import { providerOf, supportsCreditExpiry, supportsUsage } from './accounts-domain'
import { RuleRow, type RuleState } from './accounts-group-settings'
import { balanceSortStateOf } from '../../../ui-shared/account-sorting'
import '../styles/sorting-page.css'

const PAGE_SELECTOR = '.page[data-page="sorting"]'

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

function cloneGroups(groups: AccountGroupRecord[]): AccountGroupRecord[] {
  return groups.map(group => ({ ...group, accounts: [...group.accounts] }))
}

function errorText(failure: unknown): string {
  return failure instanceof Error ? failure.message : String(failure)
}

function ruleLabel(rule: GroupSortRule): string {
  if (rule.key === 'credit') return rule.dir === 'late' ? t('最晚到期优先') : t('最快到期优先')
  if (rule.key === 'balance') return rule.dir === 'low' ? t('余额小优先') : t('余额大优先')
  return rule.dir === 'old' ? t('最早添加优先') : t('最新添加优先')
}

function SortingPage() {
  const api = shared().workbuddyDesktop
  const [accounts, setAccounts] = React.useState<AccountRecord[]>(() => allAccounts())
  const [groups, setGroups] = React.useState<AccountGroupRecord[]>([])
  /** 最近一次加载 / 保存后的形态：只用来判断「有没有未保存的改动」 */
  const [loadedGroups, setLoadedGroups] = React.useState<AccountGroupRecord[]>([])
  const [sorting, setSorting] = React.useState<AccountSorting | null>(null)
  /** 本次改过的组规则（键 = 组 id）；没改过的组沿用落库值 */
  const [drafts, setDrafts] = React.useState<Record<string, GroupSortRule[]>>({})
  /** 打开时已有的组 id：保存时据此清掉被删组的孤儿规则 */
  const loadedIds = React.useRef<string[]>([])
  /** 右侧规则抽屉当前编辑的组 */
  const [editingId, setEditingId] = React.useState<string | null>(null)
  const [loading, setLoading] = React.useState(true)
  const [busy, setBusy] = React.useState(false)
  const [error, setError] = React.useState('')
  const [query, setQuery] = React.useState('')

  const syncAccounts = React.useCallback(() => {
    const next = allAccounts()
    setAccounts(previous => previous === next ? previous : next)
  }, [])

  // 账号快照由 app.js 的全局 refresh 维护：这里只读它的 state，并在进入页面时
  // 主动刷一次。低频比对引用即可 —— 没变化时 applyAccounts 会复用同一份数组。
  React.useEffect(() => {
    let alive = true
    const sync = () => { if (alive) syncAccounts() }
    sync()
    void shared().wbApp?.refresh?.().then(sync).catch(() => undefined)
    const timer = window.setInterval(sync, 5000)
    return () => { alive = false; window.clearInterval(timer) }
  }, [syncAccounts])

  const load = React.useCallback(async () => {
    setLoading(true)
    setError('')
    try {
      // 组列表与规则集一起取：规则可用性要按成员真实 provider 的余额 / 到期读数判。
      const [groupResult, sortingResult] = await Promise.all([
        api?.getAccountGroups?.(), api?.getAccountSorting?.(),
      ])
      const list = Array.isArray(groupResult?.groups) ? groupResult!.groups! : []
      const copy = cloneGroups(list)
      setGroups(copy)
      setLoadedGroups(cloneGroups(copy))
      loadedIds.current = copy.map(group => group.id)
      setSorting(sortingResult || null)
      setDrafts({})
      setEditingId(previous => copy.some(group => group.id === previous) ? previous : null)
    } catch (failure) {
      setError(errorText(failure))
    } finally {
      setLoading(false)
    }
  }, [api])

  React.useEffect(() => { void load() }, [load])

  const reloadAll = async () => {
    await load()
    await shared().wbApp?.refresh?.().catch(() => undefined)
    syncAccounts()
  }

  const totalMembers = groups.reduce((sum, group) => sum + group.accounts.length, 0)
  const groupedIds = React.useMemo(() => {
    const ids = new Set<string>()
    for (const group of groups) for (const id of group.accounts) ids.add(id)
    return ids
  }, [groups])

  const nameOf = React.useMemo(() => {
    const names = new Map<string, string>()
    for (const account of accounts) {
      names.set(account.id, String(account.name || account.email || account.nickname || account.id))
    }
    return names
  }, [accounts])

  const providerOfId = React.useMemo(() => {
    const providers = new Map<string, string>()
    for (const account of accounts) providers.set(account.id, providerOf(account))
    return providers
  }, [accounts])

  const visibleAccounts = React.useMemo(() => {
    const keyword = query.trim().toLowerCase()
    if (!keyword) return accounts
    return accounts.filter(account => {
      const name = nameOf.get(account.id) || ''
      return name.toLowerCase().includes(keyword) || providerOf(account).toLowerCase().includes(keyword)
    })
  }, [accounts, nameOf, query])

  const visibleIds = React.useMemo(() => new Set(visibleAccounts.map(account => account.id)), [visibleAccounts])
  const ungrouped = visibleAccounts.filter(account => !groupedIds.has(account.id))
  const visibleGroups = query.trim()
    ? groups.filter(group => group.accounts.some(id => visibleIds.has(id)))
    : groups
  const noMatch = Boolean(query.trim()) && !visibleGroups.length && !ungrouped.length

  const membersOf = React.useCallback((group: AccountGroupRecord): AccountRecord[] => {
    const ids = new Set(group.accounts)
    return accounts.filter(account => ids.has(account.id))
  }, [accounts])

  const providersOf = React.useCallback((group: AccountGroupRecord): string => {
    const unique = [...new Set(group.accounts.map(id => providerOfId.get(id)).filter((value): value is string => Boolean(value)))]
    return unique.length > 3 ? `${unique.slice(0, 3).join(' · ')}…` : unique.join(' · ')
  }, [providerOfId])

  /** 组的有效规则：改过的用改后的，否则用落库值，再否则按块内能力生成默认。 */
  const rulesOf = React.useCallback((group: AccountGroupRecord): GroupSortRule[] => {
    const edited = drafts[group.id]
    if (edited) return edited
    const saved = sorting?.configured?.[group.id]
    if (Array.isArray(saved) && saved.length) return saved
    const members = membersOf(group)
    return defaultRules(members.some(account => supportsCreditExpiry(providerOf(account))))
  }, [drafts, sorting, membersOf])

  const summaryOf = (group: AccountGroupRecord): string => {
    const active = rulesOf(group).filter(rule => rule.on)
    return active.length ? active.map(ruleLabel).join(' · ') : t('仅按手动顺序')
  }

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

  const dirty = Object.keys(drafts).length > 0 || JSON.stringify(groups) !== JSON.stringify(loadedGroups)
  const drawerGroup = editingId ? groups.find(group => group.id === editingId) || null : null

  const mutate = (id: string, patch: Partial<AccountGroupRecord>) => {
    setGroups(current => current.map(group => group.id === id ? { ...group, ...patch } : group))
    setError('')
  }

  /** groupId = null 表示移出组；否则从别的组挪进来（一个账号只属一个组） */
  const moveAccount = (accountId: string, groupId: string | null) => {
    setGroups(current => current.map(group => {
      const has = group.accounts.includes(accountId)
      if (group.id === groupId) return has ? group : { ...group, accounts: [...group.accounts, accountId] }
      return has ? { ...group, accounts: group.accounts.filter(id => id !== accountId) } : group
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

  const addGroup = () => {
    const id = newGroupId()
    setGroups(current => [...current, { id, name: t('新排序组'), accounts: [] }])
    setEditingId(id)
    setError('')
  }

  const removeGroup = (id: string) => {
    setGroups(current => current.filter(group => group.id !== id))
    if (editingId === id) setEditingId(null)
    setError('')
  }

  const save = async () => {
    if (busy || loading) return
    const empty = groups.find(group => !group.name.trim())
    if (empty) { setError(t('组名不能为空')); return }
    setBusy(true)
    setError('')
    try {
      // 先落组（后端校验账号真实存在），再落规则 —— 组 id 必须先存在才能当键。
      const result = await api?.saveAccountGroups?.({ groups })
      const saved = Array.isArray(result?.groups) ? result!.groups! : groups
      setGroups(saved)
      setLoadedGroups(cloneGroups(saved))
      // 被删掉的组：规则一起清掉，否则 `accountSort` 里留下没有组可用的孤儿键。
      // 判据用「保存前已有的组 - 保存后的列表」，不能用本地 `groups` —— 本地那份
      // 可能因为保存失败而被回滚，拿它当依据会漏清。
      const remaining = new Set(saved.map(group => group.id))
      for (const id of loadedIds.current) {
        if (remaining.has(id)) continue
        if (sorting?.configured?.[id]) await api?.saveAccountSorting?.({ provider: id, rules: null })
      }
      for (const group of groups) {
        if (!drafts[group.id]) continue
        await api?.saveAccountSorting?.({ provider: group.id, rules: rulesOf(group) })
      }
      loadedIds.current = saved.map(group => group.id)
      setDrafts({})
      const refreshed = await api?.getAccountSorting?.()
      if (refreshed) setSorting(refreshed)
      // 账号页的排序标记要跟着变（组规则也在 accountSort 里）。
      await shared().wbApp?.refresh?.().catch(() => undefined)
      toast(t('已保存'))
    } catch (failure) {
      setError(errorText(failure))
    } finally {
      setBusy(false)
    }
  }

  const accountRow = (account: AccountRecord, groupId: string | null) => <tr key={account.id} className='sorting-account-row'>
    <td className='sorting-account-cell'>
      <span className='sorting-account-name'>{nameOf.get(account.id) || account.id}</span>
    </td>
    <td className='sorting-provider-cell'>{providerOfId.get(account.id) || ''}</td>
    <td className='sorting-row-actions'>
      {groupId ? <Button variant='ghost' size='sm' disabled={busy}
        onClick={() => moveAccount(account.id, null)}>{t('移出组')}</Button>
        : groups.length ? <Select value='' onValueChange={value => moveAccount(account.id, value)}>
          <SelectTrigger className='sorting-assign' aria-label={t('加入组')}>
            <SelectValue placeholder={t('加入组')} />
          </SelectTrigger>
          <SelectContent>
            {groups.map(group => <SelectItem key={group.id} value={group.id}>{group.name || t('新排序组')}</SelectItem>)}
          </SelectContent>
        </Select> : <span className='sorting-note'>—</span>}
    </td>
  </tr>

  const groupSection = (group: AccountGroupRecord) => {
    const members = membersOf(group).filter(account => visibleIds.has(account.id))
    const count = query.trim() ? members.length : group.accounts.length
    return <React.Fragment key={group.id}>
      <tr className='sorting-group-row'>
        <td colSpan={3}>
          <div className='sorting-group-head'>
            <span className='sorting-group-title'>{group.name || t('新排序组')}</span>
            <span className='sorting-group-meta'>{t('{n} 个账号', { n: count })}</span>
            <span className='sorting-group-providers'>{providersOf(group)}</span>
            <span className='sorting-group-rules' title={summaryOf(group)}>{summaryOf(group)}</span>
            <Button variant='ghost' size='sm' disabled={busy} className='sorting-edit-rule'
              onClick={() => setEditingId(group.id)}>{t('编辑规则')}</Button>
          </div>
        </td>
      </tr>
      {members.length ? members.map(account => accountRow(account, group.id))
        : <tr><td colSpan={3} className='sorting-empty-cell'>{t('组内还没有账号。')}</td></tr>}
    </React.Fragment>
  }

  return <Card render={<section />} className='panel sorting-panel'>
    <div className='toolbar'>
      <div className='group'>
        <span className='label'>{t('搜索')}</span>
        <Input className='sorting-search' type='search' value={query} disabled={busy || loading}
          placeholder={t('搜索账号')} aria-label={t('搜索账号')}
          onChange={event => setQuery(event.currentTarget.value)} />
      </div>
      <div className='divider' />
      <div className='group sorting-summary'
        title={t('把多家的账号放进同一个组，它们会按同一套消耗规则排在一起 —— 积分到期近→远、余额大优先因此能跨家生效。账号本身不搬家，签到、旅行、成长任务都不受影响。')}>
        <span className='label'>{t('统一排序组')}</span>
        <strong className='sorting-summary-count'>{groups.length}</strong>
        <span className='sorting-summary-muted'>· {t('{n} 个账号', { n: totalMembers })}</span>
        {dirty ? <span className='sorting-dirty-chip'>{t('有未保存的改动')}</span> : null}
      </div>
      <div className='actions'>
        <Button variant='outline' disabled={busy || loading} onClick={() => void reloadAll()}>{loading ? t('刷新中…') : t('刷新')}</Button>
        <Button variant='outline' disabled={busy || loading} onClick={addGroup}>{t('新建组')}</Button>
        <Button variant='outline' disabled={busy || loading || !dirty} onClick={() => void load()}>{t('撤销')}</Button>
        <Button variant='default' disabled={busy || loading} onClick={() => void save()}>{busy ? t('保存中…') : t('保存')}</Button>
      </div>
    </div>
    {error ? <p className='sorting-error' role='alert'>{error}</p> : null}

    <div className='sorting-table-wrap'>
      <table className='sorting-table'>
        <thead>
          <tr>
            <th className='sorting-account-cell'>{t('账号')}</th>
            <th className='sorting-provider-cell'>{t('提供商')}</th>
            <th className='sorting-row-actions'>{t('操作')}</th>
          </tr>
        </thead>
        <tbody>
          {loading ? <tr><td colSpan={3}><Empty className='empty'><Spinner />{t('加载中…')}</Empty></td></tr> : null}
          {!loading && !groups.length ? <tr><td colSpan={3}>
            <Empty className='empty'>{t('还没有统一排序组。新建一个，然后把各家的账号勾进来。')}</Empty>
          </td></tr> : null}
          {!loading ? visibleGroups.map(groupSection) : null}
          {!loading && ungrouped.length ? <>
            <tr className='sorting-group-row sorting-ungrouped-row'>
              <td colSpan={3}>
                <div className='sorting-group-head'>
                  <span className='sorting-group-title'>{t('未分组账号')}</span>
                  <span className='sorting-group-meta'>{t('{n} 个账号', { n: ungrouped.length })}</span>
                </div>
              </td>
            </tr>
            {ungrouped.map(account => accountRow(account, null))}
          </> : null}
          {!loading && noMatch ? <tr><td colSpan={3}><Empty className='empty'>{t('没有匹配的选项')}</Empty></td></tr> : null}
        </tbody>
      </table>
    </div>

    <Sheet open={Boolean(drawerGroup)} onOpenChange={open => { if (!open) setEditingId(null) }}>
      {drawerGroup ? <SheetContent side='right' showHandle={false} className='sorting-drawer w-[min(520px,94vw)]'>
        <SheetHeader>
          <SheetTitle>{t('{name} · 分组设置', { name: drawerGroup.name || t('新排序组') })}</SheetTitle>
        </SheetHeader>
        <SheetBody className='sorting-drawer-body'>
          <div className='sorting-field'>
            <span className='sorting-field-label'>{t('组名')}</span>
            <Input value={drawerGroup.name} disabled={busy} aria-label={t('组名')}
              placeholder={t('组名，例如「主力池」')}
              onChange={event => mutate(drawerGroup.id, { name: event.currentTarget.value })} />
          </div>
          <div className='sorting-field'>
            <span className='sorting-field-label'>{t('消耗顺序')}</span>
            {rulesOf(drawerGroup).map((rule, index, list) => <RuleRow key={rule.key} rule={rule}
              priority={index + 1} isLast={index === list.length - 1}
              state={stateOf(drawerGroup, rule.key)} busy={busy}
              onChange={patch => changeRule(drawerGroup, rule.key, patch)} />)}
            <p className='sorting-hint'>{t('按优先级从上到下比较，勾选的项才参与')}</p>
          </div>
        </SheetBody>
        <SheetFooter className='sorting-drawer-foot'>
          <Button variant='ghost' disabled={busy} onClick={() => removeGroup(drawerGroup.id)}>{t('删除')}</Button>
          <Button variant='outline' onClick={() => setEditingId(null)}>{t('完成')}</Button>
          <Button variant='default' disabled={busy || loading} onClick={() => void save()}>{busy ? t('保存中…') : t('保存')}</Button>
        </SheetFooter>
      </SheetContent> : null}
    </Sheet>
  </Card>
}

function mount(): void {
  const node = document.querySelector<HTMLElement>(PAGE_SELECTOR)
  if (!node) return
  flushSync(() => createRoot(node).render(<SortingPage />))
}

mountOnPage('sorting', PAGE_SELECTOR, mount)
