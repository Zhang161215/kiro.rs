import { useState, useRef } from 'react'
import { toast } from 'sonner'
import { CheckCircle2, XCircle, AlertCircle, Loader2, FolderOpen } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { useCredentials, useAddCredential, useDeleteCredential } from '@/hooks/use-credentials'
import { getCredentialBalance, setCredentialDisabled } from '@/api/credentials'
import { extractErrorMessage, sha256Hex } from '@/lib/utils'

interface BatchImportDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

interface CredentialInput {
  refreshToken: string
  authMethod?: string
  clientId?: string
  clientSecret?: string
  region?: string
  authRegion?: string
  apiRegion?: string
  priority?: number
  machineId?: string
  email?: string
  // 企业 SSO (external_idp)
  provider?: string
  tokenEndpoint?: string
  issuerUrl?: string
  scopes?: string[]
  profileArn?: string
}

// scopes 兼容 string(空格/逗号分隔) 或 string[]
function normalizeScopes(v: unknown): string[] | undefined {
  if (Array.isArray(v)) {
    const arr = v.filter((s): s is string => typeof s === 'string' && s.trim().length > 0)
    return arr.length > 0 ? arr : undefined
  }
  if (typeof v === 'string') {
    const arr = v.split(/[\s,]+/).map((s) => s.trim()).filter(Boolean)
    return arr.length > 0 ? arr : undefined
  }
  return undefined
}

const str = (v: unknown): string | undefined =>
  typeof v === 'string' && v.trim() ? v.trim() : undefined

// 从一个对象提取凭据字段（兼容 Kiro 原生 kiro-auth-token.json 格式）
function extractCred(obj: Record<string, unknown>): CredentialInput | null {
  const refreshToken = str(obj.refreshToken)
  if (!refreshToken) return null
  return {
    refreshToken,
    authMethod: str(obj.authMethod),
    clientId: str(obj.clientId),
    clientSecret: str(obj.clientSecret),
    region: str(obj.region),
    authRegion: str(obj.authRegion),
    apiRegion: str(obj.apiRegion),
    machineId: str(obj.machineId),
    email: str(obj.email),
    priority: typeof obj.priority === 'number' ? obj.priority : undefined,
    provider: str(obj.provider),
    tokenEndpoint: str(obj.tokenEndpoint),
    issuerUrl: str(obj.issuerUrl),
    scopes: normalizeScopes(obj.scopes),
    profileArn: str(obj.profileArn),
  }
}

// 从任意 JSON 中提取凭据列表，兼容多种格式
function parseCredentials(raw: string): CredentialInput[] {
  const parsed = JSON.parse(raw)
  const items: unknown[] = Array.isArray(parsed) ? parsed : parsed?.accounts ? parsed.accounts : [parsed]

  const results: CredentialInput[] = []
  for (const item of items) {
    if (typeof item !== 'object' || item === null) continue
    const obj = item as Record<string, unknown>

    // 顶层平铺格式（KAM 新版 / Kiro 原生 kiro-auth-token.json / 标准数组）
    const top = extractCred(obj)
    if (top) {
      results.push(top)
      continue
    }

    // 嵌套格式: credentials.refreshToken（KAM 旧版）
    const cred = obj.credentials as Record<string, unknown> | undefined
    if (cred && typeof cred === 'object') {
      const nested = extractCred(cred)
      if (nested) {
        // 顶层的 machineId/email 作为补充
        nested.machineId = nested.machineId ?? str(obj.machineId)
        nested.email = nested.email ?? str(obj.email)
        results.push(nested)
      }
    }
  }
  return results
}

interface VerificationResult {
  index: number
  status: 'pending' | 'checking' | 'verifying' | 'verified' | 'duplicate' | 'failed'
  error?: string
  usage?: string
  email?: string
  credentialId?: number
  rollbackStatus?: 'success' | 'failed' | 'skipped'
  rollbackError?: string
}

export function BatchImportDialog({ open, onOpenChange }: BatchImportDialogProps) {
  const [jsonInput, setJsonInput] = useState('')
  const [importing, setImporting] = useState(false)
  const [progress, setProgress] = useState({ current: 0, total: 0 })
  const [currentProcessing, setCurrentProcessing] = useState<string>('')
  const [results, setResults] = useState<VerificationResult[]>([])
  const [loadedFiles, setLoadedFiles] = useState<string[]>([])
  const fileInputRef = useRef<HTMLInputElement>(null)

  const { data: existingCredentials } = useCredentials()
  const { mutateAsync: addCredential } = useAddCredential()
  const { mutateAsync: deleteCredential } = useDeleteCredential()

  const rollbackCredential = async (id: number): Promise<{ success: boolean; error?: string }> => {
    try { await setCredentialDisabled(id, true) } catch (error) {
      return { success: false, error: `禁用失败: ${extractErrorMessage(error)}` }
    }
    try { await deleteCredential(id); return { success: true } } catch (error) {
      return { success: false, error: `删除失败: ${extractErrorMessage(error)}` }
    }
  }

  const resetForm = () => {
    setJsonInput('')
    setProgress({ current: 0, total: 0 })
    setCurrentProcessing('')
    setResults([])
    setLoadedFiles([])
  }

  // 文件选择处理
  const handleFileSelect = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const files = e.target.files
    if (!files || files.length === 0) return

    const allCredentials: CredentialInput[] = []
    const fileNames: string[] = []

    for (const file of Array.from(files)) {
      try {
        const text = await file.text()
        const creds = parseCredentials(text)
        allCredentials.push(...creds)
        fileNames.push(`${file.name} (${creds.length} 条)`)
      } catch (error) {
        toast.error(`解析 ${file.name} 失败: ${extractErrorMessage(error)}`)
      }
    }

    if (allCredentials.length > 0) {
      setJsonInput(JSON.stringify(allCredentials, null, 2))
      setLoadedFiles(fileNames)
      toast.success(`已加载 ${allCredentials.length} 条凭据（来自 ${fileNames.length} 个文件）`)
    }

    // 重置 input 以便重复选择同一文件
    e.target.value = ''
  }

  const handleBatchImport = async () => {
    let credentials: CredentialInput[]
    try {
      credentials = parseCredentials(jsonInput)
    } catch (error) {
      toast.error('JSON 格式错误: ' + extractErrorMessage(error))
      return
    }
    if (credentials.length === 0) { toast.error('没有可导入的凭据'); return }

    try {
      setImporting(true)
      setProgress({ current: 0, total: credentials.length })
      const initialResults: VerificationResult[] = credentials.map((c, i) => ({
        index: i + 1, status: 'pending', email: c.email,
      }))
      setResults(initialResults)

      const existingTokenHashes = new Set(
        existingCredentials?.credentials.map(c => c.refreshTokenHash).filter((h): h is string => Boolean(h)) || []
      )

      let successCount = 0, duplicateCount = 0, failCount = 0

      for (let i = 0; i < credentials.length; i++) {
        const cred = credentials[i]
        const token = cred.refreshToken.trim()
        const tokenHash = await sha256Hex(token)

        setCurrentProcessing(`正在处理凭据 ${i + 1}/${credentials.length}`)
        setResults(prev => { const n = [...prev]; n[i] = { ...n[i], status: 'checking' }; return n })

        if (existingTokenHashes.has(tokenHash)) {
          duplicateCount++
          const existing = existingCredentials?.credentials.find(c => c.refreshTokenHash === tokenHash)
          setResults(prev => { const n = [...prev]; n[i] = { ...n[i], status: 'duplicate', error: '该凭据已存在', email: existing?.email || cred.email }; return n })
          setProgress({ current: i + 1, total: credentials.length })
          continue
        }

        setResults(prev => { const n = [...prev]; n[i] = { ...n[i], status: 'verifying' }; return n })
        let addedCredId: number | null = null

        try {
          const clientId = cred.clientId?.trim() || undefined
          const clientSecret = cred.clientSecret?.trim() || undefined
          // 优先使用原生 authMethod（含 external_idp）；缺省再按 clientSecret 推断
          const authMethod =
            cred.authMethod?.trim() ||
            (clientId && clientSecret ? 'idc' : 'social')

          const addedCred = await addCredential({
            refreshToken: token,
            authMethod: authMethod as 'social' | 'idc' | 'external_idp',
            authRegion: cred.authRegion?.trim() || cred.region?.trim() || undefined,
            apiRegion: cred.apiRegion?.trim() || undefined,
            clientId, clientSecret,
            priority: cred.priority || 0,
            machineId: cred.machineId?.trim() || undefined,
            email: cred.email?.trim() || undefined,
            // 企业 SSO (external_idp) 字段
            provider: cred.provider,
            tokenEndpoint: cred.tokenEndpoint,
            issuerUrl: cred.issuerUrl,
            scopes: cred.scopes,
            profileArn: cred.profileArn,
          })
          addedCredId = addedCred.credentialId
          await new Promise(resolve => setTimeout(resolve, 1000))
          const balance = await getCredentialBalance(addedCred.credentialId)

          successCount++
          existingTokenHashes.add(tokenHash)
          setResults(prev => { const n = [...prev]; n[i] = { ...n[i], status: 'verified', usage: `${balance.currentUsage}/${balance.usageLimit}`, email: addedCred.email || cred.email, credentialId: addedCred.credentialId }; return n })
        } catch (error) {
          let rollbackStatus: VerificationResult['rollbackStatus'] = 'skipped'
          let rollbackError: string | undefined
          if (addedCredId) {
            const r = await rollbackCredential(addedCredId)
            rollbackStatus = r.success ? 'success' : 'failed'
            rollbackError = r.error
          }
          failCount++
          setResults(prev => { const n = [...prev]; n[i] = { ...n[i], status: 'failed', error: extractErrorMessage(error), rollbackStatus, rollbackError }; return n })
        }
        setProgress({ current: i + 1, total: credentials.length })
      }

      if (failCount === 0 && duplicateCount === 0) {
        toast.success(`成功导入并验活 ${successCount} 个凭据`)
      } else {
        toast.info(`验活完成：成功 ${successCount}，重复 ${duplicateCount}，失败 ${failCount}`)
      }
    } catch (error) {
      toast.error('导入失败: ' + extractErrorMessage(error))
    } finally {
      setImporting(false)
    }
  }
  const getStatusIcon = (status: VerificationResult['status']) => {
    switch (status) {
      case 'pending': return <div className="w-5 h-5 rounded-full border-2 border-gray-300" />
      case 'checking': case 'verifying': return <Loader2 className="w-5 h-5 animate-spin text-blue-500" />
      case 'verified': return <CheckCircle2 className="w-5 h-5 text-green-500" />
      case 'duplicate': return <AlertCircle className="w-5 h-5 text-yellow-500" />
      case 'failed': return <XCircle className="w-5 h-5 text-red-500" />
    }
  }

  const getStatusText = (result: VerificationResult) => {
    switch (result.status) {
      case 'pending': return '等待中'
      case 'checking': return '检查重复...'
      case 'verifying': return '验活中...'
      case 'verified': return '验活成功'
      case 'duplicate': return '重复凭据'
      case 'failed':
        if (result.rollbackStatus === 'success') return '验活失败（已排除）'
        if (result.rollbackStatus === 'failed') return '验活失败（未排除）'
        return '验活失败（未创建）'
    }
  }

  return (
    <Dialog open={open} onOpenChange={(v) => { if (!v && !importing) resetForm(); onOpenChange(v) }}>
      <DialogContent className="sm:max-w-2xl max-h-[80vh] flex flex-col">
        <DialogHeader>
          <DialogTitle>批量导入凭据（自动验活）</DialogTitle>
        </DialogHeader>
        <div className="flex-1 overflow-y-auto space-y-4 py-4">
          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <label className="text-sm font-medium">JSON 格式凭据</label>
              <div>
                <input ref={fileInputRef} type="file" accept=".json" multiple className="hidden" onChange={handleFileSelect} />
                <Button size="sm" variant="outline" onClick={() => fileInputRef.current?.click()} disabled={importing}>
                  <FolderOpen className="h-4 w-4 mr-1" /> 选择文件导入
                </Button>
              </div>
            </div>
            {loadedFiles.length > 0 && (
              <div className="text-xs text-muted-foreground bg-muted/50 rounded px-2 py-1.5">
                已加载：{loadedFiles.join('、')}
              </div>
            )}
            <textarea
              placeholder={'粘贴 JSON 或点击「选择文件导入」加载 .json 文件\n支持 Kiro 原生 kiro-auth-token.json（含企业 SSO）、KAM 导出、单对象、数组等格式'}
              value={jsonInput}
              onChange={(e) => { setJsonInput(e.target.value); setLoadedFiles([]) }}
              disabled={importing}
              className="flex min-h-[200px] w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50 font-mono"
            />
            <p className="text-xs text-muted-foreground">支持多文件选择，自动合并。兼容 Kiro 原生 kiro-auth-token.json（含 external_idp 企业 SSO）、KAM 导出、标准凭据数组等</p>
          </div>

          {(importing || results.length > 0) && (
            <>
              <div className="space-y-2">
                <div className="flex justify-between text-sm">
                  <span>{importing ? '验活进度' : '验活完成'}</span>
                  <span>{progress.current} / {progress.total}</span>
                </div>
                <div className="w-full bg-secondary rounded-full h-2">
                  <div className="bg-primary h-2 rounded-full transition-all" style={{ width: `${progress.total > 0 ? (progress.current / progress.total) * 100 : 0}%` }} />
                </div>
                {importing && currentProcessing && <div className="text-xs text-muted-foreground">{currentProcessing}</div>}
              </div>
              <div className="flex gap-4 text-sm">
                <span className="text-green-600 dark:text-green-400">✓ 成功: {results.filter(r => r.status === 'verified').length}</span>
                <span className="text-yellow-600 dark:text-yellow-400">⚠ 重复: {results.filter(r => r.status === 'duplicate').length}</span>
                <span className="text-red-600 dark:text-red-400">✗ 失败: {results.filter(r => r.status === 'failed').length}</span>
              </div>
              <div className="border rounded-md divide-y max-h-[300px] overflow-y-auto">
                {results.map((result) => (
                  <div key={result.index} className="p-3">
                    <div className="flex items-start gap-3">
                      {getStatusIcon(result.status)}
                      <div className="flex-1 min-w-0">
                        <div className="flex items-center gap-2">
                          <span className="text-sm font-medium">{result.email || `凭据 #${result.index}`}</span>
                          <span className="text-xs text-muted-foreground">{getStatusText(result)}</span>
                        </div>
                        {result.usage && <div className="text-xs text-muted-foreground mt-1">用量: {result.usage}</div>}
                        {result.error && <div className="text-xs text-red-600 dark:text-red-400 mt-1">{result.error}</div>}
                        {result.rollbackError && <div className="text-xs text-red-600 dark:text-red-400 mt-1">回滚失败: {result.rollbackError}</div>}
                      </div>
                    </div>
                  </div>
                ))}
              </div>
            </>
          )}
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={() => { onOpenChange(false); resetForm() }} disabled={importing}>
            {importing ? '验活中...' : results.length > 0 ? '关闭' : '取消'}
          </Button>
          {results.length === 0 && (
            <Button onClick={handleBatchImport} disabled={importing || !jsonInput.trim()}>开始导入并验活</Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
