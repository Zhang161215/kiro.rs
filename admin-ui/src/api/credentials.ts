import { api } from '@/api/client'
import type {
  CredentialsStatusResponse,
  BalanceResponse,
  SuccessResponse,
  SetDisabledRequest,
  SetPriorityRequest,
  AddCredentialRequest,
  AddCredentialResponse,
  RequestDetailsResponse,
  UpdateCredentialRequest,
  AdminKeysResponse,
  SetOverageRequest,
  ProxyCheckRequest,
  ProxyCheckResponse,
  ProxyEntry,
  ProxyPoolResponse,
  BatchAddProxyResult,
  CheckAllProxiesResult,
  AssignProxyRequest,
} from '@/types/api'

// 获取所有凭据状态
export async function getCredentials(): Promise<CredentialsStatusResponse> {
  const { data } = await api.get<CredentialsStatusResponse>('/credentials')
  return data
}

// 设置凭据禁用状态
export async function setCredentialDisabled(
  id: number,
  disabled: boolean
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/disabled`,
    { disabled } as SetDisabledRequest
  )
  return data
}

// 设置凭据优先级
export async function setCredentialPriority(
  id: number,
  priority: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/priority`,
    { priority } as SetPriorityRequest
  )
  return data
}

// 重置失败计数
export async function resetCredentialFailure(
  id: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/reset`)
  return data
}

// 强制刷新 Token
export async function forceRefreshToken(
  id: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/refresh`)
  return data
}

// 获取凭据余额
export async function getCredentialBalance(id: number): Promise<BalanceResponse> {
  const { data } = await api.get<BalanceResponse>(`/credentials/${id}/balance`)
  return data
}

// 添加新凭据
export async function addCredential(
  req: AddCredentialRequest
): Promise<AddCredentialResponse> {
  const { data } = await api.post<AddCredentialResponse>('/credentials', req)
  return data
}

// 删除凭据
export async function deleteCredential(id: number): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>(`/credentials/${id}`)
  return data
}

// 获取负载均衡模式
export async function getLoadBalancingMode(): Promise<{ mode: 'priority' | 'balanced' }> {
  const { data } = await api.get<{ mode: 'priority' | 'balanced' }>('/config/load-balancing')
  return data
}

// 设置负载均衡模式
export async function setLoadBalancingMode(mode: 'priority' | 'balanced'): Promise<{ mode: 'priority' | 'balanced' }> {
  const { data } = await api.put<{ mode: 'priority' | 'balanced' }>('/config/load-balancing', { mode })
  return data
}

// KV 缓存配置
export interface KvCacheConfig {
  cacheReadEfficiency: number
  kvCacheTtlSecs: number
}

// 获取 KV 缓存配置
export async function getKvCacheConfig(): Promise<KvCacheConfig> {
  const { data } = await api.get<KvCacheConfig>('/config/kv-cache')
  return data
}

// 设置 KV 缓存配置
export async function setKvCacheConfig(config: Partial<KvCacheConfig>): Promise<KvCacheConfig> {
  const { data } = await api.put<KvCacheConfig>('/config/kv-cache', config)
  return data
}

// 模型配置
export interface ModelEntry {
  id: string
  displayName: string
  kiroModelId: string
  contextWindow: number
  maxTokens: number
  matchKeywords: string[]
  created: number
}

export interface ModelsConfig {
  models: ModelEntry[]
}

// 获取模型配置
export async function getModelsConfig(): Promise<ModelsConfig> {
  const { data } = await api.get<ModelsConfig>('/config/models')
  return data
}

// 设置模型配置（保存即热更新生效）
export async function setModelsConfig(models: ModelEntry[]): Promise<ModelsConfig> {
  const { data } = await api.put<ModelsConfig>('/config/models', { models })
  return data
}

// 重启服务（进程退出，由容器 restart 策略拉起）
export async function restartService(): Promise<{ success: boolean; message: string }> {
  const { data } = await api.post<{ success: boolean; message: string }>('/restart')
  return data
}


// 获取请求明细
export async function getRequestDetails(limit?: number): Promise<RequestDetailsResponse> {
  const params = limit ? { limit } : {}
  const { data } = await api.get<RequestDetailsResponse>('/details', { params })
  return data
}

// 清空请求明细
export async function clearRequestDetails(): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>('/details')
  return data
}

// 更新凭据（PATCH）
export async function updateCredential(
  id: number,
  req: UpdateCredentialRequest
): Promise<SuccessResponse> {
  const { data } = await api.patch<SuccessResponse>(`/credentials/${id}`, req)
  return data
}

// 检测代理连通性。不传 req 时检测该凭据已保存的代理（含用户名/密码）。
// 编辑页账密不回显：URL 未改且账密留空时不要传 proxyUrl，否则后端会当成无认证 HTTP 代理。
export async function checkCredentialProxy(
  id: number,
  req?: ProxyCheckRequest
): Promise<ProxyCheckResponse> {
  const { data } = await api.post<ProxyCheckResponse>(
    `/credentials/${id}/proxy-check`,
    req ?? {}
  )
  return data
}

// ============ 代理 IP 池 ============

export async function listProxies(): Promise<ProxyPoolResponse> {
  const { data } = await api.get<ProxyPoolResponse>('/proxy-pool')
  return data
}

export async function addProxy(url: string, label?: string): Promise<ProxyEntry> {
  const { data } = await api.post<ProxyEntry>('/proxy-pool', { url, label })
  return data
}

export async function batchAddProxies(urls: string[]): Promise<BatchAddProxyResult> {
  const { data } = await api.post<BatchAddProxyResult>('/proxy-pool/batch', { urls })
  return data
}

export async function deleteProxy(id: number): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>(`/proxy-pool/${id}`)
  return data
}

export async function setProxyEnabled(id: number, enabled: boolean): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/proxy-pool/${id}/enabled`, { enabled })
  return data
}

export async function checkPoolProxy(id: number): Promise<ProxyEntry> {
  const { data } = await api.post<ProxyEntry>(`/proxy-pool/${id}/check`)
  return data
}

export async function checkAllPoolProxies(): Promise<CheckAllProxiesResult> {
  const { data } = await api.post<CheckAllProxiesResult>('/proxy-pool/check-all')
  return data
}

export async function assignProxy(req: AssignProxyRequest): Promise<{ assigned: number; proxies?: number }> {
  const { data } = await api.post<{ assigned: number; proxies?: number }>('/proxy-pool/assign', req)
  return data
}

// 设置凭据超额开关
export async function setCredentialOverage(
  id: number,
  enabled: boolean
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/overage`,
    { enabled } as SetOverageRequest
  )
  return data
}

// 获取 Admin Keys 信息
export async function getAdminKeys(): Promise<AdminKeysResponse> {
  const { data } = await api.get<AdminKeysResponse>('/keys')
  return data
}
