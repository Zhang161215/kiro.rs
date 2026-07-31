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
export interface UpdateCredentialRequest {
  email?: string | null
  authRegion?: string | null
  apiRegion?: string | null
  proxyUrl?: string | null
  proxyUsername?: string | null
  proxyPassword?: string | null
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
