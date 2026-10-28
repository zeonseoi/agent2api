export type {
  AutoState, CheckinAccount, CheckinCenter, CheckinResult, CheckinSection, GrowthEntry, HistoryRow,
  KeepaliveState, OnboardingEntry, OnboardingSettlement, OnboardingTask, OutOfScope, PlanEntry,
  ProviderGroup, RecordsPage, RecordRow, ScheduleEntry, TravelAction,
  TravelRunResult, TravelState,
} from '../../../ui-shared/checkin'
export { CHECKIN_SECTION_OPTIONS, modesOf } from '../../../ui-shared/checkin'
import type { RecordRow } from '../../../ui-shared/checkin'

/** 档位 → 按钮文案（`checkin` 是兜底档，不是插件申报值） */
export const MODE_LABEL: Record<string, string> = {
  checkin: '签到',
  keepalive: '保活',
  claim: '领取',
  full: '保活 + 领取',
}

/** 档位 → 悬停说明 */
export const MODE_TITLE: Record<string, string> = {
  checkin: '走这一家的每日签到接口',
  keepalive: '用免费模型与网页会话维持账号活跃 · 不领取奖励',
  claim: '只领取当天的奖励 · 不做保活',
  full: '保活并领取奖励',
}

/** 来源 → 展示名（签到与合并台账共用：旅行 / 成长任务那几行只出现 auto / manual /
 *  startup 三档，与签到同名同义，不另立一套）。 */
export const SOURCE_LABEL: Record<string, string> = {
  schedule: '定时',
  startup: '补签',
  manual: '手动',
  single: '单签',
  activity: '活动',
  auto: '自动',
}

/**
 * 结果 → 展示名与色调。**按字面量**索引，供筛选下拉与会话历史用 —— 下拉选的是
 * `outcome` 这个值本身，而 `failed` 两张表共用（选它会同时筛出签到失败与猫猫
 * 旅行失败），所以这一份里它只能写中性的「失败」。
 *
 * 行上的徽章另有更细的一层：见 `checkin-records-tab` 的 `outcomeView` —— 它按行
 * 来源把 `failed` 再分成「签到失败」/「猫猫旅行失败」/「成长任务领取失败」。
 */
export const OUTCOME_VIEW: Record<string, { label: string; variant: 'success' | 'brand' | 'warning' | 'destructive' | 'outline' }> = {
  // ── 签到 ──
  success: { label: '成功领取', variant: 'success' },
  already: { label: '今日已领', variant: 'brand' },
  pending: { label: '未领取', variant: 'warning' },
  failed: { label: '失败', variant: 'destructive' },
  // ── 猫猫旅行 ──
  departed: { label: '派猫猫旅行', variant: 'brand' },
  claimed: { label: '领取猫猫旅行积分', variant: 'success' },
  adopted: { label: '领养猫猫', variant: 'success' },
  // ── 成长任务 ──
  task_claimed: { label: '领取成长任务', variant: 'success' },
  task_failed: { label: '成长任务领取失败', variant: 'destructive' },
}

/** 行是否来自猫猫旅行 / 成长任务那张表（行号前缀 `g`；见 `core::checkin_ledger`） */
export function fromTravel(rowId: string | undefined): boolean {
  return (rowId ?? '').startsWith('g')
}

/**
 * 一行结果档位的展示形态 —— 比 `OUTCOME_VIEW` 多分一层 `failed`。
 *
 * `failed` 是两张表共用的字面量（签到失败与猫猫旅行失败各写各的，值相同），
 * 光看值分不出是哪条线，得看行来源（`rowId` 前缀）：「签到失败」/「猫猫旅行失败」。
 * 其余档位直接查表；认不出的原样透出（后端加了新档位时不至于显示空白）。
 */
export function outcomeView(row: RecordRow): {
  label: string
  variant: 'success' | 'brand' | 'warning' | 'destructive' | 'outline'
} {
  const outcome = row.outcome ?? ''
  if (outcome === 'failed') {
    return fromTravel(row.rowId)
      ? { label: '猫猫旅行失败', variant: 'destructive' }
      : { label: '签到失败', variant: 'destructive' }
  }
  return OUTCOME_VIEW[outcome] ?? { label: outcome || '—', variant: 'outline' }
}

/** 毫秒时间戳 → 本地可读时间；缺失 / 非法给 `—` */
export function formatTime(value: unknown): string {
  const stamp = Number(value)
  if (!Number.isFinite(stamp) || stamp <= 0) return '—'
  const date = new Date(stamp)
  if (Number.isNaN(date.getTime())) return '—'
  return date.toLocaleString('zh-CN', { hour12: false })
}

/** 毫秒 → `HH:MM`（只关心当天时刻的场合） */
export function formatClock(value: unknown): string {
  const stamp = Number(value)
  if (!Number.isFinite(stamp) || stamp <= 0) return ''
  const date = new Date(stamp)
  if (Number.isNaN(date.getTime())) return ''
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${pad(date.getHours())}:${pad(date.getMinutes())}`
}

/** 毫秒 → `MM-DD HH:MM`（记录表的时间列） */
export function formatShort(value: unknown): string {
  const stamp = Number(value)
  if (!Number.isFinite(stamp) || stamp <= 0) return '—'
  const date = new Date(stamp)
  if (Number.isNaN(date.getTime())) return '—'
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`
}

declare global {
  interface Window {
    wbApp?: {
      showPage?: (name: string) => void
      toast?: (message: string, tone?: string) => void
      readonly currentPage?: string
    }
    wbZcodePlans?: { open(options: { id: string; name?: string }): void }
  }
}
