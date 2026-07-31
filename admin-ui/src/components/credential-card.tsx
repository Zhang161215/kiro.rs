import { useState } from 'react'
import { toast } from 'sonner'
import { RefreshCw, ChevronUp, ChevronDown, Wallet, Trash2, Loader2, Pencil, Zap, ZapOff } from 'lucide-react'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Switch } from '@/components/ui/switch'
import { Input } from '@/components/ui/input'
import { Checkbox } from '@/components/ui/checkbox'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { EditCredentialDialog } from '@/components/edit-credential-dialog'
import type { CredentialStatusItem, BalanceResponse } from '@/types/api'
import {
  useSetDisabled,
  useSetPriority,
  useResetFailure,
  useDeleteCredential,
  useForceRefreshToken,
  useSetOverage,
} from '@/hooks/use-credentials'

interface CredentialCardProps {
  credential: CredentialStatusItem
  onViewBalance: (id: number) => void
  selected: boolean
  onToggleSelect: () => void
  balance: BalanceResponse | null
  loadingBalance: boolean
}

function formatLastUsed(lastUsedAt: string | null): string {
  if (!lastUsedAt) return '从未使用'
  const date = new Date(lastUsedAt)
  const now = new Date()
  const diff = now.getTime() - date.getTime()
  if (diff < 0) return '刚刚'
  const seconds = Math.floor(diff / 1000)
  if (seconds < 60) return `${seconds} 秒前`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes} 分钟前`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours} 小时前`
  const days = Math.floor(hours / 24)
  return `${days} 天前`
}

export function CredentialCard({
  credential,
  onViewBalance,
  selected,
  onToggleSelect,
  balance,
  loadingBalance,
}: CredentialCardProps) {
  const [editingPriority, setEditingPriority] = useState(false)
  const [priorityValue, setPriorityValue] = useState(String(credential.priority))
  const [showDeleteDialog, setShowDeleteDialog] = useState(false)
  const [showEditDialog, setShowEditDialog] = useState(false)

  const setDisabled = useSetDisabled()
  const setPriority = useSetPriority()
  const resetFailure = useResetFailure()
  const deleteCredential = useDeleteCredential()
  const forceRefresh = useForceRefreshToken()
  const setOverage = useSetOverage()

  const handleToggleDisabled = () => {
    setDisabled.mutate(
      { id: credential.id, disabled: !credential.disabled },
      {
        onSuccess: (res) => {
          toast.success(res.message)
        },
        onError: (err) => {
          toast.error('操作失败: ' + (err as Error).message)
        },
      }
    )
  }

  const handlePriorityChange = () => {
    const newPriority = parseInt(priorityValue, 10)
    if (isNaN(newPriority) || newPriority < 0) {
      toast.error('优先级必须是非负整数')
      return
    }
    setPriority.mutate(
      { id: credential.id, priority: newPriority },
      {
        onSuccess: (res) => {
          toast.success(res.message)
          setEditingPriority(false)
        },
        onError: (err) => {
          toast.error('操作失败: ' + (err as Error).message)
        },
      }
    )
  }

  const handleReset = () => {
    resetFailure.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
      },
      onError: (err) => {
        toast.error('操作失败: ' + (err as Error).message)
      },
    })
  }

  const handleForceRefresh = () => {
    forceRefresh.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
      },
      onError: (err) => {
        toast.error('刷新失败: ' + (err as Error).message)
      },
    })
  }

  const handleDelete = () => {
    if (!credential.disabled) {
      toast.error('请先禁用凭据再删除')
      setShowDeleteDialog(false)
      return
    }

    deleteCredential.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
        setShowDeleteDialog(false)
      },
      onError: (err) => {
        toast.error('删除失败: ' + (err as Error).message)
      },
    })
  }

  return (
    <>
      <Card className={`glass-card shadow-apple-sm transition-all duration-200 ${credential.isCurrent ? 'ring-2 ring-primary' : ''}`}>
        <CardHeader className="pb-2">
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2">
              <Checkbox
                checked={selected}
                onCheckedChange={onToggleSelect}
              />
              <CardTitle className="text-lg flex items-center gap-2">
                {credential.email || `凭据 #${credential.id}`}
                {credential.isCurrent && (
                  <Badge variant="success">当前</Badge>
                )}
                {credential.disabled && (
                  <Badge variant="destructive">已禁用</Badge>
                )}
                {credential.disabled && credential.disabledReason && (
                  <Badge variant="outline">{credential.disabledReason}</Badge>
                )}
              </CardTitle>
            </div>
            <div className="flex items-center gap-2">
              <span className="text-sm text-muted-foreground">启用</span>
              <Switch
                checked={!credential.disabled}
                onCheckedChange={handleToggleDisabled}
                disabled={setDisabled.isPending}
              />
            </div>
          </div>
        </CardHeader>
        <CardContent className="space-y-4">
          {/* 信息网格 */}
          <div className="grid grid-cols-2 gap-4 text-sm">
            <div>
              <span className="text-muted-foreground">优先级：</span>
              {editingPriority ? (
                <div className="inline-flex items-center gap-1 ml-1">
                  <Input
                    type="number"
                    value={priorityValue}
                    onChange={(e) => setPriorityValue(e.target.value)}
                    className="w-16 h-7 text-sm"
                    min="0"
                  />
                  <Button
                    size="sm"
                    variant="ghost"
                    className="h-7 w-7 p-0"
                    onClick={handlePriorityChange}
                    disabled={setPriority.isPending}
                  >
                    ✓
                  </Button>
                  <Button
                    size="sm"
                    variant="ghost"
                    className="h-7 w-7 p-0"
                    onClick={() => {
                      setEditingPriority(false)
                      setPriorityValue(String(credential.priority))
                    }}
                  >
                    ✕
                  </Button>
                </div>
              ) : (
                <span
                  className="font-medium cursor-pointer hover:underline ml-1"
                  onClick={() => setEditingPriority(true)}
                >
                  {credential.priority}
                  <span className="text-xs text-muted-foreground ml-1">(点击编辑)</span>
                </span>
              )}
            </div>
            <div>
              <span className="text-muted-foreground">失败次数：</span>
              <span className={credential.failureCount > 0 ? 'text-red-500 font-medium' : ''}>
                {credential.failureCount}
              </span>
            </div>
            <div>
              <span className="text-muted-foreground">刷新失败：</span>
              <span className={credential.refreshFailureCount > 0 ? 'text-red-500 font-medium' : ''}>
                {credential.refreshFailureCount}
              </span>
            </div>
            <div>
              <span className="text-muted-foreground">订阅等级：</span>
              <span className="font-medium">
                {loadingBalance ? (
                  <Loader2 className="inline w-3 h-3 animate-spin" />
                ) : balance?.subscriptionTitle || '未知'}
              </span>
            </div>
            <div>
              <span className="text-muted-foreground">成功次数：</span>
              <span className="font-medium">{credential.successCount}</span>
            </div>
            <div className="col-span-2">
              <span className="text-muted-foreground">最后调用：</span>
              <span className="font-medium">{formatLastUsed(credential.lastUsedAt)}</span>
            </div>
            <div className="col-span-2">
              <span className="text-muted-foreground">用量：</span>
              {loadingBalance ? (
                <span className="text-sm ml-1">
                  <Loader2 className="inline w-3 h-3 animate-spin" /> 加载中...
                </span>
              ) : balance ? (() => {
                const overage = balance.currentUsage > balance.usageLimit
                const maxBar = Math.max(balance.usageLimit, Math.min(balance.currentUsage, 10000))
                const limitPct = (balance.usageLimit / maxBar) * 100
                const usagePct = (Math.min(balance.currentUsage, maxBar) / maxBar) * 100
                return (
                <div className="mt-1.5 space-y-1">
                  <div className="flex justify-between text-xs">
                    <span>${balance.currentUsage.toFixed(2)} / ${balance.usageLimit.toFixed(2)}</span>
                    <span className={overage ? 'text-red-500 font-medium' : 'text-muted-foreground'}>
                      {overage
                        ? `超额 $${(balance.currentUsage - balance.usageLimit).toFixed(2)}`
                        : `剩余 $${balance.remaining.toFixed(2)}`}
                    </span>
                  </div>
                  <div className="w-full h-2 rounded-full bg-secondary overflow-hidden relative">
                    {overage ? (
                      <>
                        <div className="absolute h-full rounded-l-full bg-emerald-500" style={{ width: `${limitPct}%` }} />
                        <div className="absolute h-full bg-red-500 rounded-r-full" style={{ left: `${limitPct}%`, width: `${usagePct - limitPct}%` }} />
                      </>
                    ) : (
                      <div
                        className={`h-full rounded-full transition-all ${
                          balance.usagePercentage > 80 ? 'bg-amber-500' : 'bg-emerald-500'
                        }`}
                        style={{ width: `${usagePct}%` }}
                      />
                    )}
                  </div>
                  <div className="text-xs text-muted-foreground">
                    {balance.usagePercentage.toFixed(1)}% 已使用
                    {balance.nextResetAt && ` · 重置于 ${new Date(balance.nextResetAt * 1000).toLocaleDateString('zh-CN')}`}
                  </div>
                </div>
                )
              })() : (
                <span className="text-sm text-muted-foreground ml-1">未知</span>
              )}
            </div>
            {credential.maskedApiKey && (
              <div className="col-span-2">
                <span className="text-muted-foreground">API Key：</span>
                <span className="font-mono font-medium">{credential.maskedApiKey}</span>
              </div>
            )}
            {credential.hasProxy && (
              <div className="col-span-2">
                <span className="text-muted-foreground">代理：</span>
                <span className="font-medium">{credential.proxyUrl}</span>
              </div>
            )}
            {credential.hasProfileArn && (
              <div className="col-span-2">
                <Badge variant="secondary">有 Profile ARN</Badge>
              </div>
            )}
          </div>

          {/* 操作按钮：主操作 / 优先级与重置 / 危险操作 三行分组 */}
          <div className="space-y-2 pt-3 border-t border-border/60">
            {/* 主操作 4 等分 */}
            <div className="grid grid-cols-2 sm:grid-cols-4 gap-2">
              <Button
                size="sm"
                variant="default"
                onClick={() => onViewBalance(credential.id)}
                className="w-full min-w-0 px-2"
              >
                <Wallet className="h-4 w-4" />
                <span className="truncate">查看余额</span>
              </Button>
              <Button
                size="sm"
                variant="outline"
                onClick={() => setShowEditDialog(true)}
                className="w-full min-w-0 px-2"
              >
                <Pencil className="h-4 w-4" />
                <span className="truncate">编辑</span>
              </Button>
              {(() => {
                const overageEnabled = balance?.overageStatus === 'ENABLED'
                return (
                  <Button
                    size="sm"
                    variant={overageEnabled ? 'outline' : 'secondary'}
                    onClick={() => {
                      setOverage.mutate(
                        { id: credential.id, enabled: !overageEnabled },
                        {
                          onSuccess: (res) => toast.success(res.message || (overageEnabled ? '已关闭超额' : '已开启超额')),
                          onError: (err) => toast.error('操作失败: ' + (err as Error).message),
                        },
                      )
                    }}
                    disabled={setOverage.isPending || !balance}
                    title={!balance ? '请先查看余额获取超额状态' : overageEnabled ? '关闭超额计费' : '开启超额计费'}
                    className="w-full min-w-0 px-2"
                  >
                    {overageEnabled ? (
                      <ZapOff className="h-4 w-4" />
                    ) : (
                      <Zap className="h-4 w-4" />
                    )}
                    <span className="truncate">{overageEnabled ? '关超额' : '开超额'}</span>
                  </Button>
                )
              })()}
              <Button
                size="sm"
                variant="outline"
                onClick={handleForceRefresh}
                disabled={forceRefresh.isPending || credential.disabled}
                title={credential.disabled ? '已禁用的凭据无法刷新 Token' : '强制刷新 Token'}
                className="w-full min-w-0 px-2"
              >
                <RefreshCw className={`h-4 w-4 ${forceRefresh.isPending ? 'animate-spin' : ''}`} />
                <span className="truncate">刷新 Token</span>
              </Button>
            </div>

            {/* 次要操作：优先级 + 重置失败 */}
            <div className="grid grid-cols-3 gap-2">
              <Button
                size="sm"
                variant="ghost"
                onClick={() => {
                  const newPriority = Math.max(0, credential.priority - 1)
                  setPriority.mutate(
                    { id: credential.id, priority: newPriority },
                    {
                      onSuccess: (res) => toast.success(res.message),
                      onError: (err) => toast.error('操作失败: ' + (err as Error).message),
                    }
                  )
                }}
                disabled={setPriority.isPending || credential.priority === 0}
                className="w-full min-w-0 px-2"
              >
                <ChevronUp className="h-4 w-4" />
                <span className="truncate">提高优先级</span>
              </Button>
              <Button
                size="sm"
                variant="ghost"
                onClick={() => {
                  const newPriority = credential.priority + 1
                  setPriority.mutate(
                    { id: credential.id, priority: newPriority },
                    {
                      onSuccess: (res) => toast.success(res.message),
                      onError: (err) => toast.error('操作失败: ' + (err as Error).message),
                    }
                  )
                }}
                disabled={setPriority.isPending}
                className="w-full min-w-0 px-2"
              >
                <ChevronDown className="h-4 w-4" />
                <span className="truncate">降低优先级</span>
              </Button>
              <Button
                size="sm"
                variant="ghost"
                onClick={handleReset}
                disabled={resetFailure.isPending || (credential.failureCount === 0 && credential.refreshFailureCount === 0)}
                className="w-full min-w-0 px-2"
              >
                <RefreshCw className="h-4 w-4" />
                <span className="truncate">重置失败</span>
              </Button>
            </div>

            {/* 危险操作：删除右对齐 */}
            <div className="flex justify-end">
              <Button
                size="sm"
                variant="destructive"
                onClick={() => setShowDeleteDialog(true)}
                disabled={!credential.disabled}
                title={!credential.disabled ? '需要先禁用凭据才能删除' : undefined}
              >
                <Trash2 className="h-4 w-4" />
                <span>删除凭据</span>
              </Button>
            </div>
          </div>
        </CardContent>
      </Card>

      {/* 编辑对话框 */}
      <EditCredentialDialog
        open={showEditDialog}
        onOpenChange={setShowEditDialog}
        credential={credential}
      />

      {/* 删除确认对话框 */}
      <Dialog open={showDeleteDialog} onOpenChange={setShowDeleteDialog}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>确认删除凭据</DialogTitle>
            <DialogDescription>
              您确定要删除凭据 #{credential.id} 吗？此操作无法撤销。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setShowDeleteDialog(false)}
              disabled={deleteCredential.isPending}
            >
              取消
            </Button>
            <Button
              variant="destructive"
              onClick={handleDelete}
              disabled={deleteCredential.isPending || !credential.disabled}
            >
              确认删除
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  )
}
