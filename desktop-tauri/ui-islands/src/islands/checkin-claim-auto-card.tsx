import * as React from 'react'
import { Card, CardHeader, Switch } from '@ui'
import { setClaimAuto, store } from './checkin-store'
import { t } from '../i18n'

/**
 * WorkBuddy 成长中心的「到期前自动领取」开关卡（挂在「定时签到」页）。
 *
 * 「自动派猫猫旅行」由官方的「猫猫旅行定时排程」卡接管（`autoCheckin` 与
 * `growthSchedule` 是两个独立排程，见 `core::growth_scheduler`）—— 这里不再
 * 重复给开关。「到期前自动领取」是官方没有的一档（按任务的 `valid_end` 提前
 * 一天领），仍由本 fork 的独立循环负责（见 `core::growth_claim_auto`）。
 *
 * 开关读数取签到中心快照的 `claimAuto.enabled`，写走 `setClaimAuto`
 * （POST /api/claim-auto，起停后端轮询）。没有国内版账号时后端不给这段，
 * 整卡不渲染。
 */
export function ClaimAutoCard(): React.ReactElement {
  const claimEnabled = store.center?.claimAuto?.enabled === true
  return (
    <Card render={<section />} className='panel ck-growth-card'>
      <CardHeader className='panel-head'>
        <h2>{t('WorkBuddy 成长任务')}</h2>
        <span className='tip-q' data-tip={t('后台每 2 小时检查一次：把快过期（今天 ≥ 到期日 − 1 天）的成长任务领回来（已完成直接领；未完成先点亮再领）。仅 WorkBuddy 国内版；会真发起写请求，请确认后再开。')}></span>
      </CardHeader>
      <div className='p-4'>
        <div className='ck-set-line'>
          <span className='ck-set-k'>{t('到期自动领取')}</span>
          <span className='ck-set-v'>
            <Switch
              checked={claimEnabled}
              disabled={store.saving}
              onCheckedChange={next => void setClaimAuto(next)}
            />
          </span>
        </div>
        <div className='ck-note'>
          {t('对「有结束时间」的任务生效：在前一天起把已完成的、可完成的领回来，动作记入「签到记录」。也可在「签到中心」页成长任务行点「巡检一次」当场跑一轮。')}
        </div>
      </div>
    </Card>
  )
}
