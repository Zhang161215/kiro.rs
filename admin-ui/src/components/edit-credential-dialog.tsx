import { useState, useEffect } from 'react'
import { toast } from 'sonner'
import { Activity } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { useQuery } from '@tanstack/react-query'
import { useUpdateCredential } from '@/hooks/use-credentials'
import { checkCredentialProxy, listProxies } from '@/api/credentials'
import type { CredentialStatusItem, ProxyCheckResponse, ProxyEntry } from '@/types/api'
import { extractErrorMessage } from '@/lib/utils'

interface EditCredentialDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  credential: CredentialStatusItem
}

export function EditCredentialDialog({
  open,
  onOpenChange,
  credential,
}: EditCredentialDialogProps) {
  const [email, setEmail] = useState('')
  const [authRegion, setAuthRegion] = useState('')
  const [apiRegion, setApiRegion] = useState('')
  const [proxyUrl, setProxyUrl] = useState('')
  const [proxyUsername, setProxyUsername] = useState('')
  const [proxyPassword, setProxyPassword] = useState('')

  const [checking, setChecking] = useState(false)
  const [checkResult, setCheckResult] = useState<ProxyCheckResponse | null>(null)

  useEffect(() => {
    if (open) {
      setEmail(credential.email ?? '')
      setAuthRegion(credential.authRegion ?? '')
      setApiRegion(credential.apiRegion ?? '')
      setProxyUrl(credential.proxyUrl ?? '')
      setProxyUsername('')
      setProxyPassword('')
      setCheckResult(null)
    }
  }, [open, credential])

  const { mutate, isPending } = useUpdateCredential()
  // 代理池里的地址，供下拉选择（问题2：从已有代理中选）
  const { data: poolData } = useQuery({ queryKey: ['proxy-pool'], queryFn: listProxies })
  const knownProxies = (poolData?.proxies ?? []).filter(p => p.enabled)

  const applyProxyFromPool = (url: string) => {
    setProxyUrl(url)
    const hit = findProxyInPool(knownProxies, url)
    if (hit) {
      setProxyUsername(hit.username ?? '')
      setProxyPassword(hit.password ?? '')
    }
  }

  // 检测：账密留空且 URL 未改 → 测已保存配置；否则测输入框这一组（未保存也能试）
  const handleCheckProxy = async () => {
    setChecking(true)
    setCheckResult(null)
    try {
      const url = proxyUrl.trim()
      const user = proxyUsername.trim()
      const pass = proxyPassword.trim()
      const savedUrl = (credential.proxyUrl ?? '').trim()

      let req: Parameters<typeof checkCredentialProxy>[1]
      if (!url || (!user && !pass && url === savedUrl)) {
        req = undefined
      } else {
        let u = user
        let p = pass
        if (!u && !p) {
          const hit = findProxyInPool(knownProxies, url)
          if (hit) {
            u = hit.username ?? ''
            p = hit.password ?? ''
          }
        }
        req = {
          proxyUrl: url,
          proxyUsername: u || null,
          proxyPassword: p || null,
        }
      }
      const result = await checkCredentialProxy(credential.id, req)
      setCheckResult(result)
    } catch (err) {
      setCheckResult({ ok: false, error: extractErrorMessage(err) })
    } finally {
      setChecking(false)
    }
  }

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault()
    // 后端约定：空字符串 = 清除该字段，null = 保持原值。
    // 这几个字段在表单里都回显了当前值，用户清空即表示要清除，所以直接发空串。
    const trim = (s: string) => s.trim()
    const nextProxyUrl = trim(proxyUrl)
    const clearingProxy = nextProxyUrl === ''
    mutate(
      {
        id: credential.id,
        req: {
          email: trim(email),
          authRegion: trim(authRegion),
          apiRegion: trim(apiRegion),
          proxyUrl: nextProxyUrl,
          // 用户名/密码不回显，留空表示沿用原值；但清除代理时要一并清掉
          proxyUsername: clearingProxy ? '' : trim(proxyUsername) || null,
          proxyPassword: clearingProxy ? '' : trim(proxyPassword) || null,
        },
      },
      {
        onSuccess: (res) => {
          toast.success(res.message || '更新成功')
          onOpenChange(false)
        },
        onError: (err) => {
          toast.error('更新失败: ' + extractErrorMessage(err))
        },
      },
    )
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>编辑凭据 #{credential.id}</DialogTitle>
        </DialogHeader>
        <form onSubmit={handleSubmit} className="space-y-4 py-2">
          <Field label="备注邮箱">
            <Input
              type="email"
              value={email}
              onChange={(e) => setEmail(e.target.value)}
              placeholder="example@domain.com"
              disabled={isPending}
            />
          </Field>

          <div className="grid grid-cols-2 gap-3">
            <Field label="Auth Region" hint="OIDC token 刷新区域">
              <Input
                value={authRegion}
                onChange={(e) => setAuthRegion(e.target.value)}
                placeholder="us-east-1"
                disabled={isPending}
              />
            </Field>
            <Field label="API Region" hint="getUsageLimits 区域">
              <Input
                value={apiRegion}
                onChange={(e) => setApiRegion(e.target.value)}
                placeholder="us-east-1"
                disabled={isPending}
              />
            </Field>
          </div>

          <div className="space-y-3 rounded-xl border border-border/60 p-3 bg-muted/30">
            <div className="text-[13px] font-medium">代理设置</div>
            <Field
              label="代理 URL"
              hint={
                knownProxies.length > 0
                  ? `留空表示直连；可从 ${knownProxies.length} 个已有代理中选择`
                  : '留空表示直连；支持 http(s):// 和 socks5://'
              }
            >
              <div className="flex gap-2">
                <Input
                  value={proxyUrl}
                  onChange={(e) => applyProxyFromPool(e.target.value)}
                  placeholder="host:port 或 socks5://127.0.0.1:1080"
                  list={knownProxies.length > 0 ? 'edit-known-proxies' : undefined}
                  disabled={isPending}
                />
                <Button
                  type="button"
                  variant="outline"
                  onClick={handleCheckProxy}
                  disabled={isPending || checking}
                  title="测试连通性：经代理访问 generate_204（不会保存）"
                >
                  <Activity className={`h-4 w-4 ${checking ? 'animate-pulse' : ''}`} />
                </Button>
              </div>
              {knownProxies.length > 0 && (
                <datalist id="edit-known-proxies">
                  {knownProxies.map((p) => (
                    <option key={p.id} value={p.url}>
                      {p.username ? `${p.url} (${p.username})` : p.url}
                    </option>
                  ))}
                </datalist>
              )}
              {checkResult && (
                <p
                  className={`text-[11px] ${checkResult.ok ? 'text-emerald-600' : 'text-destructive'}`}
                >
                  {checkResult.ok
                    ? `连通，延迟 ${checkResult.latencyMs ?? '?'} ms`
                    : `不通：${checkResult.error ?? '未知错误'}`}
                </p>
              )}
            </Field>
            <div className="grid grid-cols-2 gap-3">
              <Field label="代理用户名">
                <Input
                  value={proxyUsername}
                  onChange={(e) => setProxyUsername(e.target.value)}
                  placeholder="留空沿用已保存"
                  disabled={isPending}
                  autoComplete="off"
                />
              </Field>
              <Field label="代理密码">
                <Input
                  type="password"
                  value={proxyPassword}
                  onChange={(e) => setProxyPassword(e.target.value)}
                  placeholder="留空沿用已保存"
                  disabled={isPending}
                  autoComplete="new-password"
                />
              </Field>
            </div>
            <p className="text-[11px] text-muted-foreground">
              用户名/密码不回显。测试时留空会使用已保存的账密；裸 host:port 带账密按 SOCKS5h（远程 DNS）探测。清空代理请直接清空 URL。
            </p>
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => onOpenChange(false)}
              disabled={isPending}
            >
              取消
            </Button>
            <Button type="submit" disabled={isPending}>
              {isPending ? '保存中…' : '保存'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

function proxyHostPort(raw: string): string {
  const s = raw.trim()
  if (!s) return ''
  const afterScheme = s.includes('://') ? s.slice(s.indexOf('://') + 3) : s
  const afterAuth = afterScheme.includes('@')
    ? afterScheme.slice(afterScheme.lastIndexOf('@') + 1)
    : afterScheme
  return afterAuth.split('/')[0] ?? afterAuth
}

function findProxyInPool(proxies: ProxyEntry[], url: string): ProxyEntry | undefined {
  const needle = url.trim()
  if (!needle) return undefined
  const hp = proxyHostPort(needle)
  return proxies.find((p) => p.url === needle || proxyHostPort(p.url) === hp)
}

function Field({
  label,
  hint,
  children,
}: {
  label: string
  hint?: string
  children: React.ReactNode
}) {
  return (
    <label className="block space-y-1.5">
      <div className="flex items-center justify-between">
        <span className="text-[13px] font-medium">{label}</span>
        {hint && (
          <span className="text-[11px] text-muted-foreground">{hint}</span>
        )}
      </div>
      {children}
    </label>
  )
}
