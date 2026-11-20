import * as React from 'react'
import { Button, Card, Input, Label, Switch } from '@ui'
import { setClaimAuto, store } from './checkin-store'
import { t } from '../i18n'

/**
 * WorkBuddy 成长中心的「到期前自动领取」设置行。
 *
 * 挂在「签到与成长自动任务」卡片内，与插件任务同一套交互：开关 + 北京时间
 * 多个 HH:MM（逗号/空格分隔）+ 保存。后端按设置的时刻逐拍执行；跨重启只在
 * 启动后补跑最近一个已过时段一次（见 `core::growth_claim_auto`）。
 */
export function ClaimAutoSettings(): React.ReactElement | null {
  const claim = store.center?.claimAuto
  const [draft, setDraft] = React.useState<string | undefined>(undefined)
  if (!claim) return null
  const enabled = claim.enabled === true
  const current = claim.times?.length ? claim.times.join(', ') : ''
  const value = draft ?? current
  return (
    <Card className='min-w-0 p-3'>
      <div className='flex flex-wrap items-center gap-3'>
        <Label className='flex min-w-0 flex-1 items-center gap-2 text-[13px]'>
          <Switch
            checked={enabled}
            disabled={store.saving}
            onCheckedChange={next => void setClaimAuto({ enabled: next })}
          />
          <span className='break-words'>{t('到期自动领取')}</span>
        </Label>
      </div>
      <div className='mt-3 flex flex-wrap items-center gap-2'>
        <Input
          className='min-w-40 flex-1'
          aria-label={t('执行时刻')}
          disabled={store.saving}
          value={value}
          onChange={event => setDraft(event.currentTarget.value)}
        />
        <span className='text-xs text-muted-foreground'>UTC +8</span>
        <Button
          size='sm'
          disabled={store.saving || draft === undefined}
          onClick={() => {
            const times = value.split(/[,，\s]+/).map(item => item.trim()).filter(Boolean)
            void setClaimAuto({ times }).then(saved => { if (saved) setDraft(undefined) })
          }}
        >
          {t('保存')}
        </Button>
      </div>
      <p className='mb-0 mt-3 text-xs leading-relaxed text-muted-foreground'>
        {t('按设置的时刻（北京时间）检查：把快过期（今天 ≥ 到期日 − 1 天）的成长任务领回来（已完成直接领；未完成先点亮再领）。仅 WorkBuddy 国内版；会真发起写请求，请确认后再开。')}
      </p>
      <p className='mb-0 mt-1 text-xs leading-relaxed text-muted-foreground'>
        {t('对「有结束时间」的任务生效：在前一天起把已完成的、可完成的领回来，动作记入「签到记录」。也可在「签到中心」页成长任务行点「巡检一次」当场跑一轮。')}
      </p>
    </Card>
  )
}
