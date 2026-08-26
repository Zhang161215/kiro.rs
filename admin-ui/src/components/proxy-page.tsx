import { useMemo, useState } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import {
  Plus,
  Activity,
  Trash2,
  Shuffle,
  CheckCircle2,
  XCircle,
  HelpCircle,
} from 'lucide-react'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Textarea } from '@/components/ui/textarea'
import { Switch } from '@/components/ui/switch'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from '@/components/ui/dialog'
import { useCredentials } from '@/hooks/use-credentials'
import {
  listProxies,
  addProxy,
  batchAddProxies,
  deleteProxy,
  setProxyEnabled,
  checkPoolProxy,
  checkAllPoolProxies,
  assignProxy,
} from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'
import type { ProxyEntry } from '@/types/api'

export function ProxyPage() {
  const queryClient = useQueryClient()
  const { data: poolData, isLoading, isError, error, refetch } = useQuery({
    queryKey: ['proxy-pool'],
    queryFn: listProxies,
    refetchInterval: 30000,
  })
  const { data: credData } = useCredentials()

  const [addOpen, setAddOpen] = useState(false)
  const [assignOpen, setAssignOpen] = useState(false)
  const [checkingIds, setCheckingIds] = useState<Set<number>>(new Set())
  const [checkingAll, setCheckingAll] = useState(false)

  const proxies = poolData?.proxies ?? []
  const credentials = credData?.credentials ?? []

  // 每个代理 URL 正被多少个凭据使用（代理页以 URL 为主键，这里做反向统计）
  const usageByUrl = useMemo(() => {
    const map = new Map<string, number>()
    credentials.forEach(c => {
      if (c.proxyUrl) map.set(c.proxyUrl, (map.get(c.proxyUrl) ?? 0) + 1)
    })
    return map
  }, [credentials])

  const healthyCount = proxies.filter(p => p.health === 'healthy').length
  const enabledCount = proxies.filter(p => p.enabled).length
  const refreshPool = () => queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })

  const handleCheckOne = async (id: number) => {
    setCheckingIds(prev => new Set(prev).add(id))
    try {
      await checkPoolProxy(id)
      refreshPool()
    } catch (err) {
      toast.error(`检测失败: ${extractErrorMessage(err)}`)
    } finally {
      setCheckingIds(prev => {
        const next = new Set(prev)
        next.delete(id)
        return next
      })
    }
  }

  const handleCheckAll = async () => {
    if (enabledCount === 0) {
      toast.error('没有已启用的代理可检测')
      return
    }
    setCheckingAll(true)
    try {
      const res = await checkAllPoolProxies()
      refreshPool()
      if (res.unhealthy === 0) {
        toast.success(`检测完成：${res.healthy} 个代理全部可用`)
      } else {
        toast.warning(
          `检测完成：可用 ${res.healthy}，异常 ${res.unhealthy}` +
            (res.autoDisabled > 0 ? `，自动禁用 ${res.autoDisabled}` : '')
        )
      }
    } catch (err) {
      toast.error(`检测失败: ${extractErrorMessage(err)}`)
    } finally {
      setCheckingAll(false)
    }
  }

  const handleToggle = async (proxy: ProxyEntry) => {
    try {
      await setProxyEnabled(proxy.id, !proxy.enabled)
      refreshPool()
    } catch (err) {
      toast.error(`操作失败: ${extractErrorMessage(err)}`)
    }
  }

  const handleDelete = async (proxy: ProxyEntry) => {
    const inUse = usageByUrl.get(proxy.url) ?? 0
    const extra = inUse > 0 ? `\n注意：仍有 ${inUse} 个凭据在用这个代理，删除后它们不会自动改为直连。` : ''
    if (!confirm(`确定从代理池删除 ${proxy.url} 吗？${extra}`)) return
    try {
      await deleteProxy(proxy.id)
      refreshPool()
      toast.success('已删除')
    } catch (err) {
      toast.error(`删除失败: ${extractErrorMessage(err)}`)
    }
  }

  return (
    <>
      <div className="grid gap-4 md:grid-cols-4 mb-6">
        <StatCard title="代理总数" value={proxies.length} />
        <StatCard title="健康" value={healthyCount} accent="text-green-600" />
        <StatCard title="已启用" value={enabledCount} />
        <StatCard title="被凭据使用" value={usageByUrl.size} />
      </div>

      <div className="flex flex-wrap items-center gap-2 mb-4">
        <Button size="sm" onClick={() => setAddOpen(true)}>
          <Plus className="h-4 w-4 mr-2" />
          添加代理
        </Button>
        <Button size="sm" variant="outline" onClick={handleCheckAll} disabled={checkingAll}>
          <Activity className={`h-4 w-4 mr-2 ${checkingAll ? 'animate-pulse' : ''}`} />
          {checkingAll ? '检测中…' : '检测全部'}
        </Button>
        <Button
          size="sm"
          variant="outline"
          onClick={() => setAssignOpen(true)}
          disabled={proxies.length === 0 || credentials.length === 0}
        >
          <Shuffle className="h-4 w-4 mr-2" />
          轮询分配到凭据
        </Button>
        <div className="flex-1" />
        <span className="text-sm text-muted-foreground">
          共 {proxies.length} 个代理
        </span>
      </div>

      <Card>
        <CardContent className="p-0">
          {isLoading ? (
            <div className="py-8 text-center text-muted-foreground">加载中…</div>
          ) : isError ? (
            <div className="py-10 text-center">
              <p className="text-destructive mb-3">加载失败：{extractErrorMessage(error)}</p>
              <Button size="sm" variant="outline" onClick={() => refetch()}>
                重试
              </Button>
            </div>
          ) : proxies.length === 0 ? (
            <div className="py-10 text-center text-muted-foreground">
              代理池为空，点击「添加代理」录入。支持 <code>host:port</code>、
              <code>host:port:user:pass</code>（按 SOCKS5）、<code>http(s)://…</code>、
              <code>socks5://user:pass@host:port</code>
            </div>
          ) : (
            <table className="w-full text-sm">
              <thead className="border-b border-border/60 text-muted-foreground">
                <tr>
                  <th className="py-2.5 pl-4 text-left font-medium">代理地址</th>
                  <th className="py-2.5 text-left font-medium">连通性</th>
                  <th className="py-2.5 text-left font-medium">使用中</th>
                  <th className="py-2.5 text-left font-medium">启用</th>
                  <th className="w-24 py-2.5 pr-4" />
                </tr>
              </thead>
              <tbody>
                {proxies.map(proxy => (
                  <tr key={proxy.id} className="border-b border-border/40 last:border-0">
                    <td className="py-2.5 pl-4">
                      <div className="font-mono text-xs">{proxy.url}</div>
                      <div className="text-[11px] text-muted-foreground">
                        {proxy.username ? `需认证 · ${proxy.username}` : '无认证'}
                        {proxy.label ? ` · ${proxy.label}` : ''}
                      </div>
                    </td>
                    <td className="py-2.5">
                      <HealthCell proxy={proxy} checking={checkingIds.has(proxy.id)} />
                    </td>
                    <td className="py-2.5">
                      {(() => {
                        const n = usageByUrl.get(proxy.url) ?? 0
                        return n > 0 ? (
                          <span className="text-xs">{n} 个凭据</span>
                        ) : (
                          <span className="text-xs text-muted-foreground">未使用</span>
                        )
                      })()}
                    </td>
                    <td className="py-2.5">
                      <div className="flex items-center gap-2">
                        <Switch
                          checked={proxy.enabled}
                          onCheckedChange={() => handleToggle(proxy)}
                        />
                        {proxy.autoDisabled && (
                          <span className="text-[11px] text-destructive">自动禁用</span>
                        )}
                      </div>
                    </td>
                    <td className="py-2.5 pr-4 text-right whitespace-nowrap">
                      <Button
                        size="sm"
                        variant="ghost"
                        disabled={checkingIds.has(proxy.id)}
                        onClick={() => handleCheckOne(proxy.id)}
                        title="检测连通性"
                      >
                        <Activity className="h-3.5 w-3.5" />
                      </Button>
                      <Button
                        size="sm"
                        variant="ghost"
                        className="text-destructive hover:text-destructive"
                        onClick={() => handleDelete(proxy)}
                        title="删除"
                      >
                        <Trash2 className="h-3.5 w-3.5" />
                      </Button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </CardContent>
      </Card>

      <AddProxyDialog open={addOpen} onOpenChange={setAddOpen} onDone={refreshPool} />
      <AssignDialog
        open={assignOpen}
        onOpenChange={setAssignOpen}
        totalCredentials={credentials.length}
        withoutProxyCount={credentials.filter(c => !c.proxyUrl).length}
        onDone={() => queryClient.invalidateQueries({ queryKey: ['credentials'] })}
      />
    </>
  )
}

function HealthCell({ proxy, checking }: { proxy: ProxyEntry; checking: boolean }) {
  if (checking) return <span className="text-xs text-muted-foreground">检测中…</span>
  if (proxy.health === 'healthy') {
    return (
      <span className="inline-flex items-center gap-1 text-xs text-green-600">
        <CheckCircle2 className="h-3.5 w-3.5" />
        {proxy.latencyMs != null ? `${proxy.latencyMs} ms` : '可用'}
      </span>
    )
  }
  if (proxy.health === 'unhealthy') {
    return (
      <span className="inline-flex items-center gap-1 text-xs text-destructive">
        <XCircle className="h-3.5 w-3.5" />
        不可用
      </span>
    )
  }
  return (
    <span className="inline-flex items-center gap-1 text-xs text-muted-foreground">
      <HelpCircle className="h-3.5 w-3.5" />
      未检测
    </span>
  )
}

function StatCard({ title, value, accent }: { title: string; value: number; accent?: string }) {
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-sm font-medium text-muted-foreground">{title}</CardTitle>
      </CardHeader>
      <CardContent>
        <div className={`text-2xl font-bold ${accent ?? ''}`}>{value}</div>
      </CardContent>
    </Card>
  )
}

function AddProxyDialog({
  open,
  onOpenChange,
  onDone,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  onDone: () => void
}) {
  const [text, setText] = useState('')
  const [submitting, setSubmitting] = useState(false)

  const lines = text
    .split('\n')
    .map(l => l.trim())
    .filter(l => l && !l.startsWith('#'))

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault()
    if (lines.length === 0) {
      toast.error('请至少输入一个代理')
      return
    }
    setSubmitting(true)
    try {
      if (lines.length === 1) {
        await addProxy(lines[0])
        toast.success('已添加')
      } else {
        const res = await batchAddProxies(lines)
        if (res.errors.length === 0) {
          toast.success(`已添加 ${res.added} 个代理`)
        } else {
          toast.warning(
            `添加 ${res.added} 个，跳过 ${res.errors.length} 个（${res.errors.slice(0, 2).join('；')}${res.errors.length > 2 ? '…' : ''}）`
          )
        }
      }
      setText('')
      onOpenChange(false)
      onDone()
    } catch (err) {
      toast.error(`添加失败: ${extractErrorMessage(err)}`)
    } finally {
      setSubmitting(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>添加代理</DialogTitle>
        </DialogHeader>
        <form onSubmit={handleSubmit} className="space-y-3 py-2">
          <Textarea
            value={text}
            onChange={e => setText(e.target.value)}
            placeholder={'每行一个，支持：\n1.2.3.4:8080\n1.2.3.4:8080:user:pass  （按 SOCKS5）\nsocks5://user:pass@1.2.3.4:1080\nhttp://user:pass@1.2.3.4:8080'}
            className="font-mono text-xs min-h-[160px]"
            disabled={submitting}
          />
          <div className="text-xs text-muted-foreground">
            识别到 <span className="font-medium text-foreground">{lines.length}</span> 个代理，
            重复的会自动跳过；<code>host:port:user:pass</code> 会拆开地址和账密再入库
          </div>
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)} disabled={submitting}>
              取消
            </Button>
            <Button type="submit" disabled={submitting || lines.length === 0}>
              {submitting ? '添加中…' : `添加 ${lines.length} 个`}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

function AssignDialog({
  open,
  onOpenChange,
  totalCredentials,
  withoutProxyCount,
  onDone,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  totalCredentials: number
  withoutProxyCount: number
  onDone: () => void
}) {
  const [target, setTarget] = useState<'all' | 'without'>('without')
  const [submitting, setSubmitting] = useState(false)
  const { data: credData } = useCredentials()

  const handleSubmit = async () => {
    const credentials = credData?.credentials ?? []
    const ids = credentials
      .filter(c => (target === 'without' ? !c.proxyUrl : true))
      .map(c => c.id)
    if (ids.length === 0) {
      toast.error('没有符合条件的凭据')
      return
    }
    setSubmitting(true)
    try {
      const res = await assignProxy({ credentialIds: ids, roundRobin: true })
      toast.success(`已把 ${res.proxies ?? 0} 个可用代理轮询分配给 ${res.assigned} 个凭据`)
      onOpenChange(false)
      onDone()
    } catch (err) {
      toast.error(`分配失败: ${extractErrorMessage(err)}`)
    } finally {
      setSubmitting(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>轮询分配代理到凭据</DialogTitle>
        </DialogHeader>
        <div className="space-y-3 py-2">
          <p className="text-sm text-muted-foreground">
            把代理池里「已启用且健康」的代理，按顺序循环分配给下列凭据。
          </p>
          <label className="flex items-center gap-2 text-sm">
            <input
              type="radio"
              checked={target === 'without'}
              onChange={() => setTarget('without')}
            />
            仅未配置代理的凭据（{withoutProxyCount} 个）
          </label>
          <label className="flex items-center gap-2 text-sm">
            <input type="radio" checked={target === 'all'} onChange={() => setTarget('all')} />
            全部凭据（{totalCredentials} 个，会覆盖已有代理）
          </label>
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)} disabled={submitting}>
            取消
          </Button>
          <Button onClick={handleSubmit} disabled={submitting}>
            {submitting ? '分配中…' : '开始分配'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
