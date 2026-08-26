import { useState } from 'react'
import { RefreshCw, Trash2, Database, Zap, Clock } from 'lucide-react'
import { toast } from 'sonner'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { useRequestDetails, useClearRequestDetails } from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'

function formatTime(isoString: string): string {
  const d = new Date(isoString)
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
}

function formatCost(usd: number): string {
  if (usd < 0.001) return `$${usd.toFixed(6)}`
  if (usd < 0.01) return `$${usd.toFixed(4)}`
  return `$${usd.toFixed(3)}`
}

function formatTokens(n: number): string {
  if (n >= 1000000) return `${(n / 1000000).toFixed(1)}M`
  if (n >= 1000) return `${(n / 1000).toFixed(1)}K`
  return String(n)
}

function modelShortName(model: string): string {
  if (model.includes('opus')) return 'Opus'
  if (model.includes('sonnet')) return 'Sonnet'
  if (model.includes('haiku')) return 'Haiku'
  return model
}

function modelColor(model: string): string {
  if (model.includes('opus')) return 'text-purple-600 dark:text-purple-400'
  if (model.includes('sonnet')) return 'text-blue-600 dark:text-blue-400'
  if (model.includes('haiku')) return 'text-emerald-600 dark:text-emerald-400'
  return ''
}

function cacheRatioBar(ratio: number) {
  const pct = Math.round(ratio * 100)
  const color = pct > 70 ? 'bg-emerald-500' : pct > 30 ? 'bg-amber-500' : 'bg-slate-300 dark:bg-slate-600'
  return (
    <div className="flex items-center gap-1.5">
      <div className="w-12 h-1.5 rounded-full bg-slate-200 dark:bg-slate-700 overflow-hidden">
        <div className={`h-full rounded-full ${color}`} style={{ width: `${pct}%` }} />
      </div>
      <span className="text-xs tabular-nums">{pct}%</span>
    </div>
  )
}

export function RequestDetailsPanel() {
  const [limit, setLimit] = useState(100)
  const { data, isLoading, refetch } = useRequestDetails(limit)
  const { mutate: clearDetails, isPending: isClearing } = useClearRequestDetails()

  const handleClear = () => {
    if (!confirm('确定要清空所有请求记录吗？此操作无法撤销。')) return
    clearDetails(undefined, {
      onSuccess: () => toast.success('请求记录已清空'),
      onError: (err) => toast.error('清空失败: ' + extractErrorMessage(err)),
    })
  }

  // 汇总统计
  const records = data?.records || []
  const totalCost = records.reduce((sum, r) => sum + r.costUsd, 0)
  const totalInput = records.reduce((sum, r) => sum + r.inputTokens + r.cachedTokens, 0)
  const totalOutput = records.reduce((sum, r) => sum + r.outputTokens, 0)
  const totalCached = records.reduce((sum, r) => sum + r.cachedTokens, 0)
  const avgCacheRatio = totalInput > 0 ? totalCached / totalInput : 0
  const cacheHitCount = records.filter(r => r.cacheHit).length

  return (
    <div className="space-y-4">
      {/* 汇总统计卡片 */}
      <div className="grid gap-3 grid-cols-2 md:grid-cols-4">
        <Card>
          <CardHeader className="pb-1 pt-3 px-4">
            <CardTitle className="text-xs font-medium text-muted-foreground flex items-center gap-1">
              <Database className="h-3 w-3" /> 记录数
            </CardTitle>
          </CardHeader>
          <CardContent className="px-4 pb-3">
            <div className="text-xl font-bold tabular-nums">{data?.total ?? '-'}</div>
          </CardContent>
        </Card>
        <Card>
          <CardHeader className="pb-1 pt-3 px-4">
            <CardTitle className="text-xs font-medium text-muted-foreground flex items-center gap-1">
              <Zap className="h-3 w-3" /> 缓存命中
            </CardTitle>
          </CardHeader>
          <CardContent className="px-4 pb-3">
            <div className="text-xl font-bold tabular-nums">
              {cacheHitCount}<span className="text-sm font-normal text-muted-foreground">/{records.length}</span>
            </div>
            <div className="text-xs text-muted-foreground">平均命中率 {(avgCacheRatio * 100).toFixed(1)}%</div>
          </CardContent>
        </Card>
        <Card>
          <CardHeader className="pb-1 pt-3 px-4">
            <CardTitle className="text-xs font-medium text-muted-foreground flex items-center gap-1">
              <Clock className="h-3 w-3" /> Token 用量
            </CardTitle>
          </CardHeader>
          <CardContent className="px-4 pb-3">
            <div className="text-xl font-bold tabular-nums">{formatTokens(totalInput + totalOutput)}</div>
            <div className="text-xs text-muted-foreground">入 {formatTokens(totalInput)} / 出 {formatTokens(totalOutput)}</div>
          </CardContent>
        </Card>
        <Card>
          <CardHeader className="pb-1 pt-3 px-4">
            <CardTitle className="text-xs font-medium text-muted-foreground">估算费用</CardTitle>
          </CardHeader>
          <CardContent className="px-4 pb-3">
            <div className="text-xl font-bold tabular-nums">{formatCost(totalCost)}</div>
            <div className="text-xs text-muted-foreground">基于 Anthropic 官方定价</div>
          </CardContent>
        </Card>
      </div>

      {/* 工具栏 */}
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          <span className="text-sm text-muted-foreground">显示条数：</span>
          {[50, 100, 200, 500].map(n => (
            <Button
              key={n}
              size="sm"
              variant={limit === n ? 'default' : 'outline'}
              className="h-7 px-2 text-xs"
              onClick={() => setLimit(n)}
            >
              {n}
            </Button>
          ))}
        </div>
        <div className="flex gap-2">
          <Button size="sm" variant="outline" onClick={() => refetch()} disabled={isLoading}>
            <RefreshCw className={`h-3.5 w-3.5 mr-1 ${isLoading ? 'animate-spin' : ''}`} />
            刷新
          </Button>
          <Button
            size="sm"
            variant="outline"
            className="text-destructive hover:text-destructive"
            onClick={handleClear}
            disabled={isClearing || !data?.total}
          >
            <Trash2 className="h-3.5 w-3.5 mr-1" />
            清空
          </Button>
        </div>
      </div>

      {/* 表格 */}
      {isLoading ? (
        <Card>
          <CardContent className="py-12 text-center text-muted-foreground">
            <RefreshCw className="h-6 w-6 animate-spin mx-auto mb-2" />
            加载中...
          </CardContent>
        </Card>
      ) : records.length === 0 ? (
        <Card>
          <CardContent className="py-12 text-center text-muted-foreground">
            暂无请求记录
          </CardContent>
        </Card>
      ) : (
        <div className="rounded-lg border overflow-hidden">
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b bg-muted/50">
                  <th className="text-left px-3 py-2 font-medium text-muted-foreground">时间</th>
                  <th className="text-left px-3 py-2 font-medium text-muted-foreground">凭据</th>
                  <th className="text-left px-3 py-2 font-medium text-muted-foreground">模型</th>
                  <th className="text-left px-3 py-2 font-medium text-muted-foreground">端点</th>
                  <th className="text-right px-3 py-2 font-medium text-muted-foreground">输入</th>
                  <th className="text-right px-3 py-2 font-medium text-muted-foreground">缓存读取</th>
                  <th className="text-right px-3 py-2 font-medium text-muted-foreground">输出</th>
                  <th className="text-left px-3 py-2 font-medium text-muted-foreground">缓存率</th>
                  <th className="text-right px-3 py-2 font-medium text-muted-foreground">费用</th>
                  <th className="text-center px-3 py-2 font-medium text-muted-foreground">模式</th>
                </tr>
              </thead>
              <tbody>
                {records.map((r, i) => (
                  <tr key={r.requestId + i} className="border-b last:border-0 hover:bg-muted/30 transition-colors">
                    <td className="px-3 py-1.5 text-xs text-muted-foreground tabular-nums whitespace-nowrap">{formatTime(r.recordedAt)}</td>
                    <td className="px-3 py-1.5 text-xs text-muted-foreground tabular-nums whitespace-nowrap">
                      {r.credentialId ? `#${r.credentialId}` : '—'}
                    </td>
                    <td className="px-3 py-1.5">
                      <span className={`font-medium text-xs ${modelColor(r.model)}`}>{modelShortName(r.model)}</span>
                    </td>
                    <td className="px-3 py-1.5 text-xs text-muted-foreground">{r.endpoint.replace('/v1/', '')}</td>
                    <td className="px-3 py-1.5 text-right tabular-nums text-xs">{formatTokens(r.inputTokens)}</td>
                    <td className="px-3 py-1.5 text-right tabular-nums text-xs">
                      {r.cachedTokens > 0 ? (
                        <span className="text-emerald-600 dark:text-emerald-400">{formatTokens(r.cachedTokens)}</span>
                      ) : (
                        <span className="text-muted-foreground">-</span>
                      )}
                    </td>
                    <td className="px-3 py-1.5 text-right tabular-nums text-xs">{formatTokens(r.outputTokens)}</td>
                    <td className="px-3 py-1.5">{cacheRatioBar(r.cacheRatio)}</td>
                    <td className="px-3 py-1.5 text-right tabular-nums text-xs">{formatCost(r.costUsd)}</td>
                    <td className="px-3 py-1.5 text-center">
                      <Badge variant={r.stream ? 'secondary' : 'outline'} className="text-[10px] px-1.5 py-0">
                        {r.stream ? 'SSE' : 'Sync'}
                      </Badge>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </div>
  )
}
