import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  getCredentials,
  setCredentialDisabled,
  setCredentialPriority,
  resetCredentialFailure,
  forceRefreshToken,
  getCredentialBalance,
  addCredential,
  deleteCredential,
  getLoadBalancingMode,
  setLoadBalancingMode,
  getRequestDetails,
  clearRequestDetails,
  getKvCacheConfig,
  setKvCacheConfig,
  updateCredential,
  setCredentialOverage,
  getAdminKeys,
  getModelsConfig,
  setModelsConfig,
  restartService,
  type KvCacheConfig,
  type ModelEntry,
} from '@/api/credentials'
import type { AddCredentialRequest, UpdateCredentialRequest } from '@/types/api'

// 查询凭据列表
export function useCredentials() {
  return useQuery({
    queryKey: ['credentials'],
    queryFn: getCredentials,
    refetchInterval: 30000,
    retry: 1,
  })
}

// 查询凭据余额
export function useCredentialBalance(id: number | null) {
  return useQuery({
    queryKey: ['credential-balance', id],
    queryFn: () => getCredentialBalance(id!),
    enabled: id !== null,
    retry: false, // 余额查询失败时不重试（避免重复请求被封禁的账号）
  })
}

// 设置禁用状态
export function useSetDisabled() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, disabled }: { id: number; disabled: boolean }) =>
      setCredentialDisabled(id, disabled),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 设置优先级
export function useSetPriority() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, priority }: { id: number; priority: number }) =>
      setCredentialPriority(id, priority),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 重置失败计数
export function useResetFailure() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => resetCredentialFailure(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 强制刷新 Token
export function useForceRefreshToken() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => forceRefreshToken(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 添加新凭据
export function useAddCredential() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: AddCredentialRequest) => addCredential(req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 删除凭据
export function useDeleteCredential() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => deleteCredential(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 获取负载均衡模式
export function useLoadBalancingMode() {
  return useQuery({
    queryKey: ['loadBalancingMode'],
    queryFn: getLoadBalancingMode,
  })
}

// 设置负载均衡模式
export function useSetLoadBalancingMode() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setLoadBalancingMode,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['loadBalancingMode'] })
    },
  })
}

// 查询请求明细
export function useRequestDetails(limit?: number) {
  return useQuery({
    queryKey: ['requestDetails', limit],
    queryFn: () => getRequestDetails(limit),
    refetchInterval: 30000,
  })
}

// 获取 KV 缓存配置
export function useKvCacheConfig() {
  return useQuery({
    queryKey: ['kvCacheConfig'],
    queryFn: getKvCacheConfig,
  })
}

// 设置 KV 缓存配置
export function useSetKvCacheConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (config: Partial<KvCacheConfig>) => setKvCacheConfig(config),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['kvCacheConfig'] })
    },
  })
}

// 获取模型配置
export function useModelsConfig() {
  return useQuery({
    queryKey: ['modelsConfig'],
    queryFn: getModelsConfig,
  })
}

// 设置模型配置（保存即热更新生效）
export function useSetModelsConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (models: ModelEntry[]) => setModelsConfig(models),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['modelsConfig'] })
    },
  })
}

// 重启服务
export function useRestartService() {
  return useMutation({
    mutationFn: restartService,
  })
}

// 清空请求明细
export function useClearRequestDetails() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: clearRequestDetails,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['requestDetails'] })
      queryClient.invalidateQueries({ queryKey: ['stats'] })
    },
  })
}

// 更新凭据
export function useUpdateCredential() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, req }: { id: number; req: UpdateCredentialRequest }) =>
      updateCredential(id, req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 设置超额开关
export function useSetOverage() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, enabled }: { id: number; enabled: boolean }) =>
      setCredentialOverage(id, enabled),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
      queryClient.invalidateQueries({ queryKey: ['credential-balance'] })
    },
  })
}

// 获取 Admin Keys
export function useAdminKeys() {
  return useQuery({
    queryKey: ['adminKeys'],
    queryFn: getAdminKeys,
    staleTime: Infinity,
  })
}
