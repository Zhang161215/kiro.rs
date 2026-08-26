// 凭据状态响应
export interface CredentialsStatusResponse {
  total: number
  available: number
  currentId: number
  credentials: CredentialStatusItem[]
}

// 单个凭据状态
export interface CredentialStatusItem {
  id: number
  priority: number
  disabled: boolean
  failureCount: number
  isCurrent: boolean
  expiresAt: string | null
  authMethod: string | null
  hasProfileArn: boolean
  email?: string
  refreshTokenHash?: string
  /** kiroApiKey 的 SHA-256（仅 API Key 凭据，前端去重用） */
  apiKeyHash?: string
  /** kiroApiKey 的脱敏展示（仅 API Key 凭据） */
  maskedApiKey?: string
  successCount: number
  lastUsedAt: string | null
  hasProxy: boolean
  proxyUrl?: string
  authRegion?: string | null
  apiRegion?: string | null
  refreshFailureCount: number
  disabledReason?: string
}

// 余额响应
export interface BalanceResponse {
  id: number
  subscriptionTitle: string | null
  currentUsage: number
  usageLimit: number
  remaining: number
  usagePercentage: number
  nextResetAt: number | null
  overageStatus?: string | null
  overageLimit?: number | null
  overageCharges?: number | null
}

// 更新凭据请求（PATCH /credentials/:id）
//
// 后端约定：空字符串 = 清除该字段，null / 省略 = 保持原值
export interface UpdateCredentialRequest {
  email?: string | null
  authRegion?: string | null
  apiRegion?: string | null
  proxyUrl?: string | null
  proxyUsername?: string | null
  proxyPassword?: string | null
}

// 代理检测请求（POST /credentials/:id/proxy-check）
// 不传字段表示检测该凭据已保存的代理
export interface ProxyCheckRequest {
  proxyUrl?: string | null
  proxyUsername?: string | null
  proxyPassword?: string | null
}

export interface ProxyCheckResponse {
  ok: boolean
  proxyUrl?: string | null
  latencyMs?: number | null
  error?: string | null
}

// ============ 代理 IP 池 ============

export type ProxyHealth = 'unknown' | 'healthy' | 'unhealthy'

export interface ProxyEntry {
  id: number
  url: string
  username?: string | null
  password?: string | null
  label?: string | null
  enabled: boolean
  health: ProxyHealth
  latencyMs?: number | null
  lastCheckedAt?: string | null
  consecutiveFailures: number
  autoDisabled: boolean
}

export interface ProxyPoolResponse {
  proxies: ProxyEntry[]
}

export interface BatchAddProxyResult {
  added: number
  errors: string[]
}

export interface CheckAllProxiesResult {
  healthy: number
  unhealthy: number
  autoDisabled: number
}

export interface AssignProxyRequest {
  proxyUrl?: string | null
  credentialIds: number[]
  roundRobin?: boolean
}

// API Key 信息（GET /keys）
export interface AdminKeysResponse {
  apiKey: {
    masked: string
    full: string
  }
  adminApiKey: {
    masked: string
    full: string
  }
}

// 超额开关请求
export interface SetOverageRequest {
  enabled: boolean
}

// 成功响应
export interface SuccessResponse {
  success: boolean
  message: string
}

// 错误响应
export interface AdminErrorResponse {
  error: {
    type: string
    message: string
  }
}

// 请求类型
export interface SetDisabledRequest {
  disabled: boolean
}

export interface SetPriorityRequest {
  priority: number
}

// 添加凭据请求
export interface AddCredentialRequest {
  /** OAuth 凭据必填；API Key 凭据不需要（后端已是 Option<String>） */
  refreshToken?: string
  authMethod?: 'social' | 'idc' | 'external_idp' | 'api_key'
  /** Kiro API Key（authMethod 为 api_key 时必填，格式 ksk_xxx） */
  kiroApiKey?: string
  clientId?: string
  clientSecret?: string
  priority?: number
  authRegion?: string
  apiRegion?: string
  machineId?: string
  email?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  // 企业 SSO (external_idp)
  provider?: string
  tokenEndpoint?: string
  issuerUrl?: string
  scopes?: string[]
  profileArn?: string
}

// 添加凭据响应
export interface AddCredentialResponse {
  success: boolean
  message: string
  credentialId: number
  email?: string
}

// 请求明细响应
export interface RequestDetailsResponse {
  total: number
  records: RequestDetailItem[]
}

// 单次请求明细
export interface RequestDetailItem {
  recordedAt: string
  requestId: string
  endpoint: string
  model: string
  credentialId: number
  stream: boolean
  cacheHit: boolean
  inputTokens: number
  cachedTokens: number
  outputTokens: number
  cacheRatio: number
  costUsd: number
  creditsUsed: number
  specialSettings: string[]
}

// ============ 用量统计 ============

export type StatsRange = '24h' | '7d' | '30d' | 'all'
export type StatsGranularity = 'hour' | 'day'

export interface StatsTimeFilter {
  range?: StatsRange
  startDate?: string
  endDate?: string
  granularity: StatsGranularity
}

export interface OverviewStats {
  todayCalls: number
  todayInputTokens: number
  todayOutputTokens: number
  todayErrors: number
  todayCredits: number
  weekCalls: number
  weekInputTokens: number
  weekOutputTokens: number
  weekCredits: number
  activeClientKeys: number
  activeCredentials: number
  totalCredentials?: number
}

export interface TimeSeriesPoint {
  ts: string
  inputTokens: number
  outputTokens: number
  cacheCreationTokens: number
  cacheReadTokens: number
  calls: number
  errors: number
  credits: number
}

export interface ModelDistribution {
  model: string
  calls: number
  inputTokens: number
  outputTokens: number
}

export interface CredentialDistribution {
  credentialId: number
  email?: string
  calls: number
  inputTokens: number
  outputTokens: number
  errors: number
}
