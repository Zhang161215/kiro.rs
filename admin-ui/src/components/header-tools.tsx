import { RefreshCw } from 'lucide-react'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { Button } from '@/components/ui/button'
import { useLoadBalancingMode, useSetLoadBalancingMode } from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'

export function HeaderTools() {
  const queryClient = useQueryClient()
  const { data: loadBalancingData, isLoading: isLoadingMode } = useLoadBalancingMode()
  const { mutate: setLoadBalancingMode, isPending: isSettingMode } = useSetLoadBalancingMode()

  const handleToggleLoadBalancing = () => {
    const currentMode = loadBalancingData?.mode || 'priority'
    const newMode = currentMode === 'priority' ? 'balanced' : 'priority'
    setLoadBalancingMode(newMode, {
      onSuccess: () => {
        toast.success(newMode === 'priority' ? '已切换到优先级模式' : '已切换到均衡负载模式')
      },
      onError: (error) => {
        toast.error(`切换失败: ${extractErrorMessage(error)}`)
      },
    })
  }

  const handleRefresh = () => {
    queryClient.invalidateQueries()
    toast.success('已刷新')
  }

  return (
    <div className="flex items-center gap-1">
      <Button
        variant="outline"
        size="sm"
        className="h-7 rounded-full px-2.5 text-xs"
        onClick={handleToggleLoadBalancing}
        disabled={isLoadingMode || isSettingMode}
        title="切换负载均衡模式"
      >
        {isLoadingMode
          ? '…'
          : loadBalancingData?.mode === 'priority'
            ? '优先级'
            : '均衡负载'}
      </Button>
      <Button
        variant="outline"
        size="sm"
        className="h-7 rounded-full px-2.5 text-xs"
        onClick={handleRefresh}
        title="刷新数据"
      >
        <RefreshCw className="h-3.5 w-3.5 sm:mr-1" />
        <span className="hidden sm:inline">刷新</span>
      </Button>
    </div>
  )
}
